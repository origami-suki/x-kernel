// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix datagram socket transport.
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use async_channel::TryRecvError;
use async_trait::async_trait;
use kerrno::{KError, KResult};
use kio::{IoBufMut, Read, Write};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::{Mutex, RwLock};

use crate::{
    AncillaryData, ConnectOptions, KernelAncillaryData, RecvFlags, RecvOptions, SendOptions,
    SocketAddrEx,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption, UnixCredentials},
    unix::{UnixAddr, UnixTransport, UnixTransportOps, lookup_bind_entry},
};

struct ReceiveQueue {
    receiver: async_channel::Receiver<Datagram>,
    poll: Arc<PollSet>,
    peeked: Option<Datagram>,
}

struct Datagram {
    data: Vec<u8>,
    ancillary: Vec<AncillaryData>,
    sender: UnixAddr,
    credentials: Option<UnixCredentials>,
}

struct Channel {
    tx: async_channel::Sender<Datagram>,
    poll: Arc<PollSet>,
    has_passcred: Arc<AtomicBool>,
}

pub struct Bind {
    tx: async_channel::Sender<Datagram>,
    poll: Arc<PollSet>,
    has_passcred: Arc<AtomicBool>,
}
impl Bind {
    fn connect(&self) -> Channel {
        let tx = self.tx.clone();
        Channel {
            tx,
            poll: self.poll.clone(),
            has_passcred: self.has_passcred.clone(),
        }
    }
}

pub struct DgramTransport {
    rx: Mutex<Option<ReceiveQueue>>,
    has_passcred: Arc<AtomicBool>,
    peer: RwLock<Option<Channel>>,
    local_addr: RwLock<UnixAddr>,
    bind_slot: Mutex<Option<Arc<Mutex<Option<Bind>>>>>,
    poll_state: Arc<PollSet>,
    options: GeneralOptions,
    pid: u32,
}
impl DgramTransport {
    pub fn new(pid: u32) -> Self {
        DgramTransport {
            rx: Mutex::new(None),
            has_passcred: Arc::default(),
            peer: RwLock::new(None),
            local_addr: RwLock::new(UnixAddr::Unbound),
            bind_slot: Mutex::new(None),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
            pid,
        }
    }

    fn new_connected(
        rx: (async_channel::Receiver<Datagram>, Arc<PollSet>),
        peer: Channel,
        pid: u32,
        has_passcred: Arc<AtomicBool>,
    ) -> Self {
        DgramTransport {
            rx: Mutex::new(Some(ReceiveQueue {
                receiver: rx.0,
                poll: rx.1,
                peeked: None,
            })),
            has_passcred,
            peer: RwLock::new(Some(peer)),
            local_addr: RwLock::new(UnixAddr::Unbound),
            bind_slot: Mutex::new(None),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
            pid,
        }
    }

    pub fn new_pair(pid: u32) -> (Self, Self) {
        let (tx1, rx1) = async_channel::unbounded();
        let (tx2, rx2) = async_channel::unbounded();
        let poll1 = Arc::new(PollSet::new());
        let poll2 = Arc::new(PollSet::new());
        let passcred1 = Arc::new(AtomicBool::new(false));
        let passcred2 = Arc::new(AtomicBool::new(false));
        let transport1 = DgramTransport::new_connected(
            (rx1, poll1.clone()),
            Channel {
                tx: tx2,
                poll: poll2.clone(),
                has_passcred: passcred2.clone(),
            },
            pid,
            passcred1.clone(),
        );
        let transport2 = DgramTransport::new_connected(
            (rx2, poll2.clone()),
            Channel {
                tx: tx1,
                poll: poll1.clone(),
                has_passcred: passcred1.clone(),
            },
            pid,
            passcred2,
        );
        (transport1, transport2)
    }
}

impl Configurable for DgramTransport {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        use GetSocketOption as O;

