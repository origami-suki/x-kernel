// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO GPU scanout and immutable virgl capability discovery.

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use display::{DisplayDevice, DisplayInfo, ScanoutRect, ScanoutResource, VirglCapsetInfo};
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoIrq;
use virtio_drivers::{
    Hal, PAGE_SIZE,
    config::{ReadOnly, WriteOnly, read_config},
    queue::VirtQueue,
    transport::{InterruptStatus, Transport},
};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

const QUEUE_SIZE: u16 = 2;
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

/// VirtIO GPU adapter with the existing 2D scanout operations.
///
/// Virgl negotiation and capset discovery are exposed as immutable metadata.
/// This adapter does not yet expose 3D contexts, submissions or fences.
pub struct VirtIoGpuDev<H: Hal, T: Transport> {
    info: DisplayInfo,
    has_virgl: bool,
    capsets: Arc<[VirglCapsetInfo]>,
    capset_data: Vec<CapsetData>,
    inner: SpinNoIrq<VirtIoGpuProtocol<H, T>>,
}

// SAFETY: all mutable transport/queue access is serialized by inner. The HAL
// parameter is a type-level provider, and transport ownership moves with the
// device. Capset metadata and bytes are immutable after publication.
unsafe impl<H: Hal, T: Transport> Send for VirtIoGpuDev<H, T> {}
// SAFETY: shared callers only read immutable cached values or acquire inner;
// no reference to the enclosed transport or queue escapes the lock.
unsafe impl<H: Hal, T: Transport> Sync for VirtIoGpuDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoGpuDev<H, T> {
    /// Initializes scanout and caches the device's virgl capability sets.
    ///
    /// Must run before publishing the device. Discovery performs synchronous
    /// device I/O without holding the adapter's spinlock. Malformed or excessive
    /// host metadata fails initialization instead of publishing partial caps.
    pub fn try_new(transport: T) -> DriverResult<Self> {
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
            inner: SpinNoIrq::new(device),
        })
    }
}

impl<H: Hal, T: Transport> Device for VirtIoGpuDev<H, T> {
    fn name(&self) -> &str {
        "virtio-gpu"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Display
    }
}

impl<H: Hal, T: Transport> DisplayDevice for VirtIoGpuDev<H, T> {
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

    fn create_scanout_resource(
        &self,
        resource: ScanoutResource,
        paddr: u64,
        length: u32,
    ) -> DriverResult {
        if length == 0 || resource.pitch == 0 || resource.format != display::ScanoutFormat::Bgra8888
        {
            return Err(DriverError::InvalidInput);
        }
        let mut device = self.inner.lock();
        device
            .resource_create_2d(resource.id, resource.width, resource.height)
            .map_err(crate::as_driver_error)?;
        if let Err(error) = device.resource_attach_backing(resource.id, paddr, length) {
            // Creation already succeeded, so an attach failure must not leave
            // an unreachable host resource behind.
            if let Err(cleanup) = device.resource_unref(resource.id) {
                log::warn!("virtio-gpu: attach rollback failed: {:?}", cleanup);
            }
            return Err(crate::as_driver_error(error));
        }
        Ok(())
    }

    fn destroy_scanout_resource(&self, resource_id: u32) -> DriverResult {
        let mut device = self.inner.lock();
        device
            .resource_detach_backing(resource_id)
            .map_err(crate::as_driver_error)?;
        device
            .resource_unref(resource_id)
            .map_err(crate::as_driver_error)
    }

    fn present_scanout_resource(&self, resource_id: u32, rect: ScanoutRect) -> DriverResult {
        let rect = Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        };
        let mut device = self.inner.lock();
        device
            .transfer_to_host_2d(rect, 0, resource_id)
            .map_err(crate::as_driver_error)?;
        device
            .set_scanout(rect, SCANOUT_ID, resource_id)
            .map_err(crate::as_driver_error)?;
        device
            .resource_flush(rect, resource_id)
            .map_err(crate::as_driver_error)
    }
}

