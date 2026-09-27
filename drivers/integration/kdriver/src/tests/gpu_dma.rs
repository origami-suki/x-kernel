// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The real shared bounce pool must fail before GPU publication and be reusable
//! after timeout/reset. Counters wrap real Hal methods, not identity mappings.
use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use display::{GpuDeferredWork, GpuExecution, GpuWait};
use driver_base::{DriverError, DriverResult};
use unittest::{assert, assert_eq, def_test};
use virtio::{BufferDirection, GpuDmaTestDevice, PhysAddr, VirtIoHal};

use crate::driver_registry::virtio::VirtIoHalImpl;

static MAPS: AtomicUsize = AtomicUsize::new(0);
static UNMAPS: AtomicUsize = AtomicUsize::new(0);
struct CountingHal;
// SAFETY: delegates all allocation/mapping obligations to the actual kernel
// integration HAL; counters do not alter ownership, pointers or directions.
unsafe impl VirtIoHal for CountingHal {
    fn dma_alloc(n: usize, d: BufferDirection, a: bool) -> (PhysAddr, NonNull<u8>) {
        VirtIoHalImpl::dma_alloc(n, d, a)
    }

    unsafe fn dma_dealloc(p: PhysAddr, v: NonNull<u8>, n: usize, a: bool) -> i32 {
        // SAFETY: exact matching allocation from the real HAL.
        unsafe { VirtIoHalImpl::dma_dealloc(p, v, n, a) }
    }

    unsafe fn mmio_phys_to_virt(p: PhysAddr, n: usize) -> NonNull<u8> {
        // SAFETY: caller supplies a valid MMIO physical range.
        unsafe { VirtIoHalImpl::mmio_phys_to_virt(p, n) }
    }

    unsafe fn share(b: NonNull<[u8]>, d: BufferDirection, a: bool) -> PhysAddr {
        // SAFETY: caller preserves exclusive buffer ownership through unshare.
        let p = unsafe { VirtIoHalImpl::share(b, d, a) };
        if p != 0 {
            MAPS.fetch_add(1, Ordering::Relaxed);
        }
        p
    }

    unsafe fn unshare(p: PhysAddr, b: NonNull<[u8]>, d: BufferDirection, a: bool) {
        core::assert_ne!(p, 0);
        UNMAPS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: passed unchanged from the successful share call.
        unsafe { VirtIoHalImpl::unshare(p, b, d, a) }
    }
}
// SAFETY: this wrapper uses the production host HAL's device-domain barriers.
unsafe impl virtio::VirtIoGpuHal for CountingHal {
    fn dma_write_barrier() {
        <VirtIoHalImpl as virtio::VirtIoGpuHal>::dma_write_barrier();
    }

    fn dma_read_barrier() {
        <VirtIoHalImpl as virtio::VirtIoGpuHal>::dma_read_barrier();
    }
}

fn cpu_address(pa: PhysAddr) -> usize {
    khal::mem::p2v((pa as usize).into()).as_usize()
}
struct FastDeadline(Arc<dyn GpuExecution>);
struct FastWait<'a> {
    inner: &'a mut dyn GpuWait,
    now: u64,
}
impl GpuWait for FastWait<'_> {
    fn now_ns(&self) -> u64 {
        self.now
    }

    fn yield_execution(&mut self) {
        self.now += 15_000_000_000;
        self.inner.yield_execution();
    }
}
impl GpuExecution for FastDeadline {
    fn run(&self, op: &mut dyn FnMut(&mut dyn GpuWait) -> DriverResult) -> DriverResult {
        self.0.run(&mut |wait| {
            op(&mut FastWait {
                inner: wait,
                now: 0,
            })
        })
    }

    fn deferred(&self, callback: Box<dyn Fn() + Send + Sync>) -> Arc<dyn GpuDeferredWork> {
        self.0.deferred(callback)
    }
}
fn device() -> GpuDmaTestDevice<CountingHal> {
    // SAFETY: this AArch64 host HAL uses coherent physical/bounce addresses;
    // p2v resolves its live allocations. The fixture is the only fake device.
    unsafe { GpuDmaTestDevice::new(cpu_address, Arc::new(FastDeadline(super::new_execution()))) }
        .unwrap()
}

