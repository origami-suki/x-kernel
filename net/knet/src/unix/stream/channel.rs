// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Connected Unix stream channel state.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use kerrno::LinuxError;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet};
use kspin::SpinNoPreempt;
use ksync::Mutex;
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Observer, Split},
};

use crate::{AncillaryData, options::UnixCredentials};

pub(super) const STREAM_BUF_BYTES: usize = 64 * 1024;
pub(super) const STREAM_WRITABLE_MAX_OCCUPIED_BYTES: usize = STREAM_BUF_BYTES / 4;

pub(super) fn is_stream_writable(occupied_bytes: usize) -> bool {
    occupied_bytes <= STREAM_WRITABLE_MAX_OCCUPIED_BYTES
}

fn new_ring_pair() -> (HeapProd<u8>, HeapCons<u8>) {
    let rb = HeapRb::new(STREAM_BUF_BYTES);
    rb.split()
}

#[derive(Default)]
pub(super) struct StreamPollSets {
    pub(super) readable: PollSet,
    pub(super) writable: PollSet,
    state: PollSet,
}

impl StreamPollSets {
    pub(super) fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        let has_read_events =
            events.intersects(IoEvents::IN | IoEvents::RDNORM | IoEvents::RDBAND | IoEvents::RDHUP);
        let has_write_events =
            events.intersects(IoEvents::OUT | IoEvents::WRNORM | IoEvents::WRBAND);
        if has_read_events {
            context.register(&self.readable)?;
        }
        if has_write_events {
            context.register(&self.writable)?;
        }
        if !has_read_events && !has_write_events && events.intersects(IoEvents::ERR | IoEvents::HUP)
        {
            context.register(&self.state)?;
        }
        Ok(())
    }

    pub(super) fn wake_state_change(&self) {
        self.readable.wake();
        self.writable.wake();
        self.state.wake();
    }
}

#[derive(Default)]
pub(super) struct StreamEndpoint {
    pub(super) polls: StreamPollSets,
    /// Independent receive option, observed by either endpoint at send time.
    pub(super) has_passcred: AtomicBool,
    /// Orders data publication, shutdown, and EOF observation for this
    /// endpoint's transmit direction.
    pub(super) tx_order: SpinNoPreempt<()>,
    pub(super) rx_closed: AtomicBool,
    pub(super) tx_closed: AtomicBool,
    pub(super) socket_error: AtomicI32,
}

/// Control data is attached to a published byte interval, not to a recv call.
/// Positions wrap together; each pending interval is bounded by the byte ring.
/// The mutex serializes metadata with ring publication, but is never held over
/// user-memory copies. Both endpoints acquire it before the producer tx_order.
#[derive(Default)]
pub(super) struct StreamControl {
    pub(super) written: usize,
    pub(super) read: usize,
    pub(super) pending: VecDeque<ControlRecord>,
}

pub(super) struct ControlRecord {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) data: Vec<AncillaryData>,
    pub(super) credentials: Option<UnixCredentials>,
}

pub(super) fn new_duplex_channel(
    client_endpoint: Arc<StreamEndpoint>,
    server_endpoint: Arc<StreamEndpoint>,
    pid: u32,
) -> (Channel, Channel) {
    let (client_tx, server_rx) = new_ring_pair();
    let (server_tx, client_rx) = new_ring_pair();
    let client_control = Arc::new(Mutex::new(StreamControl::default()));
    let server_control = Arc::new(Mutex::new(StreamControl::default()));
    (
        Channel {
            tx: client_tx,
            rx: client_rx,
            tx_control: client_control.clone(),
            rx_control: server_control.clone(),
            endpoint: client_endpoint.clone(),
            peer_endpoint: server_endpoint.clone(),
            peer_pid: pid,
        },
        Channel {
            tx: server_tx,
            rx: server_rx,
            tx_control: server_control,
            rx_control: client_control,
            endpoint: server_endpoint,
            peer_endpoint: client_endpoint,
            peer_pid: pid,
        },
    )
}

pub(super) struct Channel {
    pub(super) tx: HeapProd<u8>,
    pub(super) rx: HeapCons<u8>,
    pub(super) tx_control: Arc<Mutex<StreamControl>>,
    pub(super) rx_control: Arc<Mutex<StreamControl>>,
    pub(super) endpoint: Arc<StreamEndpoint>,
    pub(super) peer_endpoint: Arc<StreamEndpoint>,
    pub(super) peer_pid: u32,
}

impl Drop for Channel {
    fn drop(&mut self) {
        let mut control = self.rx_control.lock();
        let (is_rx_changed, has_unread_input) = {
            let _tx_order = self.peer_endpoint.tx_order.lock();
            (
                !self.endpoint.rx_closed.swap(true, Ordering::AcqRel),
                self.rx.occupied_len() > 0,
            )
        };
        // Closing a receiver releases queued file references even while the
        // sender still owns its half. Drop them outside the metadata/spin locks.
        let discarded = core::mem::take(&mut control.pending);
        drop(control);
        drop(discarded);
        let is_tx_changed = {
            let _tx_order = self.endpoint.tx_order.lock();
            !self.endpoint.tx_closed.swap(true, Ordering::AcqRel)
        };
        if has_unread_input {
            self.peer_endpoint
                .socket_error
                .store(LinuxError::ECONNRESET.into_raw(), Ordering::Release);
        }
        if is_rx_changed || is_tx_changed || has_unread_input {
            self.endpoint.polls.wake_state_change();
            self.peer_endpoint.polls.wake_state_change();
        }
    }
}