// The locked dependency's resolution accessor discards the enabled bit.
// Retain the existing local scanout protocol so disabled scanouts (including
// nonzero geometry) cannot be published. Capset discovery shares this queue;
// no third-party code is changed and no 3D submission API is exposed.
struct VirtIoGpuProtocol<H: Hal, T: Transport> {
    transport: T,
    has_virgl: bool,
    num_capsets: u32,
    control_queue: VirtQueue<H, { QUEUE_SIZE as usize }>,
    _cursor_queue: VirtQueue<H, { QUEUE_SIZE as usize }>,
    queue_buf_send: Box<[u8]>,
    queue_buf_recv: Box<[u8]>,
}

impl<H: Hal, T: Transport> VirtIoGpuProtocol<H, T> {
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
        let control_queue = VirtQueue::new(
            &mut transport,
            QUEUE_TRANSMIT,
            negotiated_features.contains(Features::RING_INDIRECT_DESC),
            negotiated_features.contains(Features::RING_EVENT_IDX),
            negotiated_features.contains(Features::ACCESS_PLATFORM),
        )?;
        let cursor_queue = VirtQueue::new(
            &mut transport,
            QUEUE_CURSOR,
            negotiated_features.contains(Features::RING_INDIRECT_DESC),
            negotiated_features.contains(Features::RING_EVENT_IDX),
            negotiated_features.contains(Features::ACCESS_PLATFORM),
        )?;

        let queue_buf_send = FromZeros::new_box_zeroed_with_elems(PAGE_SIZE).unwrap();
        let queue_buf_recv = FromZeros::new_box_zeroed_with_elems(PAGE_SIZE).unwrap();

        transport.finish_init();

