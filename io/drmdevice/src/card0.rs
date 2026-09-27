// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/dev/dri/card0` — minimal DRM character device.
//!
//! Single-CRTC, single-connector, single-plane resource-backed DRM/KMS driver.
//! Covers legacy libdrm (`CREATE_DUMB → ADDFB2 → SETCRTC → PAGE_FLIP`) and the
//! atomic-KMS path (`MODE_ATOMIC` + blob properties) used by modern compositors.

use alloc::{
    collections::{BTreeMap, VecDeque},
    format,
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use bytemuck::bytes_of;
use display::{DisplayDevice, ScanoutFormat, ScanoutRect, ScanoutResource};
use kalloc::GlobalPage;
use kclass::{ClassDevice, DisplayDeviceImpl, display_devices as class_display_devices};
use kdevice::subscribe_device_removed;
use khal::mem::v2p;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;
use kvfs::{
    DeviceFileOps, MmapMapper, NodeFlags, VfsError, VfsFile, VfsFileBuilder, VfsInode, VfsResult,
};
use lazyinit::LazyInit;
use memaddr::{PAGE_SIZE_4K, PhysAddrRange};
use posix_types::{UserPtr, UserRead, UserWrite};

use super::{consts::*, drm::*};

#[path = "render.rs"]
mod render;
#[path = "sharing.rs"]
mod sharing;
pub use render::RenderNode;
#[path = "virtgpu.rs"]
mod virtgpu;

static NEXT_RESOURCE_ID: AtomicU32 = AtomicU32::new(0x1000);
static NEXT_CONTEXT_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_OFFSET: AtomicU64 = AtomicU64::new(DUMB_BUFFER_OFFSET_STRIDE);
static REGISTERED_CARDS: LazyInit<Mutex<BTreeMap<kdevice::DeviceId, Arc<Card0>>>> = LazyInit::new();

static DISPLAY_DEVICES: LazyInit<Mutex<Vec<ClassDevice<DisplayDeviceImpl>>>> = LazyInit::new();

fn ensure_scanout_registry() {
    // `call_once` returns `None` for every loser of the initialization race,
    // so concurrent first callers (ioctl paths such as `DrmModeCreateDumb`,
    // `present_fb`, `DrmModeCardRes`) cannot panic a second `init_once` and
    // only the winning thread seeds the registry and subscribes.
    if DISPLAY_DEVICES
        .call_once(|| Mutex::new(Vec::new()))
        .is_none()
    {
        return;
    }
    register_display_devices(class_display_devices());
    kclass::subscribe_display_available(Arc::new(register_display_device));
    subscribe_device_removed(Arc::new(|id| {
        {
            let mut devices = DISPLAY_DEVICES.lock();
            if let Some(pos) = devices.iter().position(|device| device.id() == id) {
                devices.swap_remove(pos);
            }
        }
        let retired = if REGISTERED_CARDS.is_inited() {
            REGISTERED_CARDS.lock().remove(&id)
        } else {
            None
        };
        // Resource/context Drops may queue work, so run outside registry locks.
        drop(retired);
    }));
}

fn register_display_devices(devices: Vec<ClassDevice<DisplayDeviceImpl>>) {
    for device in devices {
        register_display_device(device);
    }
}

fn register_display_device(handle: ClassDevice<DisplayDeviceImpl>) {
    if !DISPLAY_DEVICES.is_inited() {
        return;
    }

    let mut devices = DISPLAY_DEVICES.lock();
    if devices.iter().any(|device| device.id() == handle.id()) {
        return;
    }
    devices.push(handle);
}

pub(crate) fn scanout_available() -> bool {
    ensure_scanout_registry();
    !DISPLAY_DEVICES.lock().is_empty()
}

pub(super) fn primary_scanout() -> VfsResult<ClassDevice<DisplayDeviceImpl>> {
    ensure_scanout_registry();
    DISPLAY_DEVICES
        .lock()
        .first()
        .cloned()
        .ok_or(VfsError::NoSuchDevice)
}

/// DRM ioctl handler trait. `const CMD` is injected by the `#[drm_ioctl]` attribute macro.
trait DrmIoctl: Sized {
    const CMD: u32;
    fn handle(dev: &Card0, file: &DrmFile, arg: &mut Self) -> VfsResult<usize>;

    fn handle_raw(dev: &Card0, file: &DrmFile, arg: UserPtr<Self>) -> VfsResult<usize>
    where
        Self: UserRead + UserWrite,
    {
        let mut ptr: Self = arg.read_vm().map_err(|_| kvfs::VfsError::BadAddress)?;
        let result = Self::handle(dev, file, &mut ptr);
        arg.write_vm(ptr).map_err(|_| kvfs::VfsError::BadAddress)?;
        result
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
struct DrmAuthMagic(DrmAuth);

#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
struct DrmPrimeFdToHandle(DrmPrimeHandle);

#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
struct DrmModeSetCrtc(DrmModeCrtc);

#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
struct DrmModeRmFb(u32);

/// VERSION allows length-only queries and truncates each string to the
/// supplied capacity. Lengths report the full value, without a trailing NUL.
/// This follows Linux drm_copy_field, including NULL with nonzero capacity.
fn copy_version_field(ptr: UserPtr<u8>, capacity: &mut usize, value: &[u8]) -> VfsResult<()> {
    let count = (*capacity).min(value.len());
    *capacity = value.len();
    if count != 0 && !ptr.is_null() {
        ptr.write_vm_slice(&value[..count])
            .map_err(|_| VfsError::BadAddress)?;
    }
    Ok(())
}

impl DrmIoctl for DrmVersion {
    const CMD: u32 = iowr::<DrmVersion>(DRM_TYPE, 0x00);

    fn handle(dev: &Card0, _file: &DrmFile, version: &mut Self) -> VfsResult<usize> {
        (
            version.version_major,
            version.version_minor,
            version.version_patchlevel,
        ) = dev.driver_identity.version();
        copy_version_field(
            version.name,
            &mut version.name_len,
            dev.driver_identity.name().as_bytes(),
        )?;
        copy_version_field(
            version.date,
            &mut version.date_len,
            dev.driver_identity.date().as_bytes(),
        )?;
        copy_version_field(
            version.desc,
            &mut version.desc_len,
            dev.driver_identity.description().as_bytes(),
        )?;
        Ok(0)
    }
}

impl DrmIoctl for DrmSetVersion {
    const CMD: u32 = iowr::<DrmSetVersion>(DRM_TYPE, 0x07);

    fn handle(dev: &Card0, _file: &DrmFile, set_version: &mut Self) -> VfsResult<usize> {
        if set_version.drm_di_major < 0 {
            set_version.drm_di_major = 1;
        }
        if set_version.drm_di_minor < 0 {
            set_version.drm_di_minor = 4;
        }
        let (major, minor, _) = dev.driver_identity.version();
        set_version.drm_dd_major = major;
        set_version.drm_dd_minor = minor;
        Ok(0)
    }
}

impl DrmIoctl for DrmUnique {
    const CMD: u32 = iowr::<DrmUnique>(DRM_TYPE, 0x01);

    fn handle(_dev: &Card0, _file: &DrmFile, unique: &mut Self) -> VfsResult<usize> {
        let unique_str: String = match primary_scanout().map(|device| device.location()) {
            Ok(kdevice::DeviceLocation::Pci {
                segment,
                bus,
                device,
                function,
            }) => format!("pci:{segment:04x}:{bus:02x}:{device:02x}.{function}"),
            _ => format!("{}:0", DRIVER_NAME),
        };
        // Unlike VERSION, Linux GET_UNIQUE copies only when the whole value
        // fits. A short buffer is a size query and must remain untouched.
        if unique.unique_len >= unique_str.len() {
            unique
                .unique
                .write_vm_slice(unique_str.as_bytes())
                .map_err(|_| VfsError::BadAddress)?;
        }
        unique.unique_len = unique_str.len();
        Ok(0)
    }
}

impl DrmIoctl for DrmGetCap {
    const CMD: u32 = iowr::<DrmGetCap>(DRM_TYPE, 0x0c);

    fn handle(_dev: &Card0, _file: &DrmFile, cap: &mut Self) -> VfsResult<usize> {
        cap.value = match cap.capability {
            DRM_CAP_DUMB_BUFFER => 1,
            DRM_CAP_PRIME => 3,
            DRM_CAP_TIMESTAMP_MONOTONIC => 1,
            DRM_CAP_CRTC_IN_VBLANK_EVENT => 1,
            DRM_CAP_ADDFB2_MODIFIERS => 1,
            _ => 0,
        };
        Ok(0)
    }
}

impl DrmIoctl for DrmSetClientCap {
    const CMD: u32 = iow::<DrmSetClientCap>(DRM_TYPE, 0x0d);

    fn handle(_dev: &Card0, file: &DrmFile, cap: &mut Self) -> VfsResult<usize> {
        if !matches!(
            cap.capability,
            DRM_CLIENT_CAP_UNIVERSAL_PLANES | DRM_CLIENT_CAP_ATOMIC
        ) || cap.value > 1
        {
            return Err(VfsError::InvalidInput);
        }
        file.client_caps.lock().insert(cap.capability, cap.value);
        Ok(0)
    }
}

impl DrmIoctl for DrmAuth {
    const CMD: u32 = iowr::<DrmAuth>(DRM_TYPE, 0x02);

    fn handle(_dev: &Card0, _file: &DrmFile, auth: &mut Self) -> VfsResult<usize> {
        auth.magic = 1;
        Ok(0)
    }
}

impl DrmIoctl for DrmAuthMagic {
    const CMD: u32 = iowr::<DrmAuth>(DRM_TYPE, 0x03);

    fn handle(_dev: &Card0, _file: &DrmFile, _auth: &mut Self) -> VfsResult<usize> {
        Ok(0)
    }
}

impl DrmIoctl for DrmModeDirtyFB {
    const CMD: u32 = iowr::<DrmModeDirtyFB>(DRM_TYPE, 0xB1);

    fn handle(dev: &Card0, _file: &DrmFile, dirty: &mut Self) -> VfsResult<usize> {
        let _kms_reservation = dev.kms_reservation.lock();
        // Linux's drm_mode_dirtyfb_ioctl() rejects an unknown fb_id with
        // -EINVAL; validate the framebuffer before attempting scanout.
        if !dev.fbs.lock().contains_key(&dirty.fb_id) {
            return Err(VfsError::InvalidInput);
        }
        dev.present_fb(dirty.fb_id)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmPrimeHandle {
    const CMD: u32 = iowr::<DrmPrimeHandle>(DRM_TYPE, 0x2d);

    fn handle_raw(dev: &Card0, file: &DrmFile, arg: UserPtr<Self>) -> VfsResult<usize> {
        sharing::export_raw(dev, file, arg)
    }

    fn handle(_dev: &Card0, _file: &DrmFile, _arg: &mut Self) -> VfsResult<usize> {
        Err(VfsError::OperationNotSupported)
    }
}

impl DrmIoctl for DrmPrimeFdToHandle {
    const CMD: u32 = iowr::<DrmPrimeHandle>(DRM_TYPE, 0x2e);

    fn handle_raw(dev: &Card0, file: &DrmFile, arg: UserPtr<Self>) -> VfsResult<usize> {
        sharing::import_raw(dev, file, arg)
    }

    fn handle(_dev: &Card0, _file: &DrmFile, _arg: &mut Self) -> VfsResult<usize> {
        Err(VfsError::OperationNotSupported)
    }
}

impl DrmIoctl for DrmModeCardRes {
    const CMD: u32 = iowr::<DrmModeCardRes>(DRM_TYPE, 0xA0);

    fn handle(_dev: &Card0, _file: &DrmFile, r: &mut Self) -> VfsResult<usize> {
        let (w, h) = display_resolution();
        r.min_width = w;
        r.max_width = w;
        r.min_height = h;
        r.max_height = h;

        r.count_fbs = 0;
        r.count_crtcs = report_user_array(r.crtc_id_ptr, r.count_crtcs, &[CRTC_ID])?;
        r.count_encoders = report_user_array(r.encoder_id_ptr, r.count_encoders, &[ENCODER_ID])?;
        r.count_connectors =
            report_user_array(r.connector_id_ptr, r.count_connectors, &[CONNECTOR_ID])?;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCrtc {
    const CMD: u32 = iowr::<DrmModeCrtc>(DRM_TYPE, 0xA1);

    fn handle(dev: &Card0, _file: &DrmFile, crtc: &mut Self) -> VfsResult<usize> {
        if crtc.crtc_id != CRTC_ID {
            return Err(VfsError::InvalidInput);
        }
        let state = *dev.state.lock();
        let legacy = dev.legacy_crtc.lock().clone();
        crtc.gamma_size = 0;
        crtc.x = (state.plane_src_x >> 16) as u32;
        crtc.y = (state.plane_src_y >> 16) as u32;
        crtc.fb_id = if state.crtc_active != 0 {
            state.plane_fb_id
        } else {
            0
        };
        crtc.mode_valid = u32::from(state.crtc_active != 0 && crtc.fb_id != 0);
        crtc.mode = if crtc.mode_valid != 0 {
            if state.crtc_mode_id != 0 {
                let bytes = dev
                    .mode_id_blob_ref
                    .lock()
                    .clone()
                    .ok_or(VfsError::InvalidInput)?;
                bytemuck::try_pod_read_unaligned::<DrmModeModeInfo>(&bytes)
                    .map_err(|_| VfsError::InvalidInput)?
            } else if legacy.mode_valid != 0 {
                legacy.mode
            } else {
                current_mode()
            }
        } else {
            DrmModeModeInfo::default()
        };
        let connectors = if state.conn_crtc_id != 0 && state.crtc_active != 0 {
            &[CONNECTOR_ID][..]
        } else {
            &[][..]
        };
        crtc.count_connectors =
            report_user_array(crtc.set_connectors_ptr, crtc.count_connectors, connectors)?;
        Ok(0)
    }
}

#[allow(dead_code)]
struct DumbBuffer {
    width: u32,
    height: u32,
    bpp: u32,
    pitch: u32,
    size: u64,
    offset: u64,
    resource_id: u32,
    device: Option<ClassDevice<DisplayDeviceImpl>>,
    pages: Arc<GlobalPage>,
    reservation: Mutex<()>,
    completion_poll: PollSet,
    exported: Mutex<Weak<VfsFile>>,
    virgl: Option<virtgpu::VirglObject>,
}

#[derive(Clone)]
struct Framebuffer {
    fb_id: u32,
    owner: Weak<DrmFile>,
    pending_owner_close: bool,
    object: Arc<DumbBuffer>,
    width: u32,
    height: u32,
}

#[derive(Debug, Default, Clone)]
struct LegacyCrtcState {
    fb_id: u32,
    mode: DrmModeModeInfo,
    mode_valid: u32,
}

#[derive(Debug, Default, Clone, Copy)]
struct ModesetState {
    crtc_active: u64,
    crtc_mode_id: u32,
    conn_crtc_id: u32,
    plane_fb_id: u32,
    plane_crtc_id: u32,
    plane_src_x: u64,
    plane_src_y: u64,
    plane_src_w: u64,
    plane_src_h: u64,
    plane_crtc_x: i64,
    plane_crtc_y: i64,
    plane_crtc_w: u64,
    plane_crtc_h: u64,
}

/// State attached to one open file description; dup/fork share this Arc.
struct DrmFile {
    events: Mutex<VecDeque<DrmEventVblank>>,
    poll_rx: PollSet,
    dumbs: Mutex<sharing::GemHandles>,
    next_dumb_handle: AtomicU32,
    client_caps: Mutex<BTreeMap<u64, u64>>,
    context: Mutex<Option<Arc<virtgpu::VirglContext>>>,
    prime_publication: Mutex<()>,
    owned_fbs: Mutex<alloc::collections::BTreeSet<u32>>,
    self_ref: Weak<DrmFile>,
}

impl DrmFile {
    fn new() -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            events: Mutex::new(VecDeque::with_capacity(MAX_EVENTS)),
            poll_rx: PollSet::new(),
            dumbs: Mutex::new(sharing::GemHandles::default()),
            next_dumb_handle: AtomicU32::new(FIRST_DUMB_HANDLE),
            client_caps: Mutex::new(BTreeMap::new()),
            context: Mutex::new(None),
            prime_publication: Mutex::new(()),
            owned_fbs: Mutex::new(alloc::collections::BTreeSet::new()),
            self_ref: self_ref.clone(),
        })
    }

    fn from_file(file: &VfsFile) -> VfsResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(VfsError::InvalidInput)
    }

    fn close_handle(&self, handle: u32) -> VfsResult<()> {
        let _publication = self.prime_publication.lock();
        self.close_handle_unpublished(handle)
    }

    fn close_handle_unpublished(&self, handle: u32) -> VfsResult<()> {
        self.dumbs
            .lock()
            .remove(&handle)
            .ok_or(VfsError::InvalidInput)?;
        // The final object reference queues owned fenced cleanup. Host DMA
        // allocation ownership survives deferred cleanup errors; no waits here.
        Ok(())
    }
}

/// The DRM character device implementing `/dev/dri/card0`.
pub struct Card0 {
    sequence: AtomicU32,
    state: Mutex<ModesetState>,
    legacy_crtc: Mutex<LegacyCrtcState>,
    next_offset: &'static AtomicU64,
    fbs: Mutex<BTreeMap<u32, Framebuffer>>,
    active_scanout: Mutex<Option<Framebuffer>>,
    next_fb_id: AtomicU32,
    next_resource_id: &'static AtomicU32,
    next_context_id: &'static AtomicU32,
    driver_identity: super::discovery::DriverIdentity,
    context_admission: Mutex<()>,
    kms_reservation: Mutex<()>,
    fb_cleanup_work: kwork::ScheduledWork,
    blobs: Mutex<BTreeMap<u32, Arc<Vec<u8>>>>,
    mode_id_blob_ref: Mutex<Option<Arc<Vec<u8>>>>,
    next_blob_id: AtomicU32,
    system_blobs: Mutex<BTreeMap<u32, Arc<Vec<u8>>>>,
    in_formats_blob: AtomicU32,
    system_blobs_init: Mutex<()>,
}

impl Card0 {
    /// Create a new DRM device instance.
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|self_ref: &Weak<Self>| Self {
            sequence: AtomicU32::new(0),
            state: Mutex::new(ModesetState::default()),
            legacy_crtc: Mutex::new(LegacyCrtcState::default()),
            next_offset: &NEXT_OFFSET,
            fbs: Mutex::new(BTreeMap::new()),
            active_scanout: Mutex::new(None),
            next_fb_id: AtomicU32::new(FIRST_FB_ID),
            next_resource_id: &NEXT_RESOURCE_ID,
            next_context_id: &NEXT_CONTEXT_ID,
            driver_identity: super::discovery::DriverIdentity::detected(),
            context_admission: Mutex::new(()),
            kms_reservation: Mutex::new(()),
            fb_cleanup_work: {
                let card = self_ref.clone();
                kwork::ScheduledWork::new(move |_| {
                    if let Some(card) = card.upgrade() {
                        card.drain_closed_framebuffers();
                    }
                })
            },
            blobs: Mutex::new(BTreeMap::new()),
            mode_id_blob_ref: Mutex::new(None),
            next_blob_id: AtomicU32::new(FIRST_BLOB_ID),
            system_blobs: Mutex::new(BTreeMap::new()),
            in_formats_blob: AtomicU32::new(0),
            system_blobs_init: Mutex::new(()),
        })
    }

    /// Returns the physical primary device's shared DRM adapter across mounts.
    ///
    /// Primary and render nodes and separate devfs mounts use one KMS namespace.
    /// The registry owns it until device removal, when references are dropped
    /// outside registry locks. GEM/host owners do not point back at this registry.
    /// # Errors
    /// Returns ENODEV when no display is registered.
    pub fn registered() -> VfsResult<Arc<Self>> {
        let id = primary_scanout()?.id();
        REGISTERED_CARDS.call_once(|| Mutex::new(BTreeMap::new()));
        if let Some(card) = REGISTERED_CARDS.lock().get(&id).cloned() {
            return Ok(card);
        }
        let candidate = Self::new();
        Ok(REGISTERED_CARDS
            .lock()
            .entry(id)
            .or_insert(candidate)
            .clone())
    }

    fn ensure_in_formats_blob(&self) -> u32 {
        let cur = self.in_formats_blob.load(Ordering::Acquire);
        if cur != 0 {
            return cur;
        }
        let _guard = self.system_blobs_init.lock();
        let cur = self.in_formats_blob.load(Ordering::Acquire);
        if cur != 0 {
            return cur;
        }
        let bytes = build_in_formats_blob();
        let id = self.next_blob_id.fetch_add(1, Ordering::Relaxed);
        self.system_blobs.lock().insert(id, Arc::new(bytes));
        self.in_formats_blob.store(id, Ordering::Release);
        id
    }
}

fn report_user_array<T: UserWrite + Copy>(
    user_ptr: UserPtr<T>,
    cap: u32,
    src: &[T],
) -> VfsResult<u32> {
    if !user_ptr.is_null() {
        let to_write = (cap as usize).min(src.len());
        user_ptr
            .write_vm_slice(&src[..to_write])
            .map_err(|_| VfsError::BadAddress)?;
    }
    Ok(src.len() as u32)
}

fn display_resolution() -> (u32, u32) {
    primary_scanout()
        .map(|device| {
            let info = device.info();
            (info.width, info.height)
        })
        .unwrap_or((640, 480))
}

fn current_mode() -> DrmModeModeInfo {
    let (w, h) = display_resolution();
    let mut name = [0u8; 32];
    let s = b"current";
    name[..s.len()].copy_from_slice(s);

    let hdisplay = w as u16;
    let hsync_start = hdisplay + CVT_RB_HFRONT_PORCH;
    let hsync_end = hsync_start + CVT_RB_HSYNC_WIDTH;
    let htotal = hsync_end + CVT_RB_HBACK_PORCH;

    let vdisplay = h as u16;
    let vsync_start = vdisplay + CVT_RB_VFRONT_PORCH;
    let vsync_end = vsync_start + CVT_RB_VSYNC_WIDTH;
    let vtotal = vsync_end + CVT_RB_VBACK_PORCH;

    let vrefresh: u32 = DEFAULT_VREFRESH;
    let clock = ((htotal as u32) * (vtotal as u32) * vrefresh) / 1000;

    DrmModeModeInfo {
        clock,
        hdisplay,
        hsync_start,
        hsync_end,
        htotal,
        hskew: 0,
        vdisplay,
        vsync_start,
        vsync_end,
        vtotal,
        vscan: 0,
        vrefresh,
        flags: 0,
        kind: 0,
        name,
    }
}

impl DeviceFileOps for Card0 {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        file.set_private_data(DrmFile::new());
        Ok(())
    }

    fn release(&self, _inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        let state = DrmFile::from_file(file)?;
        let retirement = self.queue_owned_framebuffers(&state);
        state.dumbs.lock().clear();
        state.events.lock().clear();
        state.client_caps.lock().clear();
        state.context.lock().take();
        retirement
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let evsz = core::mem::size_of::<DrmEventVblank>();
        if buf.len() < evsz {
            return Err(VfsError::InvalidInput);
        }
        let state = DrmFile::from_file(file)?;
        let mut events = state.events.lock();
        let mut written = 0;
        while written + evsz <= buf.len() {
            let Some(ev) = events.pop_front() else {
                break;
            };
            buf[written..written + evsz].copy_from_slice(bytes_of(&ev));
            written += evsz;
        }
        if written == 0 {
            Err(VfsError::WouldBlock)
        } else {
            Ok(written)
        }
    }

    fn ioctl(&self, file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        let drm_file = DrmFile::from_file(file)?;
        if let Some(result) = virtgpu::dispatch(self, &drm_file, cmd, arg) {
            return result;
        }
        match cmd {
            DRM_IOCTL_SET_MASTER | DRM_IOCTL_DROP_MASTER => Ok(0),
            <DrmVersion as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmVersion>::from(arg);
                <DrmVersion as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmUnique as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmUnique>::from(arg);
                <DrmUnique as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmSetVersion as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmSetVersion>::from(arg);
                <DrmSetVersion as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmGetCap as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmGetCap>::from(arg);
                <DrmGetCap as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmSetClientCap as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmSetClientCap>::from(arg);
                <DrmSetClientCap as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmAuth as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmAuth>::from(arg);
                <DrmAuth as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmAuthMagic as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmAuthMagic>::from(arg);
                <DrmAuthMagic as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeDirtyFB as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeDirtyFB>::from(arg);
                <DrmModeDirtyFB as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmPrimeHandle as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmPrimeHandle>::from(arg);
                <DrmPrimeHandle as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmPrimeFdToHandle as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmPrimeFdToHandle>::from(arg);
                <DrmPrimeFdToHandle as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeCardRes as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeCardRes>::from(arg);
                <DrmModeCardRes as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeCrtc as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeCrtc>::from(arg);
                <DrmModeCrtc as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeSetCrtc as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeSetCrtc>::from(arg);
                <DrmModeSetCrtc as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeGetEncoder as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeGetEncoder>::from(arg);
                <DrmModeGetEncoder as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeGetConnector as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeGetConnector>::from(arg);
                <DrmModeGetConnector as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeRmFb as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeRmFb>::from(arg);
                <DrmModeRmFb as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeCrtcPageFlip as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeCrtcPageFlip>::from(arg);
                <DrmModeCrtcPageFlip as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeCreateDumb as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeCreateDumb>::from(arg);
                <DrmModeCreateDumb as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeMapDumb as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeMapDumb>::from(arg);
                <DrmModeMapDumb as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmGemClose as DrmIoctl>::CMD => {
                <DrmGemClose as DrmIoctl>::handle_raw(self, &drm_file, UserPtr::from(arg))
            }
            <DrmModeDestroyDumb as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeDestroyDumb>::from(arg);
                <DrmModeDestroyDumb as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeGetPlaneRes as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeGetPlaneRes>::from(arg);
                <DrmModeGetPlaneRes as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeGetPlane as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeGetPlane>::from(arg);
                <DrmModeGetPlane as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeObjGetProperties as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeObjGetProperties>::from(arg);
                <DrmModeObjGetProperties as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeGetProperty as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeGetProperty>::from(arg);
                <DrmModeGetProperty as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmWaitVblank as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmWaitVblank>::from(arg);
                <DrmWaitVblank as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeAtomic as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeAtomic>::from(arg);
                <DrmModeAtomic as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeCreateBlob as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeCreateBlob>::from(arg);
                <DrmModeCreateBlob as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeDestroyBlob as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeDestroyBlob>::from(arg);
                <DrmModeDestroyBlob as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeGetBlob as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeGetBlob>::from(arg);
                <DrmModeGetBlob as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            <DrmModeFbCmd2 as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeFbCmd2>::from(arg);
                <DrmModeFbCmd2 as DrmIoctl>::handle_raw(self, &drm_file, user_ptr)
            }
            _ => Err(kvfs::VfsError::OperationNotSupported),
        }
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        let offset = mapper.offset() as u64;
        let state = DrmFile::from_file(file)?;
        let object = state
            .dumbs
            .lock()
            .values()
            .find(|b| b.offset == offset)
            .cloned();
        let Some(b) = object else {
            return Err(VfsError::InvalidInput);
        };
        let phys = v2p(b.pages.start_va());
        let size = b.pages.size();
        let range = PhysAddrRange::from_start_size(phys, size);
        mapper.map_physical_owned(range, 0, b)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        DrmFile::from_file(file).map_or(IoEvents::ERR, |state| Pollable::poll(state.as_ref()))
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Some(state) = file.private_data_get::<DrmFile>() {
            Pollable::register(state.as_ref(), context, events)?;
        }
        Ok(())
    }
}

