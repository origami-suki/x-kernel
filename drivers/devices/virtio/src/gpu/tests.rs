// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Protocol/lifecycle regressions using the owned GPU queue and a fake device.

use alloc::{sync::Arc, vec};
use core::sync::atomic::{Ordering, fence};

use unittest::{assert, assert_eq, def_test};
use virtio_drivers::{
    PhysAddr,
    transport::{DeviceStatus, DeviceType, InterruptStatus},
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use super::*;
use crate::mock_virtio::{MockHal, MockTransport};

// SAFETY: this HAL is only paired with the software fake device. Its DMA
// storage is ordinary CPU memory observed under fixture locks/on notify; a
// CPU fence orders that model, not production MMIO. Recording is preallocated.
unsafe impl GpuHal for MockHal {
    fn dma_write_barrier() {
        fence(Ordering::SeqCst);
        record_order(b'W');
    }

    fn dma_read_barrier() {
        fence(Ordering::SeqCst);
        record_order(b'R');
    }
}
// SAFETY: FaultHal uses the same fake CPU-memory device as MockHal.
unsafe impl GpuHal for FaultHal {
    fn dma_write_barrier() {
        <MockHal as GpuHal>::dma_write_barrier();
    }

    fn dma_read_barrier() {
        <MockHal as GpuHal>::dma_read_barrier();
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    address: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[derive(Default)]
struct Hardware {
    descriptors: usize,
    available: usize,
    used: usize,
    depth: usize,
    seen: u16,
    commands: Vec<u32>,
    negotiated_features: u64,
    capsets: Vec<(u32, u32, u32)>,
    reject_attach: bool,
    reject_detach: bool,
    reject_disable: bool,
    delay_runtime: bool,
    bad_fence: bool,
    short_runtime_reply: bool,
    disabled_scanout: bool,
    short_display_reply: bool,
    full_display_reply: bool,
    empty_capset_reply: bool,
    unset_queues: usize,
    notifications: usize,
    observe_ordering: bool,
    translate: Option<fn(PhysAddr) -> usize>,
}

impl Hardware {
    fn cpu_address(&self, address: PhysAddr) -> usize {
        self.translate.map_or(address as usize, |f| f(address))
    }

    fn reply(&mut self, request: &[u8]) -> Vec<u8> {
        let word = |offset| u32::from_le_bytes(request[offset..offset + 4].try_into().unwrap());
        let command = word(0);
        self.commands.push(command);
        let mut response = vec![0u8; 24];
        let response_type: u32 = match command {
            0x100 => {
                response.resize(48, 0);
                response[32..36].copy_from_slice(&640u32.to_le_bytes());
                response[36..40].copy_from_slice(&480u32.to_le_bytes());
                response[40..44].copy_from_slice(&u32::from(!self.disabled_scanout).to_le_bytes());
                if self.full_display_reply {
                    response.resize(24 + 16 * 24, 0);
                }
                if self.short_display_reply {
                    response.truncate(24);
                }
                0x1101
            }
            0x108 => {
                let (id, version, size) = self.capsets[word(24) as usize];
                response.extend_from_slice(&id.to_le_bytes());
                response.extend_from_slice(&version.to_le_bytes());
                response.extend_from_slice(&size.to_le_bytes());
                response.extend_from_slice(&0u32.to_le_bytes());
                0x1102
            }
            0x109 => {
                if !self.empty_capset_reply {
                    response.extend_from_slice(&word(24).to_le_bytes());
                    response.extend_from_slice(&word(28).to_le_bytes());
                }
                0x1103
            }
            0x106 if self.reject_attach => 0x1200,
            0x107 if self.reject_detach => 0x1200,
            0x103 if self.reject_disable && word(44) == 0 => 0x1200,
            _ => 0x1100,
        };
        response[..4].copy_from_slice(&response_type.to_le_bytes());
        if word(4) & 1 != 0 {
            response[4..24].copy_from_slice(&request[4..24]);
            if self.bad_fence {
                response[8] ^= 1;
            }
            if self.short_runtime_reply {
                response.truncate(16);
            }
        }
        response
    }

    fn notified(&mut self) {
        core::assert_ne!(self.descriptors, 0);
        // SAFETY: queue_set publishes these live MockHal identity-mapped queue
        // allocations until queue_unset. This fake completes commands inside
        // notify, under its state lock. All descriptor indices, traversal
        // counts and output bounds are checked before dereferencing them.
        unsafe {
            let available = self.available as *const u16;
            let end = available.add(1).read_volatile();
            while self.seen != end {
                let token = available
                    .add(2 + usize::from(self.seen) % self.depth)
                    .read_volatile();
                let table = self.descriptors as *const Descriptor;
                let mut index = usize::from(token);
                let mut request = Vec::new();
                let mut outputs = Vec::new();
                for step in 0..self.depth {
                    core::assert!(index < self.depth);
                    let desc = table.add(index).read_volatile();
                    core::assert_eq!(desc.flags & 4, 0, "fake advertises direct descriptors only");
                    if desc.flags & 2 == 0 {
                        request.extend_from_slice(core::slice::from_raw_parts(
                            self.cpu_address(desc.address) as *const u8,
                            desc.len as usize,
                        ));
                    } else {
                        outputs
                            .push((self.cpu_address(desc.address) as *mut u8, desc.len as usize));
                    }
                    if desc.flags & 1 == 0 {
                        break;
                    }
                    core::assert!(step + 1 < self.depth, "cyclic descriptor chain");
                    index = usize::from(desc.next);
                }
                let response = self.reply(&request);
                let mut written = 0;
                for (address, capacity) in outputs {
                    let count = capacity.min(response.len() - written);
                    core::ptr::copy_nonoverlapping(response.as_ptr().add(written), address, count);
                    written += count;
                }
                core::assert_eq!(written, response.len());
                let used = self.used as *mut u8;
                let slot = used
                    .add(4 + (usize::from(self.seen) % self.depth) * 8)
                    .cast::<u32>();
                slot.write_volatile(u32::from(token));
                slot.add(1).write_volatile(written as u32);
                fence(Ordering::Release);
                self.seen = self.seen.wrapping_add(1);
                used.add(2).cast::<u16>().write_volatile(self.seen);
            }
        }
    }
}

struct GpuTransport {
    inner: MockTransport,
    hardware: Arc<SpinNoIrq<Hardware>>,
}

impl Transport for GpuTransport {
    fn device_type(&self) -> DeviceType {
        DeviceType::GPU
    }

    fn read_device_features(&mut self) -> u64 {
        self.inner.read_device_features()
    }

    fn write_driver_features(&mut self, features: u64) {
        self.hardware.lock().negotiated_features = features;
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        self.inner.max_queue_size(queue)
    }

    fn notify(&mut self, queue: u16) {
        core::assert_eq!(queue, 0);
        let mut hardware = self.hardware.lock();
        hardware.notifications += 1;
        if hardware.observe_ordering {
            record_order(b'N');
        }
        if !hardware.delay_runtime {
            hardware.notified();
        }
    }

    fn get_status(&self) -> DeviceStatus {
        self.inner.get_status()
    }

    fn set_status(&mut self, status: DeviceStatus) {
        self.inner.set_status(status);
    }

    fn set_guest_page_size(&mut self, size: u32) {
        self.inner.set_guest_page_size(size);
    }

    fn requires_legacy_layout(&self) -> bool {
        false
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver: PhysAddr,
        device: PhysAddr,
    ) {
        if queue == 0 {
            let mut hw = self.hardware.lock();
            hw.descriptors = hw.cpu_address(descriptors);
            hw.available = hw.cpu_address(driver);
            hw.used = hw.cpu_address(device);
            hw.depth = size as usize;
            if hw.observe_ordering {
                let mut probe = ORDER_PROBE.lock();
                probe.descriptors = hw.descriptors;
                probe.available = hw.available;
                probe.used = hw.used;
            }
        }
    }

    fn queue_unset(&mut self, queue: u16) {
        let mut hw = self.hardware.lock();
        hw.unset_queues += 1;
        if queue == 0 {
            hw.descriptors = 0;
        }
    }

    fn queue_used(&mut self, _queue: u16) -> bool {
        false
    }

    fn ack_interrupt(&mut self) -> InterruptStatus {
        InterruptStatus::empty()
    }

    fn read_config_generation(&self) -> u32 {
        0
    }

    fn read_config_space<T: FromBytes + IntoBytes>(
        &self,
        offset: usize,
    ) -> virtio_drivers::Result<T> {
        self.inner.read_config_space(offset)
    }

    fn write_config_space<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> virtio_drivers::Result<()> {
        self.inner.write_config_space(offset, value)
    }
}

fn transport(
    has_virgl: bool,
    capsets: Vec<(u32, u32, u32)>,
) -> (GpuTransport, Arc<SpinNoIrq<Hardware>>) {
    let mut inner = MockTransport::new_with_type(DeviceType::GPU);
    inner.features = (1 << 32) | u64::from(has_virgl);
    inner.write_config_space(8, 1u32).unwrap();
    inner.write_config_space(12, capsets.len() as u32).unwrap();
    let hardware = Arc::new(SpinNoIrq::new(Hardware {
        capsets,
        ..Hardware::default()
    }));
    (
        GpuTransport {
            inner,
            hardware: hardware.clone(),
        },
        hardware,
    )
}

#[def_test]
fn test_gpu_no_virgl_preserves_scanout_protocol() {
    let (transport, hw) = transport(false, vec![]);
    let gpu = VirtIoGpuDev::<MockHal, _>::try_new_with_execution(
        transport,
        TestExecution::new(hw.clone()),
    )
    .unwrap();
    assert!(!gpu.has_virgl());
    assert!(gpu.virgl_capsets().is_none());
    assert_eq!(gpu.virgl_capset(1, 0), Err(DriverError::Unsupported));
    assert_eq!(gpu.info().width, 640);
    let resource = ScanoutResource {
        id: 37,
        width: 640,
        height: 480,
        pitch: 2560,
        format: display::ScanoutFormat::Bgra8888,
    };
    gpu.create_scanout_resource_owned(resource, backing_size(640 * 480 * 4).0)
        .unwrap();
    gpu.present_scanout_resource(
        37,
        ScanoutRect {
            width: 640,
            height: 480,
            ..ScanoutRect::default()
        },
    )
    .unwrap();
    gpu.destroy_scanout_resource(37).unwrap();
    assert_eq!(
        hw.lock().commands,
        vec![
            0x100, 0x101, 0x106, 0x105, 0x103, 0x104, 0x103, 0x107, 0x102
        ]
    );
    drop(gpu);
    assert_eq!(hw.lock().unset_queues, 2);
}

#[def_test]
fn test_gpu_virgl_capsets_are_device_data_and_queries_do_not_submit() {
    let (transport, hw) = transport(true, vec![(1, 1, 8), (2, 0, 8), (4, 0, 8)]);
    let gpu = VirtIoGpuDev::<MockHal, _>::try_new(transport).unwrap();
    assert!(gpu.has_virgl());
    assert_eq!(hw.lock().negotiated_features & 1, 1);
    assert_eq!(gpu.virgl_capsets().unwrap().len(), 2);
    let commands = hw.lock().commands.len();
    let first = gpu.virgl_capset(1, 1).unwrap();
    let second = gpu.virgl_capset(2, 0).unwrap();
    assert_eq!(first.as_ref(), &[1, 0, 0, 0, 1, 0, 0, 0]);
    assert_eq!(second.as_ref(), &[2, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(gpu.virgl_capset(1, 2), Err(DriverError::InvalidInput));
    assert_eq!(gpu.virgl_capset(4, 0), Err(DriverError::InvalidInput));
    assert_eq!(hw.lock().commands.len(), commands);
}

#[def_test]
fn test_gpu_rejects_unbounded_or_duplicate_capsets_and_releases_queues() {
    for caps in [
        vec![(1, 0, MAX_CAPSET_SIZE + 1)],
        vec![(1, MAX_CAPSET_VERSION + 1, 8)],
        vec![(1, 0, 0)],
        vec![(1, 0, 8); 2],
        vec![(1, 0, 8); 17],
    ] {
        let (transport, hw) = transport(true, caps);
        assert!(matches!(
            VirtIoGpuDev::<MockHal, _>::try_new(transport),
            Err(DriverError::InvalidInput)
        ));
        assert_eq!(hw.lock().unset_queues, 2);
    }
}

#[def_test]
fn test_gpu_attach_failure_unrefs_created_resource() {
    let (transport, hw) = transport(false, vec![]);
    hw.lock().reject_attach = true;
    let gpu = VirtIoGpuDev::<MockHal, _>::try_new_with_execution(
        transport,
        TestExecution::new(hw.clone()),
    )
    .unwrap();
    let resource = ScanoutResource {
        id: 37,
        width: 1,
        height: 1,
        pitch: 4,
        format: display::ScanoutFormat::Bgra8888,
    };
    assert_eq!(
        gpu.create_scanout_resource_owned(resource, backing().0),
        Err(DriverError::Io)
    );
    assert_eq!(hw.lock().commands, vec![0x100, 0x101, 0x106, 0x102]);
}

#[def_test]
fn test_gpu_disabled_scanout_with_nonzero_geometry_is_rejected() {
    let (transport, hw) = transport(false, vec![]);
    hw.lock().disabled_scanout = true;
    assert!(matches!(
        VirtIoGpuDev::<MockHal, _>::try_new(transport),
        Err(DriverError::Io)
    ));
    assert_eq!(hw.lock().commands, vec![0x100]);
    assert_eq!(hw.lock().unset_queues, 2);
}

#[def_test]
fn test_gpu_short_device_replies_fail_without_publishing_capabilities() {
    let (transport, hw) = transport(false, vec![]);
    hw.lock().short_display_reply = true;
    assert!(matches!(
        VirtIoGpuDev::<MockHal, _>::try_new(transport),
        Err(DriverError::Io)
    ));
    assert_eq!(hw.lock().unset_queues, 2);
    let (transport, hw) = self::transport(true, vec![(1, 0, 8)]);
    hw.lock().empty_capset_reply = true;
    assert!(matches!(
        VirtIoGpuDev::<MockHal, _>::try_new(transport),
        Err(DriverError::Io)
    ));
    assert_eq!(hw.lock().unset_queues, 2);
}

struct TestExecution {
    hardware: Arc<SpinNoIrq<Hardware>>,
    clock: core::sync::atomic::AtomicU64,
    yields: core::sync::atomic::AtomicUsize,
    step_ns: core::sync::atomic::AtomicU64,
    complete_after: core::sync::atomic::AtomicUsize,
    work: SpinNoIrq<Option<Arc<TestWork>>>,
}
struct TestWork {
    callback: Box<dyn Fn() + Send + Sync>,
    scheduled: core::sync::atomic::AtomicBool,
}
impl display::GpuDeferredWork for TestWork {
    fn schedule(&self) -> DriverResult {
        self.scheduled.store(true, Ordering::Release);
        Ok(())
    }
}
impl display::GpuExecution for TestExecution {
    fn run(
        &self,
        operation: &mut dyn FnMut(&mut dyn display::GpuWait) -> DriverResult,
    ) -> DriverResult {
        operation(&mut TestWait(self))
    }

    fn deferred(&self, callback: Box<dyn Fn() + Send + Sync>) -> Arc<dyn display::GpuDeferredWork> {
        let work = Arc::new(TestWork {
            callback,
            scheduled: core::sync::atomic::AtomicBool::new(false),
        });
        *self.work.lock() = Some(work.clone());
        work
    }
}
struct TestWait<'a>(&'a TestExecution);
impl display::GpuWait for TestWait<'_> {
    fn now_ns(&self) -> u64 {
        self.0.clock.load(Ordering::Acquire)
    }

    fn yield_execution(&mut self) {
        let n = self.0.yields.fetch_add(1, Ordering::AcqRel) + 1;
        self.0
            .clock
            .fetch_add(self.0.step_ns.load(Ordering::Relaxed), Ordering::AcqRel);
        // Emulates device/peer execution only when the caller gives up execution.
        // If a runtime command spins inside its queue call these tests hang.
        if n >= self.0.complete_after.load(Ordering::Acquire) {
            self.0.hardware.lock().notified();
        }
    }
}
impl TestExecution {
    fn new(hardware: Arc<SpinNoIrq<Hardware>>) -> Arc<Self> {
        Arc::new(Self {
            hardware,
            clock: 0.into(),
            yields: 0.into(),
            step_ns: 1.into(),
            complete_after: 1.into(),
            work: SpinNoIrq::new(None),
        })
    }

    fn drain(&self) {
        let work = self.work.lock().clone().unwrap();
        if work.scheduled.swap(false, Ordering::AcqRel) {
            (work.callback)();
        }
    }
}
fn runtime_gpu(
    virgl: bool,
) -> (
    VirtIoGpuDev<MockHal, GpuTransport>,
    Arc<SpinNoIrq<Hardware>>,
    Arc<TestExecution>,
) {
    let (transport, hw) = transport(virgl, vec![]);
    let execution = TestExecution::new(hw.clone());
    let gpu = VirtIoGpuDev::try_new_with_execution(transport, execution.clone()).unwrap();
    (gpu, hw, execution)
}
fn resource_params(id: u32) -> display::VirglResourceCreate {
    display::VirglResourceCreate {
        resource_id: id,
        target: 2,
        format: 1,
        bind: 2,
        width: 4,
        height: 4,
        depth: 1,
        array_size: 1,
        last_level: 0,
        nr_samples: 0,
        flags: 0,
    }
}
fn backing() -> (display::GpuBacking, alloc::sync::Weak<Vec<u8>>) {
    backing_size(4096)
}
fn backing_size(size: usize) -> (display::GpuBacking, alloc::sync::Weak<Vec<u8>>) {
    let bytes = Arc::new(vec![0u8; size]);
    let weak = Arc::downgrade(&bytes);
    // SAFETY: MockHal identity maps this stable allocation; tests never DMA the
    // resource contents, and the owner retains its entire advertised range.
    (
        unsafe { display::GpuBacking::new(bytes.as_ptr() as u64, bytes.len() as u32, bytes) },
        weak,
    )
}

#[def_test]
fn test_gpu_owned_commands_yield_and_fit_two_direct_descriptors() {
    let (gpu, hw, execution) = runtime_gpu(true);
    hw.lock().delay_runtime = true;
    execution.complete_after.store(3, Ordering::Release);
    gpu.virgl_command(VirglCommand::CreateContext {
        context_id: 3,
        name: "probe",
    })
    .unwrap();
    assert!(execution.yields.load(Ordering::Acquire) >= 3);
    let (backing, weak) = backing();
    gpu.virgl_command(VirglCommand::CreateResource {
        params: resource_params(37),
        backing,
    })
    .unwrap();
    gpu.virgl_command(VirglCommand::AttachContext {
        context_id: 3,
        resource_id: 37,
    })
    .unwrap();
    gpu.virgl_command(VirglCommand::Submit {
        context_id: 3,
        commands: &[0; 16384],
    })
    .unwrap();
    let transfer = display::VirglTransfer {
        context_id: 3,
        resource_id: 37,
        box_: display::VirglBox {
            x: 0,
            y: 0,
            z: 0,
            width: 4,
            height: 4,
            depth: 1,
        },
        offset: 0,
        level: 0,
        stride: 0,
        layer_stride: 0,
    };
    gpu.virgl_command(VirglCommand::TransferToHost(transfer))
        .unwrap();
    gpu.virgl_command(VirglCommand::TransferFromHost(transfer))
        .unwrap();
    let before = hw.lock().commands.len();
    gpu.virgl_command(VirglCommand::Present {
        resource_id: 37,
        rect: ScanoutRect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        },
    })
    .unwrap();
    assert_eq!(&hw.lock().commands[before..], &[0x103, 0x104]);
    assert!(weak.upgrade().is_some());
    gpu.enqueue_virgl_cleanup(37).unwrap();
    assert!(
        weak.upgrade().is_some(),
        "enqueue must not execute GPU teardown inline"
    );
    execution.drain();
    assert!(weak.upgrade().is_none());
    assert_eq!(gpu.virgl_command_status().resources, 0);
    gpu.enqueue_virgl_context_cleanup(3).unwrap();
    execution.drain();
    assert_eq!(gpu.virgl_command_status().contexts, 0);
}

#[def_test]
fn test_gpu_timeout_keeps_request_and_backing_until_reset() {
    let (gpu, hw, execution) = runtime_gpu(true);
    let (backing, weak) = backing();
    gpu.virgl_command(VirglCommand::CreateResource {
        params: resource_params(37),
        backing,
    })
    .unwrap();
    hw.lock().delay_runtime = true;
    execution
        .complete_after
        .store(usize::MAX, Ordering::Release);
    execution.step_ns.store(15_000_000_000, Ordering::Release);
    assert_eq!(
        gpu.virgl_command(VirglCommand::DestroyResource { resource_id: 37 }),
        Err(DriverError::Io)
    );
    let status = gpu.virgl_command_status();
    assert!(status.is_failed);
    assert_eq!(status.pending_requests, 1);
    assert!(weak.upgrade().is_some());
    let (rejected, rejected_owner) = self::backing();
    assert_eq!(
        gpu.create_scanout_resource_owned(
            ScanoutResource {
                id: 38,
                width: 1,
                height: 1,
                pitch: 4,
                format: display::ScanoutFormat::Bgra8888
            },
            rejected
        ),
        Err(DriverError::BadState)
    );
    assert!(rejected_owner.upgrade().is_none());
    assert_eq!(gpu.virgl_command_status().resources, 1);
    hw.lock().notified();
    assert_eq!(
        gpu.virgl_command(VirglCommand::DestroyResource { resource_id: 37 }),
        Err(DriverError::BadState)
    );
    assert_eq!(gpu.virgl_command_status().pending_requests, 0);
    assert!(
        weak.upgrade().is_some(),
        "late completion must not resume an uncertain transaction"
    );
    drop(gpu);
    assert!(
        weak.upgrade().is_none(),
        "acknowledged reset permits releasing DMA owners"
    );
}

#[def_test]
fn test_gpu_malformed_fence_and_short_response_poison_admission() {
    for short in [false, true] {
        let (gpu, hw, _) = runtime_gpu(true);
        hw.lock().bad_fence = !short;
        hw.lock().short_runtime_reply = short;
        assert_eq!(
            gpu.virgl_command(VirglCommand::CreateContext {
                context_id: 2,
                name: "bad-reply"
            }),
            Err(DriverError::Io)
        );
        assert!(gpu.virgl_command_status().is_failed);
        assert!(!gpu.has_virgl_commands());
    }
}

#[def_test]
fn test_gpu_teardown_error_retains_backing_and_can_be_retried() {
    let (gpu, hw, execution) = runtime_gpu(true);
    let (backing, weak) = backing();
    gpu.virgl_command(VirglCommand::CreateResource {
        params: resource_params(37),
        backing,
    })
    .unwrap();
    hw.lock().reject_detach = true;
    gpu.enqueue_virgl_cleanup(37).unwrap();
    execution.drain();
    assert!(weak.upgrade().is_some());
    assert_eq!(gpu.virgl_command_status().pending_cleanup, 1);
    assert!(!gpu.virgl_command_status().is_failed);
    hw.lock().reject_detach = false;
    gpu.enqueue_virgl_cleanup(37).unwrap();
    execution.drain();
    assert!(weak.upgrade().is_none());
}

#[def_test]
fn test_gpu_creation_error_rolls_back_owner_and_duplicate_attach_is_idempotent() {
    let (gpu, hw, _) = runtime_gpu(true);
    let (owned, weak) = backing();
    hw.lock().reject_attach = true;
    assert_eq!(
        gpu.virgl_command(VirglCommand::CreateResource {
            params: resource_params(37),
            backing: owned
        }),
        Err(DriverError::Io)
    );
    assert!(weak.upgrade().is_none());
    assert_eq!(gpu.virgl_command_status().resources, 0);
    hw.lock().reject_attach = false;
    gpu.virgl_command(VirglCommand::CreateContext {
        context_id: 3,
        name: "attach",
    })
    .unwrap();
    gpu.virgl_command(VirglCommand::CreateResource {
        params: resource_params(38),
        backing: backing().0,
    })
    .unwrap();
    gpu.virgl_command(VirglCommand::AttachContext {
        context_id: 3,
        resource_id: 38,
    })
    .unwrap();
    let count = hw.lock().commands.len();
    gpu.virgl_command(VirglCommand::AttachContext {
        context_id: 3,
        resource_id: 38,
    })
    .unwrap();
    assert_eq!(hw.lock().commands.len(), count);
    gpu.enqueue_virgl_context_cleanup(3).unwrap();
    gpu.enqueue_virgl_cleanup(38).unwrap();
    gpu.flush_virgl_cleanup().unwrap();
    assert_eq!(gpu.virgl_command_status().resources, 0);
    assert_eq!(gpu.virgl_command_status().contexts, 0);
}

#[def_test]
fn test_gpu_invalid_runtime_inputs_do_not_submit_or_publish_objects() {
    let (gpu, hw, _) = runtime_gpu(true);
    let count = hw.lock().commands.len();
    assert_eq!(
        gpu.virgl_command(VirglCommand::CreateContext {
            context_id: 0,
            name: "invalid"
        }),
        Err(DriverError::InvalidInput)
    );
    let (owned, weak) = backing();
    let mut params = resource_params(1);
    params.width = 0;
    assert_eq!(
        gpu.virgl_command(VirglCommand::CreateResource {
            params,
            backing: owned
        }),
        Err(DriverError::InvalidInput)
    );
    assert!(weak.upgrade().is_none());
    assert_eq!(gpu.virgl_command_status().resources, 0);
    assert_eq!(hw.lock().commands.len(), count);
    gpu.virgl_command(VirglCommand::CreateContext {
        context_id: 1,
        name: "ok",
    })
    .unwrap();
    let count = hw.lock().commands.len();
    assert_eq!(
        gpu.virgl_command(VirglCommand::Submit {
            context_id: 1,
            commands: &[0; 3]
        }),
        Err(DriverError::InvalidInput)
    );
    assert_eq!(hw.lock().commands.len(), count);
}

#[derive(Default)]
struct MappingStats {
    attempts: usize,
    live_bytes: usize,
    unmaps: usize,
    fail_at: usize,
}
kspin::static_lock! { static MAPPING_STATS:SpinNoIrq<MappingStats> = SpinNoIrq::new(MappingStats{attempts:0,live_bytes:0,unmaps:0,fail_at:0}); }
struct FaultHal;
// SAFETY: allocation/MMIO methods delegate the existing test HAL. Streaming
// failure returns the actual integration sentinel zero, without acquiring a
// mapping; successful identity maps are accounted and released exactly once.
unsafe impl Hal for FaultHal {
    fn dma_alloc(
        pages: usize,
        direction: virtio_drivers::BufferDirection,
        access: bool,
    ) -> (PhysAddr, core::ptr::NonNull<u8>) {
        MockHal::dma_alloc(pages, direction, access)
    }

    unsafe fn dma_dealloc(
        pa: PhysAddr,
        va: core::ptr::NonNull<u8>,
        pages: usize,
        access: bool,
    ) -> i32 {
        // SAFETY: same allocation/arguments as the caller's HAL contract.
        unsafe { MockHal::dma_dealloc(pa, va, pages, access) }
    }

    unsafe fn mmio_phys_to_virt(pa: PhysAddr, size: usize) -> core::ptr::NonNull<u8> {
        // SAFETY: caller provides a valid MMIO range.
        unsafe { MockHal::mmio_phys_to_virt(pa, size) }
    }

    unsafe fn share(
        buffer: core::ptr::NonNull<[u8]>,
        direction: virtio_drivers::BufferDirection,
        access: bool,
    ) -> PhysAddr {
        let mut stats = MAPPING_STATS.lock();
        stats.attempts += 1;
        if stats.fail_at == stats.attempts {
            return 0;
        }
        stats.live_bytes += buffer.len();
        drop(stats);
        // SAFETY: the caller owns a live stable buffer until matching unshare.
        unsafe { MockHal::share(buffer, direction, access) }
    }

    unsafe fn unshare(
        pa: PhysAddr,
        buffer: core::ptr::NonNull<[u8]>,
        direction: virtio_drivers::BufferDirection,
        access: bool,
    ) {
        core::assert_ne!(pa, 0);
        {
            let mut stats = MAPPING_STATS.lock();
            stats.live_bytes -= buffer.len();
            stats.unmaps += 1;
        }
        // SAFETY: exact successful identity mapping from share above.
        unsafe { MockHal::unshare(pa, buffer, direction, access) }
    }
}

#[def_test(serial)]
fn test_gpu_failed_mapping_never_publishes_and_rolls_back_prefix() {
    for fail_at in [1, 2] {
        *MAPPING_STATS.lock() = MappingStats::default();
        let (transport, hw) = transport(true, vec![]);
        let execution = TestExecution::new(hw.clone());
        let gpu =
            VirtIoGpuDev::<FaultHal, _>::try_new_with_execution(transport, execution).unwrap();
        core::assert_eq!(MAPPING_STATS.lock().live_bytes, 0);
        *MAPPING_STATS.lock() = MappingStats {
            fail_at,
            ..MappingStats::default()
        };
        let before = hardware_progress(&hw);
        assert_eq!(
            gpu.virgl_command(VirglCommand::CreateContext {
                context_id: 1,
                name: "map-fail"
            }),
            Err(DriverError::NoMemory)
        );
        assert_eq!(
            hardware_progress(&hw),
            before,
            "failed map must not publish or notify"
        );
        assert_eq!(MAPPING_STATS.lock().attempts, fail_at);
        assert_eq!(MAPPING_STATS.lock().unmaps, fail_at - 1);
        assert_eq!(MAPPING_STATS.lock().live_bytes, 0);
        assert_eq!(gpu.virgl_command_status().pending_requests, 0);
        assert!(!gpu.virgl_command_status().is_failed);
    }
}

#[def_test(serial)]
fn test_gpu_reset_retires_each_pending_map_even_if_indirect_is_advertised() {
    *MAPPING_STATS.lock() = MappingStats::default();
    let (mut transport, hw) = transport(true, vec![]);
    transport.inner.features |= 1 << 28;
    let execution = TestExecution::new(hw.clone());
    let gpu =
        VirtIoGpuDev::<FaultHal, _>::try_new_with_execution(transport, execution.clone()).unwrap();
    assert_eq!(
        hw.lock().negotiated_features & (1 << 28),
        0,
        "owned queue never negotiates indirect storage"
    );
    *MAPPING_STATS.lock() = MappingStats::default();
    hw.lock().delay_runtime = true;
    execution
        .complete_after
        .store(usize::MAX, Ordering::Release);
    execution.step_ns.store(15_000_000_000, Ordering::Release);
    assert_eq!(
        gpu.virgl_command(VirglCommand::CreateContext {
            context_id: 1,
            name: "timeout"
        }),
        Err(DriverError::Io)
    );
    assert_eq!(MAPPING_STATS.lock().attempts, 2);
    assert_eq!(MAPPING_STATS.lock().unmaps, 0);
    assert!(MAPPING_STATS.lock().live_bytes > 0);
    drop(gpu);
    assert_eq!(MAPPING_STATS.lock().unmaps, 2);
    assert_eq!(MAPPING_STATS.lock().live_bytes, 0);
}

fn hardware_progress(hardware: &Arc<SpinNoIrq<Hardware>>) -> (usize, u16) {
    let hw = hardware.lock();
    // SAFETY: fixture is live and no CPU command is running when sampled.
    // The device only reads avail; the aligned driver-owned index stays valid.
    let published = unsafe { (hw.available as *const u16).add(1).read_volatile() };
    (hw.notifications, published)
}

/// Test-only fake GPU transport around an arbitrary real kernel HAL.
/// The caller supplies bus-to-CPU translation for coherent/bounce allocations.
pub struct GpuDmaTestDevice<H: GpuHal + 'static> {
    gpu: VirtIoGpuDev<H, GpuTransport>,
    hardware: Arc<SpinNoIrq<Hardware>>,
}
impl<H: GpuHal + 'static> GpuDmaTestDevice<H> {
    /// Initializes the actual queue and HAL with a bounded fake device.
    ///
    /// # Safety
    /// `translate` must resolve every coherent/streaming bus allocation made by
    /// H to its valid CPU address until matching deallocation/unshare. The test
    /// fixture alone may emulate writes to those device-owned mappings.
    pub unsafe fn new(
        translate: fn(PhysAddr) -> usize,
        execution: Arc<dyn display::GpuExecution>,
    ) -> DriverResult<Self> {
        let (transport, hardware) = transport(true, vec![]);
        hardware.lock().translate = Some(translate);
        let gpu = VirtIoGpuDev::try_new_with_execution(transport, execution)?;
        Ok(Self { gpu, hardware })
    }

    /// Submit a small fenced context command.
    pub fn create_context(&self, id: u32) -> DriverResult {
        self.gpu.virgl_command(VirglCommand::CreateContext {
            context_id: id,
            name: "real-hal",
        })
    }

    /// Submit owned bytes on an already created context.
    pub fn submit(&self, id: u32, commands: &[u8]) -> DriverResult {
        self.gpu.virgl_command(VirglCommand::Submit {
            context_id: id,
            commands,
        })
    }

    /// Withhold completion so timeout/reset retirement can be exercised.
    pub fn delay_completion(&self) {
        self.hardware.lock().delay_runtime = true;
    }

    /// Snapshot publication/notification counters between CPU command calls.
    pub fn progress(&self) -> (usize, u16) {
        hardware_progress(&self.hardware)
    }
}