        if self.options.get_option_inner(opt)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match opt {
            O::PassCredentials(value) => {
                **value = self.has_passcred.load(Ordering::Relaxed);
            }
            O::PeerCredentials(cred) => {
                // Datagram sockets are stateless and do not have a peer, so we
                // return the credentials of the process that created the
                // socket.
                **cred = UnixCredentials::new(self.pid);
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<OptionHandled> {
        use SetSocketOption as O;

        if self.options.set_option_inner(opt)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match opt {
            O::PassCredentials(value) => {
                self.has_passcred.store(*value, Ordering::Relaxed);
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }
}
#[async_trait]
impl UnixTransportOps for DgramTransport {
    fn bind(&self, slot: &super::BindEntry, local_addr: &UnixAddr) -> KResult {
        let bind_slot_handle = slot.dgram.clone();
        let mut slot = slot.dgram.lock();
        if slot.is_some() {
            return Err(KError::AddrInUse);
        }
        let mut guard = self.rx.lock();
        if guard.is_some() {
            return Err(KError::InvalidInput);
        }
        let (tx, rx) = async_channel::unbounded();
        let poll = Arc::new(PollSet::new());
        *slot = Some(Bind {
            tx,
            poll: poll.clone(),
            has_passcred: self.has_passcred.clone(),
        });
        drop(slot);
        *guard = Some(ReceiveQueue {
            receiver: rx,
            poll,
            peeked: None,
        });
        self.local_addr.write().clone_from(local_addr);
        *self.bind_slot.lock() = Some(bind_slot_handle);
        self.poll_state.wake();
        Ok(())
    }

    fn connect(
        &self,
        slot: &super::BindEntry,
        _local_addr: &UnixAddr,
        _options: ConnectOptions,
    ) -> KResult {
        let mut guard = self.peer.write();
        if guard.is_some() {
            return Err(KError::AlreadyConnected);
        }
        *guard = Some(
            slot.dgram
                .lock()
                .as_ref()
                .ok_or(KError::NotConnected)?
                .connect(),
        );
        self.poll_state.wake();
        Ok(())
    }

    async fn accept(&self, _nonblocking: bool) -> KResult<(UnixTransport, UnixAddr)> {
        Err(KError::InvalidInput)
    }

    fn send(&self, mut src: impl Read, options: SendOptions) -> KResult<usize> {
        let mut message = Vec::new();
        src.read_to_end(&mut message)?;
        let len = message.len();
        let send_packet =
            |tx: &async_channel::Sender<Datagram>, poll: &PollSet, peer_passcred: &AtomicBool| {
                let credentials = options.credentials.and_then(|value| {
                    value.for_passcred(
                        self.has_passcred.load(Ordering::Relaxed)
                            || peer_passcred.load(Ordering::Relaxed),
                    )
                });
                let packet = Datagram {
                    data: message,
                    ancillary: options.ancillary,
                    sender: self.local_addr.read().clone(),
                    credentials,
                };
                tx.try_send(packet).map_err(|_| KError::BrokenPipe)?;
                poll.wake();
                Ok(())
            };

        let connected = self.peer.read();
        if let Some(addr) = options.to {
            let addr = addr.into_unix()?;
            let cred = kprocess::current_cred();
            lookup_bind_entry(&addr, &cred, |slot| {
                if let Some(bind) = slot.dgram.lock().as_ref() {
                    send_packet(&bind.tx, &bind.poll, &bind.has_passcred)
                } else {
                    Err(KError::NotConnected)
                }
            })?;
        } else if let Some(chan) = connected.as_ref() {
            send_packet(&chan.tx, &chan.poll, &chan.has_passcred)?;
        } else {
            return Err(KError::NotConnected);
        }
        Ok(len)
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, mut options: RecvOptions) -> KResult<usize> {
        let is_peek = options.flags.contains(RecvFlags::PEEK);
        self.options
            .recv_poller_with_nonblocking(self, options.flags.nonblocking(), || {
                let mut guard = self.rx.lock();
                let Some(queue) = guard.as_mut() else {
                    return Err(KError::NotConnected);
                };
                if queue.peeked.is_none() {
                    queue.peeked = Some(match queue.receiver.try_recv() {
                        Ok(packet) => packet,
                        Err(TryRecvError::Empty) => return Err(KError::WouldBlock),
                        Err(TryRecvError::Closed) => return Ok(0),
                    });
                }
                let packet = queue.peeked.as_ref().expect("packet staged");
                let count = dst.write(&packet.data)?;
                let length = packet.data.len();
                if count < length
                    && let Some(flags) = options.out_flags.as_mut()
                {
                    **flags |= RecvFlags::TRUNCATE;
                }
                if let Some(from) = options.from.as_mut() {
                    **from = SocketAddrEx::Unix(packet.sender.clone());
                }
                if let Some(dst) = options.ancillary.as_mut() {
                    if self.has_passcred.load(Ordering::Relaxed) {
                        dst.push(Arc::new(KernelAncillaryData::Credentials(
                            packet
                                .credentials
                                .unwrap_or_else(UnixCredentials::unavailable),
                        )));
                    }
                    dst.extend(packet.ancillary.iter().cloned());
                }
                let consumed = if is_peek { None } else { queue.peeked.take() };
                drop(guard);
                // Last file references can run close/wakeup logic. Do not drop them
                // while holding the receive queue lock.
                drop(consumed);
                Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                    length
                } else {
                    count
                })
            })
    }
}

impl Pollable for DgramTransport {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT;
        if let Some(queue) = self.rx.lock().as_ref() {
            events.set(
                IoEvents::IN,
                queue.peeked.is_some() || !queue.receiver.is_empty(),
            );
        }
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Some(queue) = self.rx.lock().as_ref()
            && events.contains(IoEvents::IN)
        {
            context.register(&queue.poll)?;
        }
        Ok(())
    }
}

impl Drop for DgramTransport {
    fn drop(&mut self) {
        if let Some(slot) = self.bind_slot.lock().take() {
            *slot.lock() = None;
        }
        if let Some(chan) = self.peer.write().take() {
            chan.poll.wake();
        }
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert_eq, def_test};

    use super::*;

    struct DropCount(Arc<AtomicUsize>);
    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[def_test]
    fn unix_datagram_peek_preserves_readiness_and_queued_ownership() {
        let (left, right) = DgramTransport::new_pair(1);
        let drops = Arc::new(AtomicUsize::new(0));
        left.send(
            &b"x"[..],
            SendOptions {
                ancillary: vec![Arc::new(DropCount(drops.clone()))],
                ..SendOptions::default()
            },
        )
        .unwrap();
        let mut bytes = [0];
        let mut control = Vec::new();
        assert_eq!(
            right.recv(
                &mut bytes[..],
                RecvOptions {
                    flags: RecvFlags::PEEK,
                    ancillary: Some(&mut control),
                    ..RecvOptions::default()
                }
            ),
            Ok(1)
        );
        drop(control);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(right.poll().contains(IoEvents::IN), true);
        assert_eq!(right.recv(&mut bytes[..], RecvOptions::default()), Ok(1));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(right.poll().contains(IoEvents::IN), false);
        assert_eq!(
            right.recv(
                &mut bytes[..],
                RecvOptions {
                    flags: RecvFlags::DONT_WAIT,
                    ..RecvOptions::default()
                }
            ),
            Err(KError::WouldBlock)
        );
    }
}
