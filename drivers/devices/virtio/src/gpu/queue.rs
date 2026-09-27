// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GPU's one-request, two-direct-descriptor split queue.
//!
//! The locked queue helper cannot reject failed Hal::share or cancel mappings.
//! This small queue owns both streaming maps explicitly. It never uses indirect
//! descriptors; all fallible mapping and bulk copy work precedes publication.
use alloc::{vec, vec::Vec};
use core::{marker::PhantomData, ptr::NonNull};

use virtio_drivers::{BufferDirection, Error, PAGE_SIZE, PhysAddr, Result, transport::Transport};

use super::GpuHal;

const DEPTH: u16 = 2;
const AVAIL_OFFSET: usize = 32;
const USED_OFFSET: usize = PAGE_SIZE;
const RING_PAGES: usize = 2;

/// Unpublished coherent ring allocation; released only after queue reset/unset.
struct RingMemory<H: GpuHal> {
    bus: PhysAddr,
    cpu: NonNull<u8>,
    _hal: PhantomData<fn() -> H>,
}
// SAFETY: Hal::dma_alloc transfers unique coherent allocation ownership. The
// pointer is never exposed as a reference; Queue accesses it only under &mut
// ownership or the engine lock. H is a static provider, not a stored instance.
unsafe impl<H: GpuHal> Send for RingMemory<H> {}
impl<H: GpuHal> RingMemory<H> {
    fn new() -> Result<Self> {
        let (bus, cpu) = H::dma_alloc(RING_PAGES, BufferDirection::Both, false);
        if bus == 0 {
            return Err(Error::DmaError);
        }
        Ok(Self {
            bus,
            cpu,
            _hal: PhantomData,
        })
    }
}
impl<H: GpuHal> Drop for RingMemory<H> {
    fn drop(&mut self) {
        // SAFETY: owned allocation from dma_alloc, same pages and addresses;
        // Queue's owner quiesces/reset-acknowledges the device before dropping.
        let result = unsafe { H::dma_dealloc(self.bus, self.cpu, RING_PAGES, false) };
        assert_eq!(result, 0, "GPU coherent ring deallocation failed");
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

pub(super) struct Queue<H: GpuHal> {
    memory: RingMemory<H>,
    available: u16,
    used: u16,
    active: bool,
}
impl<H: GpuHal> Queue<H> {
    pub(super) fn new(transport: &mut impl Transport, index: u16) -> Result<Self> {
        if transport.queue_used(index) {
            return Err(Error::AlreadyUsed);
        }
        if transport.max_queue_size(index) < u32::from(DEPTH) {
            return Err(Error::InvalidParam);
        }
        let memory = RingMemory::<H>::new()?;
        // SAFETY: the allocation is exclusive/unpublished and covers two pages.
        // Flag 1 suppresses interrupts: completion is polled with task yielding.
        unsafe {
            memory
                .cpu
                .as_ptr()
                .add(AVAIL_OFFSET)
                .cast::<u16>()
                .write_volatile(1u16.to_le());
        }
        transport.queue_set(
            index,
            u32::from(DEPTH),
            memory.bus,
            memory.bus + AVAIL_OFFSET as u64,
            memory.bus + USED_OFFSET as u64,
        );
        Ok(Self {
            memory,
            available: 0,
            used: 0,
            active: false,
        })
    }

    /// Publishes only fully mapped transactions; descriptor zero is the sole head.
    pub(super) fn publish(&mut self, transaction: &mut Transaction<H>) -> Result<()> {
        if self.active || transaction.device_owned {
            return Err(Error::QueueFull);
        }
        let buffers = transaction.buffers.as_ref().ok_or(Error::InvalidParam)?;
        // SAFETY: both mappings succeeded and stay owned by Transaction until
        // completion/reset. Rings are coherent live DMA memory. Descriptor and
        // avail entry stores precede the release fence/index publication.
        unsafe {
            let descriptors = self.memory.cpu.as_ptr().cast::<Descriptor>();
            descriptors.write_volatile(Descriptor {
                address: buffers.request.bus.to_le(),
                length: (buffers.request.bytes.len() as u32).to_le(),
                flags: 1u16.to_le(),
                next: 1u16.to_le(),
            });
            descriptors.add(1).write_volatile(Descriptor {
                address: buffers.response.bus.to_le(),
                length: (buffers.response.bytes.len() as u32).to_le(),
                flags: 2u16.to_le(),
                next: 0,
            });
            let avail = self.memory.cpu.as_ptr().add(AVAIL_OFFSET).cast::<u16>();
            avail
                .add(2 + usize::from(self.available % DEPTH))
                .write_volatile(0);
        }
        H::dma_write_barrier();
        // No fallible work remains once ownership transfers to the device.
        transaction.device_owned = true;
        self.active = true;
        self.available = self.available.wrapping_add(1);
        // SAFETY: aligned avail.idx is within the live coherent ring. The first
        // device barrier ordered its descriptor and avail-slot stores before it.
        unsafe {
            self.memory
                .cpu
                .as_ptr()
                .add(AVAIL_OFFSET + 2)
                .cast::<u16>()
                .write_volatile(self.available.to_le());
        }
        // Both bootstrap and runtime notify only after publish returns. This
        // second boundary orders the index store before the MMIO doorbell.
        H::dma_write_barrier();
        Ok(())
    }

    /// Takes a completed chain, checking both used-index movement and full u32 ID.
    pub(super) fn pop(&mut self) -> Result<Option<u32>> {
        if !self.active {
            return Ok(None);
        }
        // SAFETY: ring allocation remains live, device owns used ring writes.
        // The volatile index is followed by the host DMA read barrier before
        // reading the corresponding used entry and mapped payload.
        let index = unsafe {
            u16::from_le(
                self.memory
                    .cpu
                    .as_ptr()
                    .add(USED_OFFSET + 2)
                    .cast::<u16>()
                    .read_volatile(),
            )
        };
        if index == self.used {
            return Ok(None);
        }
        if index.wrapping_sub(self.used) != 1 {
            return Err(Error::WrongToken);
        }
        H::dma_read_barrier();
        // SAFETY: exactly one chain was published and index advanced once. Entry
        // bounds are fixed by DEPTH; a malformed ID never retires ownership.
        let (token, length) = unsafe {
            let entry = self
                .memory
                .cpu
                .as_ptr()
                .add(USED_OFFSET + 4 + usize::from(self.used % DEPTH) * 8)
                .cast::<u32>();
            (
                u32::from_le(entry.read_volatile()),
                u32::from_le(entry.add(1).read_volatile()),
            )
        };
        if token != 0 {
            return Err(Error::WrongToken);
        }
        self.used = index;
        self.active = false;
        Ok(Some(length))
    }
}

struct Mapping<H: GpuHal> {
    bytes: Vec<u8>,
    bus: PhysAddr,
    direction: BufferDirection,
    _hal: PhantomData<fn() -> H>,
}
impl<H: GpuHal> Mapping<H> {
    fn new(mut bytes: Vec<u8>, direction: BufferDirection) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > u32::MAX as usize {
            return Err(Error::InvalidParam);
        }
        // SAFETY: owned vector keeps a stable exclusive allocation throughout
        // mapping. Its bytes are not accessed while device-owned. A zero result
        // is the integration HAL's failure sentinel and is never published.
        let bus = unsafe { H::share(NonNull::from(bytes.as_mut_slice()), direction, false) };
        if bus == 0 {
            return Err(Error::DmaError);
        }
        Ok(Self {
            bytes,
            bus,
            direction,
            _hal: PhantomData,
        })
    }

    fn unmap(&mut self) {
        if self.bus != 0 {
            // SAFETY: matching successful share, original vector/length/direction.
            // Caller guarantees unpublished, returned used chain, or reset ACK.
            unsafe {
                H::unshare(
                    self.bus,
                    NonNull::from(self.bytes.as_mut_slice()),
                    self.direction,
                    false,
                )
            };
            self.bus = 0;
        }
    }
}
impl<H: GpuHal> Drop for Mapping<H> {
    fn drop(&mut self) {
        self.unmap();
    }
}
struct Buffers<H: GpuHal> {
    request: Mapping<H>,
    response: Mapping<H>,
}

/// Owns mappings as well as original request/reply storage.
/// Dropping while device-owned quarantines rather than cancelling live DMA.
pub(super) struct Transaction<H: GpuHal> {
    buffers: Option<Buffers<H>>,
    device_owned: bool,
}
impl<H: GpuHal> Transaction<H> {
    /// All mapping happens before ring publication and outside engine locks.
    pub(super) fn prepare(request: Vec<u8>, response_size: usize) -> Result<Self> {
        let request = Mapping::<H>::new(request, BufferDirection::DriverToDevice)?;
        // Failure here drops/unmaps the successful first map, with no publish.
        let response = Mapping::<H>::new(vec![0; response_size], BufferDirection::DeviceToDriver)?;
        Ok(Self {
            buffers: Some(Buffers { request, response }),
            device_owned: false,
        })
    }

    /// Retire a returned chain and synchronize its response, outside spinlocks.
    ///
    /// # Safety
    /// Queue::pop must have returned this transaction's sole active chain.
    pub(super) unsafe fn finish(mut self, length: u32) -> Result<Vec<u8>> {
        self.device_owned = false;
        let mut buffers = self.buffers.take().unwrap();
        buffers.request.unmap();
        buffers.response.unmap();
        if length as usize > buffers.response.bytes.len() {
            return Err(Error::IoError);
        }
        let mut response = core::mem::take(&mut buffers.response.bytes);
        response.truncate(length as usize);
        Ok(response)
    }

    /// Cancel mappings only after acknowledged reset; discard any reply bytes.
    ///
    /// # Safety
    /// Device reset must have been acknowledged since publication. No used-ring
    /// event is fabricated and no copied-back response is reported as success.
    pub(super) unsafe fn cancel_after_reset(mut self) {
        self.device_owned = false;
    }
}
impl<H: GpuHal> Drop for Transaction<H> {
    fn drop(&mut self) {
        if self.device_owned
            && let Some(buffers) = self.buffers.take()
        {
            core::mem::forget(buffers);
        }
    }
}