#[def_test(serial)]
fn gpu_real_hal_exhaustion_does_not_publish_and_all_capacity_is_restored() {
    let gpu = device();
    gpu.create_context(1).unwrap();
    let before = gpu.progress();
    let large = vec![0u8; 4 * 1024 * 1024];
    assert_eq!(gpu.submit(1, &large), Err(DriverError::NoMemory));
    assert_eq!(gpu.progress(), before);
    // Fill the actual shared pool, including its final small fragments. Every
    // successful map stays owned until the pressure phase ends.
    let mut pressure: Vec<(Vec<u8>, PhysAddr)> = Vec::new();
    for length in [64 * 1024, 4096, 64, 1] {
        for _ in 0..8192 {
            let mut bytes = vec![0u8; length];
            // SAFETY: bytes stays in pressure until exact matching unshare.
            let bus = unsafe {
                VirtIoHalImpl::share(
                    NonNull::from(bytes.as_mut_slice()),
                    BufferDirection::DriverToDevice,
                    false,
                )
            };
            if bus == 0 {
                break;
            }
            pressure.push((bytes, bus));
        }
    }
    core::assert!(!pressure.is_empty());
    let before = gpu.progress();
    let result = gpu.create_context(2);
    let after = gpu.progress();
    // Release pressure before assertions so a failed test cannot starve peers.
    for (mut bytes, bus) in pressure {
        // SAFETY: no device accessed these maps; exact original buffer retained.
        unsafe {
            VirtIoHalImpl::unshare(
                bus,
                NonNull::from(bytes.as_mut_slice()),
                BufferDirection::DriverToDevice,
                false,
            )
        };
    }
    assert_eq!(result, Err(DriverError::NoMemory));
    assert_eq!(after, before);
    gpu.create_context(2).unwrap();
    assert_eq!(MAPS.load(Ordering::Acquire), UNMAPS.load(Ordering::Acquire));
}

#[def_test(serial)]
fn gpu_real_hal_timeout_reset_balances_unshare_and_pool_remains_usable() {
    let before_maps = MAPS.load(Ordering::Acquire);
    let before_unmaps = UNMAPS.load(Ordering::Acquire);
    for _ in 0..8 {
        let gpu = device();
        gpu.delay_completion();
        assert_eq!(gpu.create_context(3), Err(DriverError::Io));
        assert_eq!(
            MAPS.load(Ordering::Acquire) - UNMAPS.load(Ordering::Acquire),
            2
        );
        drop(gpu);
        assert_eq!(MAPS.load(Ordering::Acquire), UNMAPS.load(Ordering::Acquire));
    }
    assert!(MAPS.load(Ordering::Acquire) > before_maps);
    assert_eq!(
        MAPS.load(Ordering::Acquire) - before_maps,
        UNMAPS.load(Ordering::Acquire) - before_unmaps
    );
    let gpu = device();
    gpu.create_context(4).unwrap();
}

#[def_test(serial)]
fn gpu_real_hal_competing_callers_share_one_owned_queue() {
    let gpu = Arc::new(device());
    gpu.create_context(1).unwrap();
    let before = gpu.progress();
    let ready = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let (gpu, ready, completed) = (gpu.clone(), ready.clone(), completed.clone());
        tasks.push(ktask::spawn(move || {
            ready.fetch_add(1, Ordering::Release);
            while ready.load(Ordering::Acquire) != 2 {
                ktask::yield_now();
            }
            for _ in 0..8 {
                core::assert_eq!(gpu.submit(1, &[0; 4]), Ok(()));
                completed.fetch_add(1, Ordering::Release);
                ktask::yield_now();
            }
        }));
    }
    for task in tasks {
        task.join();
    }
    assert_eq!(completed.load(Ordering::Acquire), 16);
    assert_eq!(gpu.progress().0 - before.0, 16);
    assert_eq!(MAPS.load(Ordering::Acquire), UNMAPS.load(Ordering::Acquire));
}
