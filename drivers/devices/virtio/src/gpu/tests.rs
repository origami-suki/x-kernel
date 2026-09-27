// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Protocol-level regressions using real VirtIOGpu queues and a fake device.

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
    disabled_scanout: bool,
    short_display_reply: bool,
    empty_capset_reply: bool,
    unset_queues: usize,
}

impl Hardware {
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
            _ => 0x1100,
        };
        response[..4].copy_from_slice(&response_type.to_le_bytes());
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
                            desc.address as *const u8,
                            desc.len as usize,
                        ));
                    } else {
                        outputs.push((desc.address as *mut u8, desc.len as usize));
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
        self.hardware.lock().notified();
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
            hw.descriptors = descriptors as usize;
            hw.available = driver as usize;
            hw.used = device as usize;
            hw.depth = size as usize;
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
    let gpu = VirtIoGpuDev::<MockHal, _>::try_new(transport).unwrap();
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
    gpu.create_scanout_resource(resource, 0x1000, 640 * 480 * 4)
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
        vec![0x100, 0x101, 0x106, 0x105, 0x103, 0x104, 0x107, 0x102]
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
    let gpu = VirtIoGpuDev::<MockHal, _>::try_new(transport).unwrap();
    let resource = ScanoutResource {
        id: 37,
        width: 1,
        height: 1,
        pitch: 4,
        format: display::ScanoutFormat::Bgra8888,
    };
    assert_eq!(
        gpu.create_scanout_resource(resource, 0x1000, 4),
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