#[def_test]
fn test_gpu_bootstrap_accepts_the_complete_sixteen_scanout_reply() {
    let (transport, hardware) = transport(false, vec![]);
    hardware.lock().full_display_reply = true;
    let gpu = VirtIoGpuDev::<MockHal, _>::try_new(transport).unwrap();
    assert_eq!(gpu.info().width, 640);
    assert_eq!(gpu.info().height, 480);
}

#[def_test]
fn test_gpu_buffer_zero_array_size_is_legal_but_texture_zero_is_not() {
    let (gpu, hardware, _) = runtime_gpu(true);
    let mut params = resource_params(71);
    params.target = 0;
    params.array_size = 0;
    params.width = 8;
    params.height = 1;
    let (owned, owner) = backing();
    gpu.virgl_command(VirglCommand::CreateResource {
        params,
        backing: owned,
    })
    .unwrap();
    assert!(owner.upgrade().is_some());
    gpu.enqueue_virgl_cleanup(71).unwrap();
    gpu.flush_virgl_cleanup().unwrap();
    assert!(owner.upgrade().is_none());
    let count = hardware.lock().commands.len();
    params.resource_id = 72;
    params.target = 2;
    let (owned, owner) = backing();
    assert_eq!(
        gpu.virgl_command(VirglCommand::CreateResource {
            params,
            backing: owned
        }),
        Err(DriverError::InvalidInput)
    );
    assert!(owner.upgrade().is_none());
    assert_eq!(hardware.lock().commands.len(), count);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OrderEvent {
    phase: u8,
    available: u16,
    used: u16,
    request_length: u32,
    command: u32,
}
struct OrderProbe {
    descriptors: usize,
    available: usize,
    used: usize,
    count: usize,
    events: [OrderEvent; 32],
}
impl OrderProbe {
    const fn empty() -> Self {
        Self {
            descriptors: 0,
            available: 0,
            used: 0,
            count: 0,
            events: [OrderEvent {
                phase: 0,
                available: 0,
                used: 0,
                request_length: 0,
                command: 0,
            }; 32],
        }
    }
}
kspin::static_lock! {static ORDER_PROBE:SpinNoIrq<OrderProbe> = SpinNoIrq::new(OrderProbe::empty());}
struct OrderProbeGuard;
impl Drop for OrderProbeGuard {
    fn drop(&mut self) {
        *ORDER_PROBE.lock() = OrderProbe::empty();
    }
}
fn record_order(phase: u8) {
    let mut probe = ORDER_PROBE.lock();
    if probe.descriptors == 0 || probe.count == probe.events.len() {
        return;
    }
    // SAFETY: the serial fixture owns all three live identity-mapped ring
    // regions until its guard clears this probe. The sole CPU command caller
    // invokes each hook; notify has not run at either write hook. The request
    // buffer is immutable and remains mapped through the read hook.
    let event = unsafe {
        let descriptor = (probe.descriptors as *const Descriptor).read_volatile();
        OrderEvent {
            phase,
            available: (probe.available as *const u16).add(1).read_volatile(),
            used: (probe.used as *const u16).add(1).read_volatile(),
            request_length: descriptor.len,
            command: (descriptor.address as *const u32).read_unaligned(),
        }
    };
    let position = probe.count;
    probe.events[position] = event;
    probe.count += 1;
}

#[def_test(serial)]
fn test_gpu_dma_barriers_bracket_bootstrap_and_runtime_publication() {
    *ORDER_PROBE.lock() = OrderProbe::empty();
    let _guard = OrderProbeGuard;
    let (transport, hardware) = transport(true, vec![]);
    hardware.lock().observe_ordering = true;
    let gpu =
        VirtIoGpuDev::<MockHal, _>::try_new_with_execution(transport, TestExecution::new(hardware))
            .unwrap();
    gpu.virgl_command(VirglCommand::CreateContext {
        context_id: 1,
        name: "ordered",
    })
    .unwrap();
    let probe = ORDER_PROBE.lock();
    assert_eq!(probe.count, 8);
    for (start, old_index, command, length) in [(0, 0, 0x100, 24), (4, 1, 0x200, 96)] {
        let events = &probe.events[start..start + 4];
        assert_eq!(
            events.iter().map(|e| e.phase).collect::<Vec<_>>(),
            vec![b'W', b'W', b'N', b'R']
        );
        assert_eq!(
            events[0].available, old_index,
            "descriptor/data barrier must precede index"
        );
        assert_eq!(
            events[1].available,
            old_index + 1,
            "second barrier must follow index publication"
        );
        assert_eq!(
            events[2].available,
            old_index + 1,
            "notify must observe the published index"
        );
        assert_eq!(
            events[3].used,
            old_index + 1,
            "read barrier follows used-index observation"
        );
        assert!(
            events
                .iter()
                .all(|e| e.command == command && e.request_length == length)
        );
    }
}

#[def_test]
fn test_gpu_disable_2d_scanout_keeps_resource_and_commits_disabled_state() {
    let (gpu, hardware, _) = runtime_gpu(false);
    let (owned, owner) = backing();
    gpu.create_scanout_resource_owned(
        ScanoutResource {
            id: 73,
            width: 1,
            height: 1,
            pitch: 4,
            format: display::ScanoutFormat::Bgra8888,
        },
        owned,
    )
    .unwrap();
    gpu.present_scanout_resource(
        73,
        ScanoutRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
    )
    .unwrap();
    let before = hardware.lock().commands.len();
    gpu.disable_scanout().unwrap();
    assert!(
        owner.upgrade().is_some(),
        "disable is not resource destruction"
    );
    gpu.enqueue_virgl_cleanup(73).unwrap();
    gpu.flush_virgl_cleanup().unwrap();
    assert!(owner.upgrade().is_none());
    assert_eq!(
        &hardware.lock().commands[before..],
        &[0x103, 0x107, 0x102],
        "cleanup must not repeat a successful disable"
    );
}

#[def_test]
fn test_gpu_disable_3d_error_preserves_active_resource_for_cleanup_retry() {
    let (gpu, hardware, _) = runtime_gpu(true);
    let (owned, owner) = backing();
    gpu.virgl_command(VirglCommand::CreateResource {
        params: resource_params(74),
        backing: owned,
    })
    .unwrap();
    gpu.virgl_command(VirglCommand::Present {
        resource_id: 74,
        rect: ScanoutRect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        },
    })
    .unwrap();
    let before = hardware.lock().commands.len();
    hardware.lock().reject_disable = true;
    assert_eq!(gpu.disable_scanout(), Err(DriverError::Io));
    assert!(owner.upgrade().is_some());
    assert!(!gpu.virgl_command_status().is_failed);
    hardware.lock().reject_disable = false;
    gpu.enqueue_virgl_cleanup(74).unwrap();
    gpu.flush_virgl_cleanup().unwrap();
    assert!(owner.upgrade().is_none());
    assert_eq!(
        &hardware.lock().commands[before..],
        &[0x103, 0x103, 0x107, 0x102],
        "failed disable must not clear the active ID"
    );
}

#[def_test]
fn test_gpu_disable_timeout_retains_pending_dma_and_resource_owner() {
    let (gpu, hardware, execution) = runtime_gpu(true);
    let (owned, owner) = backing();
    gpu.virgl_command(VirglCommand::CreateResource {
        params: resource_params(75),
        backing: owned,
    })
    .unwrap();
    gpu.virgl_command(VirglCommand::Present {
        resource_id: 75,
        rect: ScanoutRect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        },
    })
    .unwrap();
    hardware.lock().delay_runtime = true;
    execution
        .complete_after
        .store(usize::MAX, Ordering::Release);
    execution.step_ns.store(15_000_000_000, Ordering::Release);
    assert_eq!(gpu.disable_scanout(), Err(DriverError::Io));
    assert!(owner.upgrade().is_some());
    assert_eq!(gpu.virgl_command_status().pending_requests, 1);
    assert_eq!(gpu.virgl_command_status().resources, 1);
    drop(gpu);
    assert!(
        owner.upgrade().is_none(),
        "only acknowledged reset releases timed-out owner"
    );
}
