// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO GPU owned scanout/virgl commands and cached capability discovery.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::mem::ManuallyDrop;

use display::{
    DisplayDevice, DisplayInfo, GpuCommandStatus, GpuExecution, ScanoutRect, ScanoutResource,
    VirglCapsetInfo, VirglCommand,
};
mod commands;
mod queue;
use commands::Engine;
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoIrq;
use queue::{Queue, Transaction};
use virtio_drivers::{
    Hal, PAGE_SIZE,
    config::{ReadOnly, WriteOnly, read_config},
    transport::{InterruptStatus, Transport},
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

const QUEUE_TRANSMIT: u16 = 0;
const QUEUE_CURSOR: u16 = 1;
const SCANOUT_ID: u32 = 0;
// Device-controlled metadata must not turn boot-time discovery into an
// unbounded allocation or command loop. Current virgl sets are much smaller.
const MAX_CAPSETS: u32 = 16;
const MAX_CAPSET_VERSION: u32 = 32;
const MAX_CAPSET_SIZE: u32 = 64 * 1024;
const MAX_CAPSET_CACHE_BYTES: usize = 1024 * 1024;

struct CapsetData {
    id: u32,
    version: u32,
    bytes: Arc<[u8]>,
}

/// GPU DMA/device ordering in addition to the locked allocation/mapping HAL.
///
/// # Safety
/// Implementations must order compiler and hardware accesses across the actual
/// coherent DMA and MMIO domains. CPU-only atomic fences are insufficient when
/// device memory has a wider visibility domain. These operations must not sleep,
/// allocate or panic, and must work during bootstrap and IRQ-disabled queue work.
pub unsafe trait GpuHal: Hal {
    /// Order all prior DMA writes before subsequent DMA publication/MMIO writes.
    /// Called before avail.idx and again after idx, before any doorbell accessor.
    fn dma_write_barrier();
    /// Order an observed device completion index before DMA entry/payload reads.
    fn dma_read_barrier();
}

/// VirtIO GPU adapter with cached metadata and owned fenced runtime commands.
pub struct VirtIoGpuDev<H: GpuHal, T: Transport + Send> {
    info: DisplayInfo,
    has_virgl: bool,
    capsets: Arc<[VirglCapsetInfo]>,
    capset_data: Vec<CapsetData>,
    engine: Arc<Engine<H, T>>,
}

impl<H: GpuHal + 'static, T: Transport + Send + 'static> VirtIoGpuDev<H, T> {
    /// Initializes scanout and caches the device's virgl capability sets.
    ///
    /// Without an execution provider only metadata queries are available.
    /// Must run before publishing the device. Discovery performs synchronous
    /// device I/O without holding the adapter's spinlock. Malformed or excessive
    /// host metadata fails initialization instead of publishing partial caps.
    pub fn try_new(transport: T) -> DriverResult<Self> {
        Self::initialize(transport, None)
    }

    /// Discover capabilities and install the host's sleepable command runtime.
    /// Initialization uses only unpublished boot-time metadata requests.
    pub fn try_new_with_execution(
        transport: T,
        execution: Arc<dyn GpuExecution>,
    ) -> DriverResult<Self> {
        Self::initialize(transport, Some(execution))
    }

    fn initialize(transport: T, execution: Option<Arc<dyn GpuExecution>>) -> DriverResult<Self> {
        let mut device = VirtIoGpuProtocol::new(transport).map_err(crate::as_driver_error)?;
        let num_capsets = device.num_capsets;
        let (width, height) = device.resolution().map_err(crate::as_driver_error)?;
        if width == 0 || height == 0 {
            return Err(DriverError::Io);
        }
        let has_virgl = device.has_virgl();
        let mut capsets = Vec::new();
        let mut capset_data = Vec::new();
        if has_virgl {
            if num_capsets > MAX_CAPSETS {
                return Err(DriverError::InvalidInput);
            }
            let mut cache_bytes = 0usize;
            for index in 0..num_capsets {
                let cap = device
                    .get_capset_info(index)
                    .map_err(crate::as_driver_error)?;
                // Venus and other protocols have distinct consumers. Do not
                // publish their bytes through this virgl-only contract.
                if !matches!(cap.capset_id, 1 | 2) {
                    continue;
                }
                if cap.capset_max_version > MAX_CAPSET_VERSION
                    || cap.capset_max_size == 0
                    || cap.capset_max_size > MAX_CAPSET_SIZE
                    || capsets
                        .iter()
                        .any(|c: &VirglCapsetInfo| c.id == cap.capset_id)
                {
                    return Err(DriverError::InvalidInput);
                }
                for version in 0..=cap.capset_max_version {
                    cache_bytes = cache_bytes
                        .checked_add(cap.capset_max_size as usize)
                        .filter(|&size| size <= MAX_CAPSET_CACHE_BYTES)
                        .ok_or(DriverError::InvalidInput)?;
                    let bytes = device
                        .get_capset(cap.capset_id, version, cap.capset_max_size)
                        .map_err(crate::as_driver_error)?;
                    if bytes.is_empty() || bytes.len() > cap.capset_max_size as usize {
                        return Err(DriverError::Io);
                    }
                    capset_data.push(CapsetData {
                        id: cap.capset_id,
                        version,
                        bytes: bytes.into(),
                    });
                }
                log::info!(
                    "virtio-gpu: cached capset id={} max_version={} max_size={}",
                    cap.capset_id,
                    cap.capset_max_version,
                    cap.capset_max_size
                );
                capsets.push(VirglCapsetInfo {
                    id: cap.capset_id,
                    max_version: cap.capset_max_version,
                    max_size: cap.capset_max_size,
                });
            }
        }
        log::info!(
            "virtio-gpu: virgl={}, cached_virgl_capsets={}",
            has_virgl,
            capsets.len()
        );
        Ok(Self {
            info: DisplayInfo { width, height },
            has_virgl,
            capsets: capsets.into(),
            capset_data,
            engine: Engine::new(device, execution),
        })
    }
}

impl<H: GpuHal, T: Transport + Send> Device for VirtIoGpuDev<H, T> {
    fn name(&self) -> &str {
        "virtio-gpu"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Display
    }
}

impl<H: GpuHal + 'static, T: Transport + Send + 'static> DisplayDevice for VirtIoGpuDev<H, T> {
    fn info(&self) -> DisplayInfo {
        self.info
    }

    fn need_flush(&self) -> bool {
        false
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }

    fn has_virgl(&self) -> bool {
        self.has_virgl
    }

    fn virgl_capsets(&self) -> Option<Arc<[VirglCapsetInfo]>> {
        self.has_virgl.then(|| self.capsets.clone())
    }

    fn virgl_capset(&self, id: u32, version: u32) -> DriverResult<Arc<[u8]>> {
        if !self.has_virgl {
            return Err(DriverError::Unsupported);
        }
        self.capset_data
            .iter()
            .find(|cap| cap.id == id && cap.version == version)
            .map(|cap| cap.bytes.clone())
            .ok_or(DriverError::InvalidInput)
    }

    fn create_scanout_resource_owned(
        &self,
        resource: ScanoutResource,
        backing: display::GpuBacking,
    ) -> DriverResult {
        if backing.length() == 0
            || resource.id == 0
            || resource.width == 0
            || resource.height == 0
            || resource.width.checked_mul(4) != Some(resource.pitch)
            || u64::from(resource.pitch) * u64::from(resource.height) > u64::from(backing.length())
            || resource.format != display::ScanoutFormat::Bgra8888
        {
            return Err(DriverError::InvalidInput);
        }
        self.engine.create_2d(resource, backing)
    }

    fn destroy_scanout_resource(&self, resource_id: u32) -> DriverResult {
        self.engine.destroy_2d(resource_id)
    }

    fn disable_scanout(&self) -> DriverResult {
        self.engine.disable_scanout()
    }

    fn present_scanout_resource(&self, resource_id: u32, rect: ScanoutRect) -> DriverResult {
        self.engine.present_2d(resource_id, rect)
    }

    fn has_virgl_commands(&self) -> bool {
        self.has_virgl && self.engine.available()
    }

    fn virgl_command(&self, command: VirglCommand<'_>) -> DriverResult {
        if !self.has_virgl {
            return Err(DriverError::Unsupported);
        }
        self.engine.command(command)
    }

    fn enqueue_virgl_cleanup(&self, id: u32) -> DriverResult {
        self.engine.enqueue(id, false)
    }

    fn enqueue_virgl_context_cleanup(&self, id: u32) -> DriverResult {
        self.engine.enqueue(id, true)
    }

    fn flush_virgl_cleanup(&self) -> DriverResult {
        self.engine.reclaim()
    }

    fn virgl_command_status(&self) -> GpuCommandStatus {
        self.engine.status()
    }
}

// The locked dependency's resolution accessor discards the enabled bit.
// Retain the existing local scanout protocol so disabled scanouts (including
// nonzero geometry) cannot be published. Capset discovery shares this queue;
// runtime commands share this queue through the owned command engine.
struct VirtIoGpuProtocol<H: GpuHal, T: Transport + Send> {
    transport: T,
    has_virgl: bool,
    num_capsets: u32,
    control_queue: ManuallyDrop<Queue<H>>,
    cursor_queue: ManuallyDrop<Queue<H>>,
    is_reset: bool,
}

impl<H: GpuHal, T: Transport + Send> VirtIoGpuProtocol<H, T> {
    fn new(mut transport: T) -> virtio_drivers::Result<Self> {
        let negotiated_features = transport.begin_init(SUPPORTED_FEATURES);

        let events_read = read_config!(transport, Config, events_read)?;
        let num_scanouts = read_config!(transport, Config, num_scanouts)?;
        log::info!(
            "virtio-gpu: events_read={:#x}, num_scanouts={:#x}",
            events_read,
            num_scanouts
        );

        let num_capsets = read_config!(transport, Config, num_capsets)?;
        let control_queue = Queue::<H>::new(&mut transport, QUEUE_TRANSMIT)?;
        let cursor_queue = Queue::<H>::new(&mut transport, QUEUE_CURSOR)?;

        transport.finish_init();

        Ok(Self {
            transport,
            has_virgl: negotiated_features.contains(Features::VIRGL),
            num_capsets,
            control_queue: ManuallyDrop::new(control_queue),
            cursor_queue: ManuallyDrop::new(cursor_queue),
            is_reset: false,
        })
    }

    fn resolution(&mut self) -> virtio_drivers::Result<(u32, u32)> {
        let display_info = self.get_display_info()?;
        if display_info.enabled == 0 {
            // Host disabled this scanout: rect is all-zero, which would
            // surface to DRM/fbdev as a bogus 0x0 mode.
            return Err(virtio_drivers::Error::IoError);
        }
        Ok((display_info.rect.width, display_info.rect.height))
    }

    #[allow(dead_code)]
    fn ack_interrupt(&mut self) -> InterruptStatus {
        self.transport.ack_interrupt()
    }

    fn request<Req: IntoBytes + Immutable, Rsp: FromBytes>(
        &mut self,
        req: Req,
    ) -> virtio_drivers::Result<Rsp> {
        // GET_DISPLAY_INFO contains all 16 scanouts although this adapter
        // decodes only the first. Preserve capacity for the complete wire reply.
        let bytes = self.bootstrap_request(req.as_bytes().to_vec(), PAGE_SIZE)?;
        Rsp::read_from_prefix(&bytes)
            .map(|(response, _)| response)
            .map_err(|_| virtio_drivers::Error::IoError)
    }

    fn bootstrap_request(
        &mut self,
        request: Vec<u8>,
        response_size: usize,
    ) -> virtio_drivers::Result<Vec<u8>> {
        let mut transaction = Transaction::<H>::prepare(request, response_size)?;
        self.control_queue.publish(&mut transaction)?;
        self.transport.notify(QUEUE_TRANSMIT);
        loop {
            match self.control_queue.pop() {
                Ok(Some(length)) => {
                    // SAFETY: bootstrap owns this sole published transaction;
                    // pop returned its chain, so DMA no longer owns its maps.
                    return unsafe { transaction.finish(length) };
                }
                Ok(None) => core::hint::spin_loop(),
                Err(error) => {
                    if self.reset() {
                        // SAFETY: acknowledged reset stops DMA on all queues.
                        unsafe { transaction.cancel_after_reset() };
                    }
                    return Err(error);
                }
            }
        }
    }

    fn reset(&mut self) -> bool {
        if !self.is_reset {
            self.transport
                .set_status(virtio_drivers::transport::DeviceStatus::empty());
            self.is_reset =
                self.transport.get_status() == virtio_drivers::transport::DeviceStatus::empty();
        }
        self.is_reset
    }

    fn has_virgl(&self) -> bool {
        self.has_virgl
    }

    fn get_capset_info(&mut self, index: u32) -> virtio_drivers::Result<RespCapsetInfo> {
        let response: RespCapsetInfo = self.request(CapsetQuery {
            header: CtrlHeader::with_type(Command(0x108)),
            id: index,
            version: 0,
        })?;
        response.header.check_type(Command(0x1102))?;
        Ok(response)
    }

    fn get_capset(&mut self, id: u32, version: u32, size: u32) -> virtio_drivers::Result<Vec<u8>> {
        let request = CapsetQuery {
            header: CtrlHeader::with_type(Command(0x109)),
            id,
            version,
        };
        let header_size = core::mem::size_of::<CtrlHeader>();
        let response =
            self.bootstrap_request(request.as_bytes().to_vec(), header_size + size as usize)?;
        let used = response.len();
        if used < header_size {
            return Err(virtio_drivers::Error::IoError);
        }
        let (header, _) =
            CtrlHeader::read_from_prefix(&response).map_err(|_| virtio_drivers::Error::IoError)?;
        header.check_type(Command(0x1103))?;
        let bytes = response[header_size..used].to_vec();
        Ok(bytes)
    }

    fn get_display_info(&mut self) -> virtio_drivers::Result<RespDisplayInfo> {
        let info: RespDisplayInfo =
            self.request(CtrlHeader::with_type(Command::GET_DISPLAY_INFO))?;
        info.header.check_type(Command::OK_DISPLAY_INFO)?;
        Ok(info)
    }
}

impl<H: GpuHal, T: Transport + Send> Drop for VirtIoGpuProtocol<H, T> {
    fn drop(&mut self) {
        if self.reset() {
            self.transport.queue_unset(QUEUE_TRANSMIT);
            self.transport.queue_unset(QUEUE_CURSOR);
            // SAFETY: reset is acknowledged, so neither coherent ring can be
            // accessed by the device. Both fields are dropped exactly once.
            unsafe {
                ManuallyDrop::drop(&mut self.control_queue);
                ManuallyDrop::drop(&mut self.cursor_queue);
            }
        } else {
            log::error!("virtio-gpu: reset unacknowledged; retaining coherent rings");
        }
    }
}

#[repr(C)]
struct Config {
    events_read: ReadOnly<u32>,
    events_clear: WriteOnly<u32>,
    num_scanouts: ReadOnly<u32>,
    num_capsets: ReadOnly<u32>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    struct Features: u64 {
        const VIRGL                 = 1 << 0;
        const EDID                  = 1 << 1;
        const NOTIFY_ON_EMPTY       = 1 << 24;
        const ANY_LAYOUT            = 1 << 27;
        const RING_INDIRECT_DESC    = 1 << 28;
        const RING_EVENT_IDX        = 1 << 29;
        const UNUSED                = 1 << 30;
        const VERSION_1             = 1 << 32;
        const ACCESS_PLATFORM       = 1 << 33;
        const RING_PACKED           = 1 << 34;
        const IN_ORDER              = 1 << 35;
        const ORDER_PLATFORM        = 1 << 36;
        const SR_IOV                = 1 << 37;
        const NOTIFICATION_DATA     = 1 << 38;
    }
}

// The owned GPU queue uses two direct descriptors and polls completion. Do
// not negotiate indirect/event-index features that it does not implement.
const SUPPORTED_FEATURES: Features = Features::VERSION_1.union(Features::VIRGL);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq)]
struct Command(u32);