impl Pollable for DrmFile {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, !self.events.lock().is_empty());
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.poll_rx)?;
        }
        Ok(())
    }
}

impl DrmIoctl for DrmModeSetCrtc {
    const CMD: u32 = iowr::<DrmModeCrtc>(DRM_TYPE, 0xA2);

    fn handle(dev: &Card0, _file: &DrmFile, crtc: &mut Self) -> VfsResult<usize> {
        let _kms_reservation = dev.kms_reservation.lock();
        let c = &crtc.0;
        if c.crtc_id != CRTC_ID {
            return Err(VfsError::InvalidInput);
        }
        if c.fb_id == 0 && c.count_connectors == 0 {
            dev.disable_current_scanout()?;
            *dev.legacy_crtc.lock() = LegacyCrtcState::default();
            *dev.state.lock() = ModesetState::default();
            *dev.mode_id_blob_ref.lock() = None;
            return Ok(0);
        }
        if c.fb_id == 0 || !dev.fbs.lock().contains_key(&c.fb_id) {
            return Err(VfsError::InvalidInput);
        }
        if c.count_connectors == 0 || c.set_connectors_ptr.is_null() {
            return Err(VfsError::InvalidInput);
        }
        if c.count_connectors > 16 {
            return Err(VfsError::InvalidInput);
        }
        let connectors: Vec<u32> = c
            .set_connectors_ptr
            .load_vm_vec(c.count_connectors as usize)?;
        for &id in &connectors {
            if id != CONNECTOR_ID {
                return Err(VfsError::InvalidInput);
            }
        }
        let proposed = LegacyCrtcState {
            fb_id: c.fb_id,
            mode: c.mode,
            mode_valid: c.mode_valid,
        };
        dev.present_fb(c.fb_id)?;
        *dev.legacy_crtc.lock() = proposed;
        let fb = dev
            .fbs
            .lock()
            .get(&c.fb_id)
            .cloned()
            .ok_or(VfsError::InvalidInput)?;
        *dev.state.lock() = ModesetState {
            crtc_active: 1,
            plane_fb_id: c.fb_id,
            plane_crtc_id: CRTC_ID,
            conn_crtc_id: CRTC_ID,
            plane_src_x: u64::from(c.x) << 16,
            plane_src_y: u64::from(c.y) << 16,
            plane_src_w: u64::from(fb.width) << 16,
            plane_src_h: u64::from(fb.height) << 16,
            plane_crtc_w: u64::from(fb.width),
            plane_crtc_h: u64::from(fb.height),
            ..ModesetState::default()
        };
        *dev.mode_id_blob_ref.lock() = None;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetEncoder {
    const CMD: u32 = iowr::<DrmModeGetEncoder>(DRM_TYPE, 0xA6);

    fn handle(_dev: &Card0, _file: &DrmFile, e: &mut Self) -> VfsResult<usize> {
        if e.encoder_id != ENCODER_ID {
            return Err(VfsError::InvalidInput);
        }
        e.encoder_type = DRM_MODE_ENCODER_VIRTUAL;
        e.crtc_id = CRTC_ID;
        e.possible_crtcs = 1;
        e.possible_clones = 0;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetConnector {
    const CMD: u32 = iowr::<DrmModeGetConnector>(DRM_TYPE, 0xA7);

    fn handle(_dev: &Card0, _file: &DrmFile, c: &mut Self) -> VfsResult<usize> {
        if c.connector_id != CONNECTOR_ID {
            return Err(VfsError::InvalidInput);
        }
        c.encoder_id = ENCODER_ID;
        c.connector_type = DRM_MODE_CONNECTOR_VIRTUAL;
        c.connector_type_id = 1;
        c.connection = DRM_MODE_CONNECTED;
        let (w, h) = display_resolution();
        c.mm_width = w;
        c.mm_height = h;
        c.subpixel = 0;
        c.count_encoders = report_user_array(c.encoders_ptr, c.count_encoders, &[ENCODER_ID])?;
        if !c.modes_ptr.is_null() && c.count_modes > 0 {
            c.modes_ptr
                .write_vm(current_mode())
                .map_err(|_| VfsError::BadAddress)?;
        }
        c.count_modes = 1;
        c.count_props = 0;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeRmFb {
    const CMD: u32 = iowr::<u32>(DRM_TYPE, 0xAF);

    fn handle(dev: &Card0, _file: &DrmFile, fb_id: &mut Self) -> VfsResult<usize> {
        let _kms_reservation = dev.kms_reservation.lock();
        dev.retire_framebuffer(fb_id.0)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCrtcPageFlip {
    const CMD: u32 = iowr::<DrmModeCrtcPageFlip>(DRM_TYPE, 0xB0);

    fn handle(dev: &Card0, file: &DrmFile, f: &mut Self) -> VfsResult<usize> {
        let _kms_reservation = dev.kms_reservation.lock();
        if f.crtc_id != CRTC_ID || !dev.fbs.lock().contains_key(&f.fb_id) {
            return Err(VfsError::InvalidInput);
        }
        dev.present_fb(f.fb_id)?;
        dev.legacy_crtc.lock().fb_id = f.fb_id;
        dev.state.lock().plane_fb_id = f.fb_id;
        if f.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            dev.queue_flip_event(file, f.user_data);
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCreateDumb {
    const CMD: u32 = iowr::<DrmModeCreateDumb>(DRM_TYPE, 0xB2);

    fn handle_raw(dev: &Card0, file: &DrmFile, arg: UserPtr<Self>) -> VfsResult<usize> {
        let mut request = arg.read_vm().map_err(|_| VfsError::BadAddress)?;
        Self::handle(dev, file, &mut request)?;
        if arg.write_vm(request).is_err() {
            let _ = file.close_handle(request.handle);
            return Err(VfsError::BadAddress);
        }
        Ok(0)
    }

    fn handle(dev: &Card0, file: &DrmFile, c: &mut Self) -> VfsResult<usize> {
        // Only BGRA8888 (32bpp) is supported, so the bpp == 0 / bpp > 64 /
        // multiple-of-8 checks are subsumed by the bpp != 32 comparison and
        // bytes_per_pixel is constant.
        if c.width == 0 || c.height == 0 || c.bpp != 32 || c.flags != 0 {
            return Err(VfsError::InvalidInput);
        }
        if c.width > 16384 || c.height > 16384 {
            return Err(VfsError::InvalidInput);
        }
        let bytes_per_pixel = 4;
        let pitch = c
            .width
            .checked_mul(bytes_per_pixel)
            .ok_or(VfsError::InvalidInput)?;
        let size = (pitch as u64)
            .checked_mul(c.height as u64)
            .ok_or(VfsError::InvalidInput)?;
        if size as usize > DUMB_BUFFER_MAX_SIZE {
            return Err(VfsError::NoMemory);
        }
        c.pitch = pitch;
        c.size = size;
        let size_aligned = (size as usize).next_multiple_of(PAGE_SIZE_4K);
        c.size = size_aligned as u64;
        let pages = size_aligned / PAGE_SIZE_4K;
        let mut backing =
            GlobalPage::alloc_contiguous(pages, PAGE_SIZE_4K).map_err(|_| VfsError::NoMemory)?;
        backing.zero();
        let device = primary_scanout()?;
        let pages_arc = Arc::new(backing);
        let offset = dev
            .next_offset
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |offset| {
                offset.checked_add(DUMB_BUFFER_OFFSET_STRIDE)
            })
            .map_err(|_| VfsError::NoMemory)?;
        let resource_id = virtgpu::next_id(dev.next_resource_id)?;
        let handle = virtgpu::next_id(&file.next_dumb_handle)?;
        let mut object = Arc::new(DumbBuffer {
            width: c.width,
            height: c.height,
            bpp: c.bpp,
            pitch: c.pitch,
            size: c.size,
            offset,
            resource_id,
            device: None,
            pages: pages_arc.clone(),
            reservation: Mutex::new(()),
            completion_poll: PollSet::new(),
            exported: Mutex::new(Weak::new()),
            virgl: None,
        });
        // SAFETY: This allocation-only Arc pins the entire contiguous coherent
        // guest RAM range; it cannot form a cycle back to the display device.
        let backing = unsafe {
            display::GpuBacking::new(
                v2p(pages_arc.start_va()).as_usize() as u64,
                size_aligned as u32,
                pages_arc,
            )
        };
        device
            .create_scanout_resource_owned(
                ScanoutResource {
                    id: resource_id,
                    width: c.width,
                    height: c.height,
                    pitch: c.pitch,
                    format: ScanoutFormat::Bgra8888,
                },
                backing,
            )
            .map_err(virtgpu::driver_error)?;
        Arc::get_mut(&mut object)
            .expect("new dumb object is unpublished")
            .device = Some(device);
        file.dumbs.lock().insert(handle, object);
        c.handle = handle;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeMapDumb {
    const CMD: u32 = iowr::<DrmModeMapDumb>(DRM_TYPE, 0xB3);

    fn handle(_dev: &Card0, file: &DrmFile, m: &mut Self) -> VfsResult<usize> {
        let offset = file
            .dumbs
            .lock()
            .get(&m.handle)
            .map(|b| b.offset)
            .ok_or(VfsError::InvalidInput)?;
        m.offset = offset;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeDestroyDumb {
    const CMD: u32 = iowr::<DrmModeDestroyDumb>(DRM_TYPE, 0xB4);

    fn handle(_dev: &Card0, file: &DrmFile, d: &mut Self) -> VfsResult<usize> {
        file.close_handle(d.handle)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetPlaneRes {
    const CMD: u32 = iowr::<DrmModeGetPlaneRes>(DRM_TYPE, 0xB5);

    fn handle(_dev: &Card0, _file: &DrmFile, r: &mut Self) -> VfsResult<usize> {
        let planes: &[u32] = &[PLANE_ID];
        r.count_planes = report_user_array(r.plane_id_ptr, r.count_planes, planes)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetPlane {
    const CMD: u32 = iowr::<DrmModeGetPlane>(DRM_TYPE, 0xB6);

    fn handle(_dev: &Card0, _file: &DrmFile, p: &mut Self) -> VfsResult<usize> {
        if p.plane_id != PLANE_ID {
            return Err(VfsError::InvalidInput);
        }
        p.crtc_id = CRTC_ID;
        p.fb_id = 0;
        p.possible_crtcs = 1;
        p.gamma_size = 0;
        p.count_format_types =
            report_user_array(p.format_type_ptr, p.count_format_types, SUPPORTED_FORMATS)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeObjGetProperties {
    const CMD: u32 = iowr::<DrmModeObjGetProperties>(DRM_TYPE, 0xB9);

    fn handle(dev: &Card0, _file: &DrmFile, q: &mut Self) -> VfsResult<usize> {
        let state = *dev.state.lock();
        let (prop_ids, prop_vals): (&[u32], Vec<u64>) = match (q.obj_type, q.obj_id) {
            (DRM_MODE_OBJECT_PLANE, PLANE_ID) => {
                let blob_id = dev.ensure_in_formats_blob() as u64;
                (PLANE_PROPS, plane_prop_values(&state, blob_id))
            }
            (DRM_MODE_OBJECT_CRTC, CRTC_ID) => (CRTC_PROPS, crtc_prop_values(&state)),
            (DRM_MODE_OBJECT_CONNECTOR, CONNECTOR_ID) => (CONN_PROPS, conn_prop_values(&state)),
            _ => return Err(VfsError::NotFound),
        };
        report_user_array(q.props_ptr, q.count_props, prop_ids)?;
        report_user_array(q.prop_values_ptr, q.count_props, &prop_vals)?;
        q.count_props = prop_ids.len() as u32;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetProperty {
    const CMD: u32 = iowr::<DrmModeGetProperty>(DRM_TYPE, 0xAA);

    fn handle(_dev: &Card0, _file: &DrmFile, g: &mut Self) -> VfsResult<usize> {
        let meta = property_meta(g.prop_id).ok_or(VfsError::NotFound)?;

        g.flags = meta.flags;
        g.name = [0; DRM_PROP_NAME_LEN];
        let nb = meta.name.as_bytes();
        let n = nb.len().min(DRM_PROP_NAME_LEN - 1);
        g.name[..n].copy_from_slice(&nb[..n]);

        match meta.kind {
            PropKind::Enum(enums) => {
                g.count_values = enums.len() as u32;
                g.count_enum_blobs = report_user_array(g.enum_blob_ptr, g.count_enum_blobs, enums)?;
            }
            PropKind::RangeU64 { min, max } => {
                let limits = [min, max];
                g.count_values = report_user_array(g.values_ptr, g.count_values, &limits)?;
                g.count_enum_blobs = 0;
            }
            PropKind::Object | PropKind::Blob => {
                g.count_values = 0;
                g.count_enum_blobs = 0;
            }
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmWaitVblank {
    const CMD: u32 = iowr::<DrmWaitVblank>(DRM_TYPE, 0x3A);

    fn handle(dev: &Card0, _file: &DrmFile, request: &mut Self) -> VfsResult<usize> {
        let is_relative = request.rep_type & DRM_VBLANK_RELATIVE != 0;
        let current = dev.sequence.load(Ordering::Acquire);
        let target = if is_relative {
            current.wrapping_add(request.sequence)
        } else {
            request.sequence
        };
        let raw_wait = target.wrapping_sub(current);
        let wait_count = if raw_wait == 0 || raw_wait >= i32::MAX as u32 {
            1
        } else {
            raw_wait
        };

        const FRAME_PERIOD_NS: u64 = ktime_types::NANOS_PER_SEC / 60;
        let delay =
            ktime_types::TimeSpan::from_nanos(FRAME_PERIOD_NS.saturating_mul(wait_count as u64));
        ktask::sleep(delay);
        dev.sequence.fetch_add(wait_count, Ordering::AcqRel);

        let now = khal::time::monotonic_time();
        let now = now.span_since_origin();
        *request = DrmWaitVblank {
            rep_type: 0,
            sequence: dev.sequence.load(Ordering::Acquire),
            tv_sec: now.as_secs() as i64,
            tv_usec: now.subsec_micros() as i64,
        };
        Ok(0)
    }
}

impl DrmIoctl for DrmModeAtomic {
    const CMD: u32 = iowr::<DrmModeAtomic>(DRM_TYPE, 0xBC);

    fn handle(dev: &Card0, file: &DrmFile, a: &mut Self) -> VfsResult<usize> {
        let _kms_reservation = dev.kms_reservation.lock();
        let known = DRM_MODE_ATOMIC_TEST_ONLY
            | DRM_MODE_ATOMIC_NONBLOCK
            | DRM_MODE_ATOMIC_ALLOW_MODESET
            | DRM_MODE_PAGE_FLIP_EVENT;
        if a.flags & !known != 0 {
            return Err(VfsError::InvalidInput);
        }

        let n = a.count_objs as usize;
        let objs: Vec<u32> = a.objs_ptr.load_vm_vec(n)?;
        let counts: Vec<u32> = a.count_props_ptr.load_vm_vec(n)?;
        let total_props: usize = counts.iter().map(|c| *c as usize).sum();
        let props: Vec<u32> = a.props_ptr.load_vm_vec(total_props)?;
        let values: Vec<u64> = a.prop_values_ptr.load_vm_vec(total_props)?;

        let state = dev.state.lock();
        let mut proposed = *state;
        let mut new_mode_blob: Option<Option<Arc<Vec<u8>>>> = None;
        let mut idx = 0;
        for (obj_i, &obj_id) in objs.iter().enumerate() {
            let obj_type = object_type_of(obj_id).ok_or(VfsError::NotFound)?;
            for _ in 0..counts[obj_i] {
                let prop_id = props[idx];
                let value = values[idx];
                idx += 1;
                if !dev.apply_prop(
                    obj_type,
                    obj_id,
                    prop_id,
                    value,
                    &mut proposed,
                    &mut new_mode_blob,
                )? {
                    return Err(VfsError::InvalidInput);
                }
            }
        }

        if a.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 {
            return Ok(0);
        }

        let current_fb = proposed.plane_fb_id;
        drop(state);
        if current_fb != 0 && proposed.crtc_active != 0 {
            dev.present_fb(current_fb)?;
        } else {
            dev.disable_current_scanout()?;
        }
        *dev.state.lock() = proposed;
        if let Some(new_ref) = new_mode_blob {
            *dev.mode_id_blob_ref.lock() = new_ref;
        }
        if a.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            dev.queue_flip_event(file, a.user_data);
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCreateBlob {
    const CMD: u32 = iowr::<DrmModeCreateBlob>(DRM_TYPE, 0xBD);

    fn handle(dev: &Card0, _file: &DrmFile, c: &mut Self) -> VfsResult<usize> {
        if c.length == 0 || c.length as usize > MAX_BLOB_BYTES {
            return Err(VfsError::InvalidInput);
        }
        let len = c.length as usize;
        let bytes = c.data.load_vm_vec(len).map_err(|_| VfsError::BadAddress)?;
        let id = dev.next_blob_id.fetch_add(1, Ordering::Relaxed);
        dev.blobs.lock().insert(id, Arc::new(bytes));
        c.blob_id = id;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeDestroyBlob {
    const CMD: u32 = iowr::<DrmModeDestroyBlob>(DRM_TYPE, 0xBE);

    fn handle(dev: &Card0, _file: &DrmFile, d: &mut Self) -> VfsResult<usize> {
        if dev.system_blobs.lock().contains_key(&d.blob_id) {
            return Err(VfsError::PermissionDenied);
        }
        dev.blobs
            .lock()
            .remove(&d.blob_id)
            .ok_or(VfsError::NotFound)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetBlob {
    const CMD: u32 = iowr::<DrmModeGetBlob>(DRM_TYPE, 0xAC);

    fn handle(dev: &Card0, _file: &DrmFile, g: &mut Self) -> VfsResult<usize> {
        let bytes = if let Some(b) = dev.blobs.lock().get(&g.blob_id).cloned() {
            b
        } else if g.blob_id == dev.state.lock().crtc_mode_id
            && let Some(b) = dev.mode_id_blob_ref.lock().clone()
        {
            b
        } else if let Some(b) = dev.system_blobs.lock().get(&g.blob_id).cloned() {
            b
        } else {
            return Err(VfsError::NotFound);
        };
        if !g.data.is_null() && g.length > 0 {
            let n = (g.length as usize).min(bytes.len());
            g.data
                .write_vm_slice(&bytes[..n])
                .map_err(|_| VfsError::BadAddress)?;
        }
        g.length = bytes.len() as u32;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeFbCmd2 {
    const CMD: u32 = iowr::<DrmModeFbCmd2>(DRM_TYPE, 0xB8);

    fn handle_raw(dev: &Card0, file: &DrmFile, arg: UserPtr<Self>) -> VfsResult<usize> {
        let mut request = arg.read_vm().map_err(|_| VfsError::BadAddress)?;
        publish_framebuffer(dev, file, &mut request, |request| {
            arg.write_vm(*request).map_err(|_| VfsError::BadAddress)
        })?;
        Ok(0)
    }

    fn handle(dev: &Card0, file: &DrmFile, f: &mut Self) -> VfsResult<usize> {
        let handle = f.handles[0];
        if handle == 0 || f.offsets[0] != 0 {
            return Err(VfsError::InvalidInput);
        }
        for i in 1..4 {
            if f.handles[i] != 0 || f.pitches[i] != 0 || f.offsets[i] != 0 {
                return Err(VfsError::InvalidInput);
            }
        }
        if !matches!(f.pixel_format, DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888) {
            return Err(VfsError::InvalidInput);
        }
        let object = {
            let dumbs = file.dumbs.lock();
            let Some(b) = dumbs.get(&handle) else {
                return Err(VfsError::InvalidInput);
            };
            b.clone()
        };
        if let Some(virgl) = &object.virgl {
            validate_virgl_scanout(virgl.params)?;
        }
        // The scanout resource is created at exactly the dumb buffer's size
        // and pitch (no ANY_LAYOUT negotiation), and TRANSFER_TO_HOST_2D reads
        // the backing row-by-row with the resource pitch. A sub-size
        // framebuffer would be misread (garbled scanout) or rejected by the
        // device, so the framebuffer must match the resource layout exactly.
        if f.width != object.width || f.height != object.height || f.pitches[0] != object.pitch {
            return Err(VfsError::InvalidInput);
        }
        if f.flags & DRM_MODE_FB_MODIFIERS != 0 {
            for i in 0..4 {
                if f.handles[i] == 0 {
                    continue;
                }
                let m = f.modifier[i];
                if m != DRM_FORMAT_MOD_LINEAR && m != DRM_FORMAT_MOD_INVALID {
                    return Err(VfsError::InvalidInput);
                }
            }
        }
        let fb_id = virtgpu::next_id(&dev.next_fb_id)?;
        dev.fbs.lock().insert(
            fb_id,
            Framebuffer {
                fb_id,
                owner: file.self_ref.clone(),
                pending_owner_close: false,
                object,
                width: f.width,
                height: f.height,
            },
        );
        file.owned_fbs.lock().insert(fb_id);
        f.fb_id = fb_id;
        Ok(0)
    }
}

fn publish_framebuffer(
    dev: &Card0,
    file: &DrmFile,
    request: &mut DrmModeFbCmd2,
    copyout: impl FnOnce(&DrmModeFbCmd2) -> VfsResult<()>,
) -> VfsResult<()> {
    let _kms = dev.kms_reservation.lock();
    DrmModeFbCmd2::handle(dev, file, request)?;
    if let Err(error) = copyout(request) {
        // No KMS effect can have acquired this unpublished FB while the
        // dedicated gate is held; retirement therefore performs no GPU wait.
        dev.retire_framebuffer(request.fb_id)?;
        return Err(error);
    }
    Ok(())
}

fn validate_virgl_scanout(params: display::VirglResourceCreate) -> VfsResult<()> {
    if params.target != 2
        || !matches!(params.format, 1 | 2)
        || params.depth != 1
        || params.array_size != 1
        || params.last_level != 0
        || params.nr_samples > 1
    {
        return Err(VfsError::InvalidInput);
    }
    Ok(())
}

impl Card0 {
    fn retire_framebuffer(&self, fb_id: u32) -> VfsResult<()> {
        // Caller owns the dedicated KMS reservation, never a framebuffer mutex.
        let fb = self
            .fbs
            .lock()
            .get(&fb_id)
            .cloned()
            .ok_or(VfsError::NotFound)?;
        let is_active = self
            .active_scanout
            .lock()
            .as_ref()
            .is_some_and(|active| active.fb_id == fb_id)
            || self.state.lock().plane_fb_id == fb_id;
        if is_active {
            self.disable_current_scanout()?;
            *self.state.lock() = ModesetState::default();
            *self.mode_id_blob_ref.lock() = None;
        }
        let retired = self.fbs.lock().remove(&fb_id);
        if let Some(owner) = fb.owner.upgrade() {
            owner.owned_fbs.lock().remove(&fb_id);
        }
        if self.legacy_crtc.lock().fb_id == fb_id {
            *self.legacy_crtc.lock() = LegacyCrtcState::default();
        }
        drop(retired);
        Ok(())
    }

    fn queue_owned_framebuffers(&self, file: &DrmFile) -> VfsResult<()> {
        self.mark_owned_framebuffers(file);
        if !self.fbs.lock().values().any(|fb| fb.pending_owner_close) {
            return Ok(());
        }
        match self.fb_cleanup_work.schedule() {
            kwork::QueueWorkResult::Queued | kwork::QueueWorkResult::AlreadyQueued => Ok(()),
            error => {
                log::warn!("drm: framebuffer retirement remains pending: {:?}", error);
                Err(VfsError::WouldBlock)
            }
        }
    }

    fn mark_owned_framebuffers(&self, file: &DrmFile) -> bool {
        let ids = core::mem::take(&mut *file.owned_fbs.lock());
        let had_owned = !ids.is_empty();
        let mut fbs = self.fbs.lock();
        for id in ids {
            if let Some(fb) = fbs.get_mut(&id) {
                fb.pending_owner_close = true;
            }
        }
        had_owned
    }

    fn drain_closed_framebuffers(&self) {
        let _kms = self.kms_reservation.lock();
        loop {
            let next = self
                .fbs
                .lock()
                .values()
                .find(|fb| fb.pending_owner_close)
                .map(|fb| fb.fb_id);
            let Some(id) = next else {
                break;
            };
            if let Err(error) = self.retire_framebuffer(id) {
                log::warn!(
                    "drm: framebuffer {} owner-close retirement pending: {:?}",
                    id,
                    error
                );
                break;
            }
        }
    }

    fn disable_current_scanout(&self) -> VfsResult<()> {
        let active = self.active_scanout.lock().clone();
        let device = if let Some(active) = &active {
            active.object.device.clone().ok_or(VfsError::NoSuchDevice)?
        } else {
            primary_scanout()?
        };
        let _reservation = active.as_ref().map(|fb| fb.object.reserve());
        device.disable_scanout().map_err(virtgpu::driver_error)?;
        *self.active_scanout.lock() = None;
        Ok(())
    }

    fn present_fb(&self, fb_id: u32) -> VfsResult<()> {
        let fb = self
            .fbs
            .lock()
            .get(&fb_id)
            .cloned()
            .ok_or(VfsError::InvalidInput)?;
        let device = fb.object.device.as_ref().ok_or(VfsError::NoSuchDevice)?;
        let _reservation = fb.object.reserve();
        let resource_id = fb.object.resource_id;
        let rect = ScanoutRect {
            x: 0,
            y: 0,
            width: fb.width,
            height: fb.height,
        };
        // Keep an attempted scanout reference even on error: only the backend's
        // fenced reset/disable can prove that host scanout ownership has ended.
        *self.active_scanout.lock() = Some(fb.clone());
        // A 3D resource already contains host-rendered pixels. The 2D path
        // uploads guest pages and would corrupt those pixels, so never use it.
        if fb.object.virgl.is_some() {
            device.virgl_command(display::VirglCommand::Present { resource_id, rect })
        } else {
            device.present_scanout_resource(resource_id, rect)
        }
        .map_err(virtgpu::driver_error)
    }

    fn queue_flip_event(&self, file: &DrmFile, user_data: u64) {
        let seq = self
            .sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let now = khal::time::monotonic_time();
        let now = now.span_since_origin();
        let ev = DrmEventVblank {
            base: DrmEvent {
                event_type: DRM_EVENT_FLIP_COMPLETE,
                length: core::mem::size_of::<DrmEventVblank>() as u32,
            },
            user_data,
            tv_sec: now.as_secs() as u32,
            tv_usec: now.subsec_micros(),
            sequence: seq,
            crtc_id: CRTC_ID,
        };
        let enqueued = {
            let mut queue = file.events.lock();
            if queue.len() >= MAX_EVENTS {
                false
            } else {
                queue.push_back(ev);
                true
            }
        };
        if enqueued {
            file.poll_rx.wake();
        }
    }

    fn apply_prop(
        &self,
        obj_type: u32,
        _obj_id: u32,
        prop_id: u32,
        value: u64,
        s: &mut ModesetState,
        new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
    ) -> VfsResult<bool> {
        match (obj_type, prop_id) {
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_TYPE) => {
                if value != DRM_PLANE_TYPE_PRIMARY {
                    return Err(VfsError::InvalidInput);
                }
            }
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_FB_ID) => {
                let fb = value as u32;
                if fb != 0 && !self.fbs.lock().contains_key(&fb) {
                    return Err(VfsError::InvalidInput);
                }
                s.plane_fb_id = fb;
            }
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_CRTC_ID) => {
                let c = value as u32;
                if c != 0 && c != CRTC_ID {
                    return Err(VfsError::InvalidInput);
                }
                s.plane_crtc_id = c;
            }
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_SRC_X) => s.plane_src_x = value,
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_SRC_Y) => s.plane_src_y = value,
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_SRC_W) => s.plane_src_w = value,
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_SRC_H) => s.plane_src_h = value,
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_CRTC_X) => {
                s.plane_crtc_x = checked_i32(value)? as i64;
            }
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_CRTC_Y) => {
                s.plane_crtc_y = checked_i32(value)? as i64;
            }
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_CRTC_W) => s.plane_crtc_w = value,
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_CRTC_H) => s.plane_crtc_h = value,
            (DRM_MODE_OBJECT_CRTC, PROP_CRTC_ACTIVE) => {
                if value > 1 {
                    return Err(VfsError::InvalidInput);
                }
                s.crtc_active = value;
            }
            (DRM_MODE_OBJECT_CRTC, PROP_CRTC_MODE_ID) => {
                let blob = value as u32;
                let arc = if blob == 0 {
                    None
                } else {
                    let arc = self.blobs.lock().get(&blob).cloned().or_else(|| {
                        if s.crtc_mode_id == blob {
                            self.mode_id_blob_ref.lock().clone()
                        } else {
                            None
                        }
                    });
                    let arc = arc.ok_or(VfsError::InvalidInput)?;
                    let mode = bytemuck::try_pod_read_unaligned::<DrmModeModeInfo>(&arc)
                        .map_err(|_| VfsError::InvalidInput)?;
                    if mode.hdisplay == 0 || mode.vdisplay == 0 {
                        return Err(VfsError::InvalidInput);
                    }
                    Some(arc)
                };
                s.crtc_mode_id = blob;
                *new_mode_blob = Some(arc);
            }
            (DRM_MODE_OBJECT_CONNECTOR, PROP_CONN_CRTC_ID) => {
                let c = value as u32;
                if c != 0 && c != CRTC_ID {
                    return Err(VfsError::InvalidInput);
                }
                s.conn_crtc_id = c;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

fn plane_prop_values(s: &ModesetState, in_formats: u64) -> Vec<u64> {
    vec![
        DRM_PLANE_TYPE_PRIMARY,
        s.plane_fb_id as u64,
        s.plane_crtc_id as u64,
        s.plane_src_x,
        s.plane_src_y,
        s.plane_src_w,
        s.plane_src_h,
        s.plane_crtc_x as u64,
        s.plane_crtc_y as u64,
        s.plane_crtc_w,
        s.plane_crtc_h,
        in_formats,
    ]
}

fn build_in_formats_blob() -> Vec<u8> {
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::NoUninit)]
    struct Header {
        version: u32,
        flags: u32,
        count_formats: u32,
        formats_offset: u32,
        count_modifiers: u32,
        modifiers_offset: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::NoUninit)]
    struct ModifierEntry {
        formats: u64,
        offset: u32,
        _pad: u32,
        modifier: u64,
    }
    let n_formats = SUPPORTED_FORMATS.len() as u32;
    let formats_off = core::mem::size_of::<Header>() as u32;
    let modifiers_off = formats_off + n_formats * 4;
    let hdr = Header {
        version: 1,
        flags: 0,
        count_formats: n_formats,
        formats_offset: formats_off,
        count_modifiers: 1,
        modifiers_offset: modifiers_off,
    };
    let format_mask = (1u64 << n_formats) - 1;
    let me = ModifierEntry {
        formats: format_mask,
        offset: 0,
        _pad: 0,
        modifier: DRM_FORMAT_MOD_LINEAR,
    };
    let mut buf = Vec::with_capacity(
        core::mem::size_of::<Header>()
            + (n_formats as usize) * 4
            + core::mem::size_of::<ModifierEntry>(),
    );
    buf.extend_from_slice(bytes_of(&hdr));
    for fmt in SUPPORTED_FORMATS {
        buf.extend_from_slice(&fmt.to_le_bytes());
    }
    buf.extend_from_slice(bytes_of(&me));
    buf
}

fn crtc_prop_values(s: &ModesetState) -> Vec<u64> {
    vec![s.crtc_active, s.crtc_mode_id as u64]
}

fn conn_prop_values(s: &ModesetState) -> Vec<u64> {
    vec![s.conn_crtc_id as u64]
}

struct PropMeta {
    name: &'static str,
    flags: u32,
    kind: PropKind,
}

enum PropKind {
    Enum(&'static [DrmModePropertyEnum]),
    RangeU64 { min: u64, max: u64 },
    Object,
    Blob,
}

const fn enum_entry(value: u64, name: &[u8]) -> DrmModePropertyEnum {
    let mut e = DrmModePropertyEnum {
        value,
        name: [0; DRM_PROP_NAME_LEN],
    };
    let n = if name.len() < DRM_PROP_NAME_LEN - 1 {
        name.len()
    } else {
        DRM_PROP_NAME_LEN - 1
    };
    let mut i = 0;
    while i < n {
        e.name[i] = name[i];
        i += 1;
    }
    e
}

const PLANE_TYPE_ENUMS: &[DrmModePropertyEnum] = &[
    enum_entry(0, b"Overlay"),
    enum_entry(1, b"Primary"),
    enum_entry(2, b"Cursor"),
];

fn property_meta(id: u32) -> Option<PropMeta> {
    let atomic = DRM_MODE_PROP_ATOMIC;
    let meta = match id {
        PROP_PLANE_TYPE => PropMeta {
            name: "type",
            flags: DRM_MODE_PROP_ENUM | DRM_MODE_PROP_IMMUTABLE,
            kind: PropKind::Enum(PLANE_TYPE_ENUMS),
        },
        PROP_PLANE_FB_ID => PropMeta {
            name: "FB_ID",
            flags: DRM_MODE_PROP_OBJECT | atomic,
            kind: PropKind::Object,
        },
        PROP_PLANE_CRTC_ID => PropMeta {
            name: "CRTC_ID",
            flags: DRM_MODE_PROP_OBJECT | atomic,
            kind: PropKind::Object,
        },
        PROP_PLANE_SRC_X => range_u32("SRC_X", atomic),
        PROP_PLANE_SRC_Y => range_u32("SRC_Y", atomic),
        PROP_PLANE_SRC_W => range_u32("SRC_W", atomic),
        PROP_PLANE_SRC_H => range_u32("SRC_H", atomic),
        PROP_PLANE_CRTC_X => range_u32("CRTC_X", atomic),
        PROP_PLANE_CRTC_Y => range_u32("CRTC_Y", atomic),
        PROP_PLANE_CRTC_W => range_u32("CRTC_W", atomic),
        PROP_PLANE_CRTC_H => range_u32("CRTC_H", atomic),
        PROP_PLANE_IN_FORMATS => PropMeta {
            name: "IN_FORMATS",
            flags: DRM_MODE_PROP_BLOB | DRM_MODE_PROP_IMMUTABLE,
            kind: PropKind::Blob,
        },
        PROP_CRTC_ACTIVE => PropMeta {
            name: "ACTIVE",
            flags: DRM_MODE_PROP_RANGE | atomic,
            kind: PropKind::RangeU64 { min: 0, max: 1 },
        },
        PROP_CRTC_MODE_ID => PropMeta {
            name: "MODE_ID",
            flags: DRM_MODE_PROP_BLOB | atomic,
            kind: PropKind::Blob,
        },
        PROP_CONN_CRTC_ID => PropMeta {
            name: "CRTC_ID",
            flags: DRM_MODE_PROP_OBJECT | atomic,
            kind: PropKind::Object,
        },
        _ => return None,
    };
    Some(meta)
}

fn range_u32(name: &'static str, atomic: u32) -> PropMeta {
    PropMeta {
        name,
        flags: DRM_MODE_PROP_RANGE | atomic,
        kind: PropKind::RangeU64 {
            min: 0,
            max: u32::MAX as u64,
        },
    }
}

fn object_type_of(id: u32) -> Option<u32> {
    match id {
        CRTC_ID => Some(DRM_MODE_OBJECT_CRTC),
        CONNECTOR_ID => Some(DRM_MODE_OBJECT_CONNECTOR),
        PLANE_ID => Some(DRM_MODE_OBJECT_PLANE),
        _ => None,
    }
}

fn checked_i32(value: u64) -> VfsResult<i32> {
    let v = value as i64;
    if (i32::MIN as i64..=i32::MAX as i64).contains(&v) {
        Ok(v as i32)
    } else {
        Err(VfsError::InvalidInput)
    }
}

impl DrmIoctl for DrmGemClose {
    const CMD: u32 = iow::<DrmGemClose>(DRM_TYPE, 0x09);

    fn handle_raw(dev: &Card0, file: &DrmFile, arg: UserPtr<Self>) -> VfsResult<usize> {
        // GEM_CLOSE is input-only: a read-only user buffer is valid.
        let mut request = arg.read_vm().map_err(|_| VfsError::BadAddress)?;
        Self::handle(dev, file, &mut request)
    }

    fn handle(_dev: &Card0, file: &DrmFile, arg: &mut Self) -> VfsResult<usize> {
        file.close_handle(arg.handle)?;
        Ok(0)
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    pub(super) fn object() -> Arc<DumbBuffer> {
        let mut pages = GlobalPage::alloc_contiguous(1, PAGE_SIZE_4K).unwrap();
        pages.zero();
        Arc::new(DumbBuffer {
            width: 1,
            height: 1,
            bpp: 32,
            pitch: 4,
            size: 4,
            offset: DUMB_BUFFER_OFFSET_STRIDE,
            resource_id: 0x1000,
            device: None,
            pages: Arc::new(pages),
            reservation: Mutex::new(()),
            completion_poll: PollSet::new(),
            exported: Mutex::new(Weak::new()),
            virgl: None,
        })
    }

    #[def_test]
    fn separate_opens_isolate_handles_and_close_errors() {
        let first = DrmFile::new();
        let second = DrmFile::new();
        first.dumbs.lock().insert(FIRST_DUMB_HANDLE, object());
        assert_eq!(
            second.close_handle(FIRST_DUMB_HANDLE),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(first.close_handle(FIRST_DUMB_HANDLE), Ok(()));
        assert_eq!(
            first.close_handle(FIRST_DUMB_HANDLE),
            Err(VfsError::InvalidInput)
        );
    }

    #[def_test]
    fn shared_description_preserves_handles_until_last_reference() {
        let first = DrmFile::new();
        let duplicate = first.clone();
        first.dumbs.lock().insert(FIRST_DUMB_HANDLE, object());
        drop(first);
        assert_eq!(duplicate.dumbs.lock().len(), 1);
        assert_eq!(duplicate.close_handle(FIRST_DUMB_HANDLE), Ok(()));
    }

    #[def_test]
    fn events_and_client_capabilities_are_per_description() {
        let first = DrmFile::new();
        let second = DrmFile::new();
        let device = Card0::new();
        device.queue_flip_event(&first, 0x1234);
        assert_eq!(first.poll().contains(IoEvents::IN), true);
        assert_eq!(second.poll().contains(IoEvents::IN), false);
        let duplicate = first.clone();
        assert_eq!(
            duplicate.events.lock().pop_front().unwrap().user_data,
            0x1234
        );
        assert_eq!(first.poll().contains(IoEvents::IN), false);
        let mut cap = DrmSetClientCap {
            capability: 3,
            value: 1,
        };
        assert_eq!(DrmSetClientCap::handle(&device, &first, &mut cap), Ok(0));
        assert_eq!(duplicate.client_caps.lock().get(&3).copied(), Some(1));
        assert_eq!(second.client_caps.lock().get(&3).copied(), None);
        cap.value = 2;
        assert_eq!(
            DrmSetClientCap::handle(&device, &first, &mut cap),
            Err(VfsError::InvalidInput)
        );
    }

    #[def_test]
    fn failed_fb_copyout_and_owner_close_retire_global_resource_references() {
        let card = Card0::new();
        let file = DrmFile::new();
        let object = object();
        let weak = Arc::downgrade(&object);
        file.dumbs.lock().insert(1, object.clone());
        let mut fb = DrmModeFbCmd2 {
            width: 1,
            height: 1,
            pixel_format: DRM_FORMAT_XRGB8888,
            handles: [1, 0, 0, 0],
            pitches: [4, 0, 0, 0],
            ..Default::default()
        };
        assert_eq!(
            publish_framebuffer(&card, &file, &mut fb, |_| Err(VfsError::BadAddress)),
            Err(VfsError::BadAddress)
        );
        assert_eq!(card.fbs.lock().len(), 0);
        assert_eq!(file.owned_fbs.lock().len(), 0);
        publish_framebuffer(&card, &file, &mut fb, |_| Ok(())).unwrap();
        assert_eq!(card.fbs.lock().len(), 1);
        assert_eq!(card.mark_owned_framebuffers(&file), true);
        // Production's prebuilt worker uses the same drain, outside Drop/MM locks.
        card.drain_closed_framebuffers();
        assert_eq!(card.fbs.lock().len(), 0);
        file.close_handle(1).unwrap();
        drop(object);
        assert_eq!(weak.strong_count(), 0);
    }

    #[def_test]
    fn failed_owner_close_disable_keeps_explicit_pending_active_refs() {
        let card = Card0::new();
        let file = DrmFile::new();
        let object = object();
        let fb = Framebuffer {
            fb_id: 42,
            owner: file.self_ref.clone(),
            pending_owner_close: true,
            object,
            width: 1,
            height: 1,
        };
        card.fbs.lock().insert(42, fb.clone());
        *card.active_scanout.lock() = Some(fb);
        // Fixture has no bound backend, so disabling fails before retirement.
        card.drain_closed_framebuffers();
        let pending = card.fbs.lock().get(&42).unwrap().pending_owner_close;
        let active = card.active_scanout.lock().as_ref().unwrap().fb_id;
        assert_eq!(pending, true);
        assert_eq!(active, 42);
    }

    #[def_test]
    fn virgl_scanout_accepts_only_single_plane_unmultisampled_bgra_textures() {
        let mut params = display::VirglResourceCreate {
            resource_id: 1,
            target: 2,
            format: 1,
            bind: 2,
            width: 64,
            height: 64,
            depth: 1,
            array_size: 1,
            last_level: 0,
            nr_samples: 0,
            flags: 0,
        };
        assert_eq!(validate_virgl_scanout(params), Ok(()));
        params.target = 0;
        assert_eq!(validate_virgl_scanout(params), Err(VfsError::InvalidInput));
        params.target = 2;
        params.nr_samples = 4;
        assert_eq!(validate_virgl_scanout(params), Err(VfsError::InvalidInput));
        params.nr_samples = 0;
        params.format = 64;
        assert_eq!(validate_virgl_scanout(params), Err(VfsError::InvalidInput));
    }

    #[def_test]
    fn independent_card_instances_do_not_reuse_host_ids_or_mmap_tokens() {
        let first = Card0::new();
        let second = Card0::new();
        assert_eq!(
            core::ptr::eq(first.next_resource_id, second.next_resource_id),
            true
        );
        assert_eq!(
            core::ptr::eq(first.next_context_id, second.next_context_id),
            true
        );
        assert_eq!(core::ptr::eq(first.next_offset, second.next_offset), true);
    }

    #[def_test]
    fn getcrtc_reports_committed_atomic_state_and_disabled_state() {
        let dev = Card0::new();
        let file = DrmFile::new();
        *dev.state.lock() = ModesetState {
            crtc_active: 1,
            plane_fb_id: 99,
            conn_crtc_id: CRTC_ID,
            plane_src_x: 2 << 16,
            plane_src_y: 3 << 16,
            ..ModesetState::default()
        };
        let mut query = DrmModeCrtc {
            crtc_id: CRTC_ID,
            ..Default::default()
        };
        assert_eq!(DrmModeCrtc::handle(&dev, &file, &mut query), Ok(0));
        assert_eq!(query.fb_id, 99);
        assert_eq!(query.mode_valid, 1);
        assert_eq!((query.x, query.y), (2, 3));
        *dev.state.lock() = ModesetState::default();
        assert_eq!(DrmModeCrtc::handle(&dev, &file, &mut query), Ok(0));
        assert_eq!(query.fb_id, 0);
        assert_eq!(query.mode_valid, 0);
    }

    #[def_test]
    fn framebuffer_and_scanout_retain_object_after_handle_close() {
        let state = DrmFile::new();
        let backing = object();
        let weak = Arc::downgrade(&backing);
        state
            .dumbs
            .lock()
            .insert(FIRST_DUMB_HANDLE, backing.clone());
        let fb = Framebuffer {
            fb_id: 1,
            owner: Weak::new(),
            pending_owner_close: false,
            object: backing,
            width: 1,
            height: 1,
        };
        let scanout = fb.clone();
        assert_eq!(state.close_handle(FIRST_DUMB_HANDLE), Ok(()));
        drop(fb);
        assert_eq!(weak.strong_count(), 1);
        drop(scanout);
        assert_eq!(weak.strong_count(), 0);
    }
}