        Ok(Self {
            transport,
            has_virgl: negotiated_features.contains(Features::VIRGL),
            num_capsets,
            control_queue,
            _cursor_queue: cursor_queue,
            queue_buf_send,
            queue_buf_recv,
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
        // The send buffer is PAGE_SIZE while every request here is a fixed
        // ~32-byte struct, so the conversion cannot fail in practice; still,
        // propagate instead of panicking on malformed device-side data.
        req.write_to_prefix(&mut self.queue_buf_send)
            .map_err(|_| virtio_drivers::Error::IoError)?;
        self.queue_buf_recv.fill(0);
        let used_len = self.control_queue.add_notify_wait_pop(
            &[&self.queue_buf_send],
            &mut [&mut self.queue_buf_recv],
            &mut self.transport,
        )?;
        if (used_len as usize) < core::mem::size_of::<Rsp>() {
            return Err(virtio_drivers::Error::IoError);
        }
        Rsp::read_from_prefix(&self.queue_buf_recv)
            .map(|(rsp, _)| rsp)
            .map_err(|_| virtio_drivers::Error::IoError)
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
        // One writable descriptor preserves the original depth-two queue even
        // when INDIRECT_DESC was not negotiated.
        let mut response = vec![0; header_size + size as usize];
        let used = self.control_queue.add_notify_wait_pop(
            &[request.as_bytes()],
            &mut [&mut response],
            &mut self.transport,
        )? as usize;
        if used < header_size || used > response.len() {
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

    fn resource_create_2d(
        &mut self,
        resource_id: u32,
        width: u32,
        height: u32,
    ) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(ResourceCreate2D {
            header: CtrlHeader::with_type(Command::RESOURCE_CREATE_2D),
            resource_id,
            format: Format::B8G8R8A8UNORM,
            width,
            height,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }

    fn set_scanout(
        &mut self,
        rect: Rect,
        scanout_id: u32,
        resource_id: u32,
    ) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(SetScanout {
            header: CtrlHeader::with_type(Command::SET_SCANOUT),
            rect,
            scanout_id,
            resource_id,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }

    fn resource_flush(&mut self, rect: Rect, resource_id: u32) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(ResourceFlush {
            header: CtrlHeader::with_type(Command::RESOURCE_FLUSH),
            rect,
            resource_id,
            _padding: 0,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }

    fn transfer_to_host_2d(
        &mut self,
        rect: Rect,
        offset: u64,
        resource_id: u32,
    ) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(TransferToHost2D {
            header: CtrlHeader::with_type(Command::TRANSFER_TO_HOST_2D),
            rect,
            offset,
            resource_id,
            _padding: 0,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }

    fn resource_attach_backing(
        &mut self,
        resource_id: u32,
        paddr: u64,
        length: u32,
    ) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(ResourceAttachBacking {
            header: CtrlHeader::with_type(Command::RESOURCE_ATTACH_BACKING),
            resource_id,
            nr_entries: 1,
            addr: paddr,
            length,
            _padding: 0,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }

    fn resource_detach_backing(&mut self, resource_id: u32) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(ResourceDetachBacking {
            header: CtrlHeader::with_type(Command::RESOURCE_DETACH_BACKING),
            resource_id,
            _padding: 0,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }

    fn resource_unref(&mut self, resource_id: u32) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(ResourceUnref {
            header: CtrlHeader::with_type(Command::RESOURCE_UNREF),
            resource_id,
            _padding: 0,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }
}

impl<H: Hal, T: Transport> Drop for VirtIoGpuProtocol<H, T> {
    fn drop(&mut self) {
        self.transport.queue_unset(QUEUE_TRANSMIT);
        self.transport.queue_unset(QUEUE_CURSOR);
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

const SUPPORTED_FEATURES: Features = Features::RING_EVENT_IDX
    .union(Features::RING_INDIRECT_DESC)
    .union(Features::VERSION_1)
    .union(Features::VIRGL);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq)]
struct Command(u32);

impl Command {
    const GET_DISPLAY_INFO: Command = Command(0x100);
    const OK_DISPLAY_INFO: Command = Command(0x1101);
    const OK_NODATA: Command = Command(0x1100);
    const RESOURCE_ATTACH_BACKING: Command = Command(0x106);
    const RESOURCE_CREATE_2D: Command = Command(0x101);
    const RESOURCE_DETACH_BACKING: Command = Command(0x107);
    const RESOURCE_FLUSH: Command = Command(0x104);
    const RESOURCE_UNREF: Command = Command(0x102);
    const SET_SCANOUT: Command = Command(0x103);
    const TRANSFER_TO_HOST_2D: Command = Command(0x105);
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
#[derive(Debug, Immutable, IntoBytes, KnownLayout)]
struct ResourceCreate2D {
    header: CtrlHeader,
    resource_id: u32,
    format: Format,
    width: u32,
    height: u32,
}

#[repr(u32)]
#[derive(Debug, Immutable, IntoBytes, KnownLayout)]
enum Format {
    B8G8R8A8UNORM = 1,
}

#[repr(C)]
#[derive(Debug, Immutable, IntoBytes, KnownLayout)]
struct ResourceAttachBacking {
    header: CtrlHeader,
    resource_id: u32,
    nr_entries: u32,
    addr: u64,
    length: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug, Immutable, IntoBytes, KnownLayout)]
struct ResourceDetachBacking {
    header: CtrlHeader,
    resource_id: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug, Immutable, IntoBytes, KnownLayout)]
struct ResourceUnref {
    header: CtrlHeader,
    resource_id: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug, Immutable, IntoBytes, KnownLayout)]
struct SetScanout {
    header: CtrlHeader,
    rect: Rect,
    scanout_id: u32,
    resource_id: u32,
}

#[repr(C)]
#[derive(Debug, Immutable, IntoBytes, KnownLayout)]
struct TransferToHost2D {
    header: CtrlHeader,
    rect: Rect,
    offset: u64,
    resource_id: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug, Immutable, IntoBytes, KnownLayout)]
struct ResourceFlush {
    header: CtrlHeader,
    rect: Rect,
    resource_id: u32,
    _padding: u32,
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