impl Command {
    const GET_DISPLAY_INFO: Command = Command(0x100);
    const OK_DISPLAY_INFO: Command = Command(0x1101);
}

#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
struct CtrlHeader {
    hdr_type: Command,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    _padding: u32,
}

impl CtrlHeader {
    fn with_type(hdr_type: Command) -> CtrlHeader {
        CtrlHeader {
            hdr_type,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            _padding: 0,
        }
    }

    fn check_type(&self, expected: Command) -> virtio_drivers::Result {
        if self.hdr_type == expected {
            Ok(())
        } else {
            Err(virtio_drivers::Error::IoError)
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, Immutable, KnownLayout)]
struct RespDisplayInfo {
    header: CtrlHeader,
    rect: Rect,
    enabled: u32,
    flags: u32,
}

#[repr(C)]
#[derive(FromBytes, Immutable, IntoBytes, KnownLayout)]
struct CapsetQuery {
    header: CtrlHeader,
    id: u32,
    version: u32,
}

#[repr(C)]
#[derive(FromBytes, Immutable, KnownLayout)]
struct RespCapsetInfo {
    header: CtrlHeader,
    capset_id: u32,
    capset_max_version: u32,
    capset_max_size: u32,
    padding: u32,
}

#[cfg(unittest)]
mod tests;

#[cfg(unittest)]
pub use tests::GpuDmaTestDevice;
