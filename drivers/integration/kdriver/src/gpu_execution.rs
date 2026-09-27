// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Sleepable execution and deferred teardown for portable GPU commands.
use alloc::{boxed::Box, sync::Arc};

use display::{GpuDeferredWork, GpuExecution, GpuWait};
use driver_base::{DriverError, DriverResult};
use ksync::Mutex;
use ktask::future::PreparedTaskWait;
use kwork::{QueueWorkResult, ScheduledWork};

/// Full DMA/MMIO ordering, also usable before the scheduler is initialized.
/// AArch64 uses the existing workspace aarch64-cpu primitive at System scope:
/// rings are NormalNonCacheable while the doorbell is Device memory, so an
/// inner-shareable CPU atomic fence or the legacy cache-coherence no-op is not
/// the contract. No nomem/readonly annotation permits compiler reordering.
#[inline]
pub(crate) fn device_memory_barrier() {
    #[cfg(not(any(
        target_arch = "aarch64",
        target_arch = "x86_64",
        target_arch = "riscv32",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )))]
    compile_error!("GPU DMA/MMIO ordering requires an explicit host architecture implementation");
    #[cfg(target_arch = "aarch64")]
    aarch64_cpu::asm::barrier::dmb(aarch64_cpu::asm::barrier::SY);
    #[cfg(target_arch = "x86_64")]
    // SAFETY: x86_64 provides MFENCE; it orders all prior loads/stores, including
    // device/uncached accesses, without touching pointers or SIMD register data.
    unsafe {
        core::arch::x86_64::_mm_mfence()
    };
    #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
    // SAFETY: full I/O + memory fence orders the DMA ring and doorbell domains;
    // no nomem option is used, so the compiler also respects this boundary.
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack))
    };
    #[cfg(target_arch = "loongarch64")]
    karch::dma_read_barrier(); // Existing full `dbar 0` implementation.
}

pub(crate) fn new_execution() -> Arc<dyn GpuExecution> {
    Arc::new(HostGpuExecution {
        gate: Mutex::new(()),
    })
}

struct HostGpuExecution {
    gate: Mutex<()>,
}
struct HostGpuWait {
    _context: PreparedTaskWait,
}

impl GpuWait for HostGpuWait {
    fn now_ns(&self) -> u64 {
        khal::time::monotonic_time()
            .span_since_origin()
            .as_nanos_u64_saturating()
    }

    fn yield_execution(&mut self) {
        ktask::yield_now();
    }
}

impl GpuExecution for HostGpuExecution {
    fn run(&self, operation: &mut dyn FnMut(&mut dyn GpuWait) -> DriverResult) -> DriverResult {
        // Validate before taking the sleepable gate. A syscall task or a normal
        // workqueue task is allowed; IRQ, idle and early boot are rejected.
        let context = PreparedTaskWait::prepare().map_err(|_| DriverError::InvalidInput)?;
        let _gate = self.gate.lock();
        operation(&mut HostGpuWait { _context: context })
    }

    fn deferred(&self, callback: Box<dyn Fn() + Send + Sync>) -> Arc<dyn GpuDeferredWork> {
        Arc::new(HostGpuWork(ScheduledWork::new(move |_| callback())))
    }
}

struct HostGpuWork(ScheduledWork);
impl GpuDeferredWork for HostGpuWork {
    fn schedule(&self) -> DriverResult {
        match self.0.schedule() {
            QueueWorkResult::Queued | QueueWorkResult::AlreadyQueued => Ok(()),
            _ => Err(DriverError::ResourceBusy),
        }
    }
}

#[cfg(unittest)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test(serial)]
    fn gpu_execution_rejects_atomic_context_before_callback() {
        let execution = new_execution();
        let _irq = kspin::NoPreemptIrqSave::new();
        assert_eq!(
            execution.run(&mut |_| panic!("atomic callback must not run")),
            Err(DriverError::InvalidInput)
        );
    }

    #[def_test(serial)]
    fn gpu_execution_serializes_callers_while_timer_and_peer_progress() {
        let execution = new_execution();
        let entered = Arc::new(AtomicBool::new(false));
        let timer_done = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let owner = ktask::spawn({
            let (execution, entered, timer_done, active, completed) = (
                execution.clone(),
                entered.clone(),
                timer_done.clone(),
                active.clone(),
                completed.clone(),
            );
            move || {
                execution
                    .run(&mut |wait| {
                        core::assert_eq!(active.fetch_add(1, Ordering::AcqRel), 0);
                        entered.store(true, Ordering::Release);
                        let deadline = wait.now_ns() + 5_000_000_000;
                        while !timer_done.load(Ordering::Acquire) {
                            core::assert!(
                                wait.now_ns() < deadline,
                                "timer could not progress during GPU wait"
                            );
                            wait.yield_execution();
                        }
                        active.fetch_sub(1, Ordering::Release);
                        completed.fetch_add(1, Ordering::Release);
                        Ok(())
                    })
                    .unwrap()
            }
        });
        while !entered.load(Ordering::Acquire) {
            ktask::yield_now();
        }
        let contender = ktask::spawn({
            let (execution, active, completed) =
                (execution.clone(), active.clone(), completed.clone());
            move || {
                execution
                    .run(&mut |_| {
                        core::assert_eq!(active.fetch_add(1, Ordering::AcqRel), 0);
                        active.fetch_sub(1, Ordering::Release);
                        completed.fetch_add(1, Ordering::Release);
                        Ok(())
                    })
                    .unwrap()
            }
        });
        // This task reaches a timer sleep even while the first caller owns the
        // GPU gate and the second caller contends for it.
        ktask::sleep(ktime_types::TimeSpan::from_millis(2));
        assert_eq!(completed.load(Ordering::Acquire), 0);
        timer_done.store(true, Ordering::Release);
        owner.join();
        contender.join();
        assert_eq!(completed.load(Ordering::Acquire), 2);
    }
}

#[cfg(unittest)]
#[path = "tests/gpu_dma.rs"]
mod dma_tests;
