// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fixed-width legacy virtgpu ioctl carriers, matching the installed DRM UAPI.

use bytemuck::AnyBitPattern;
use posix_types::{UserRead, UserWrite};

#[repr(C)]
#[derive(Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct VirtgpuMap {
    pub offset: u64,
    pub handle: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct VirtgpuExecbuffer {
    pub flags: u32,
    pub size: u32,
    pub command: u64,
    pub bo_handles: u64,
    pub num_bo_handles: u32,
    pub fence_fd: i32,
    pub ring_idx: u32,
    pub syncobj_stride: u32,
    pub num_in_syncobjs: u32,
    pub num_out_syncobjs: u32,
    pub in_syncobjs: u64,
    pub out_syncobjs: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct VirtgpuGetparam {
    pub param: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct VirtgpuResourceCreate {
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
    pub bo_handle: u32,
    pub res_handle: u32,
    pub size: u32,
    pub stride: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct VirtgpuResourceInfo {
    pub bo_handle: u32,
    pub res_handle: u32,
    pub size: u32,
    pub blob_mem: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct VirtgpuBox {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct VirtgpuTransfer {
    pub bo_handle: u32,
    pub box_: VirtgpuBox,
    pub level: u32,
    pub offset: u32,
    pub stride: u32,
    pub layer_stride: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct VirtgpuWait {
    pub handle: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct VirtgpuGetCaps {
    pub cap_set_id: u32,
    pub cap_set_ver: u32,
    pub addr: u64,
    pub size: u32,
    pub pad: u32,
}

use display::{
    DriverError, GpuBacking, VirglBox, VirglCommand, VirglResourceCreate, VirglTransfer,
};

use super::*;

const MAX_COMMAND_BYTES: usize = 1024 * 1024;
const MAX_SUBMISSION_OBJECTS: usize = 1024;

pub(super) struct VirglContext {
    id: u32,
    device: ClassDevice<DisplayDeviceImpl>,
}

impl Drop for VirglContext {
    fn drop(&mut self) {
        if let Err(error) = self.device.enqueue_virgl_context_cleanup(self.id) {
            log::warn!(
                "drm: deferred context {} cleanup failed: {:?}",
                self.id,
                error
            );
        }
    }
}

pub(super) struct VirglObject {
    params: VirglResourceCreate,
    context: Arc<VirglContext>,
}

impl Drop for DumbBuffer {
    fn drop(&mut self) {
        if let Some(device) = &self.device
            && let Err(error) = device.enqueue_virgl_cleanup(self.resource_id)
        {
            // The backend retains its allocation owner, including on failure.
            log::warn!(
                "drm: deferred resource {} cleanup failed: {:?}",
                self.resource_id,
                error
            );
        }
    }
}

pub(super) fn driver_error(error: DriverError) -> VfsError {
    match error {
        DriverError::InvalidInput => VfsError::InvalidInput,
        DriverError::Unsupported => VfsError::OperationNotSupported,
        DriverError::NoMemory => VfsError::NoMemory,
        DriverError::WouldBlock => VfsError::WouldBlock,
        DriverError::ResourceBusy => VfsError::ResourceBusy,
        _ => VfsError::Io,
    }
}

fn device() -> VfsResult<ClassDevice<DisplayDeviceImpl>> {
    let device = primary_scanout()?;
    if !device.has_virgl_commands() {
        return Err(VfsError::OperationNotSupported);
    }
    if device.virgl_command_status().is_failed {
        return Err(VfsError::Io);
    }
    Ok(device)
}

pub(super) fn next_id(counter: &AtomicU32) -> VfsResult<u32> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| VfsError::NoMemory)
}

fn lookup(file: &DrmFile, handle: u32) -> VfsResult<Arc<DumbBuffer>> {
    file.dumbs
        .lock()
        .get(&handle)
        .cloned()
        .ok_or(VfsError::InvalidInput)
}

// Caller holds context_admission, never the file-context mutex while waiting.
fn context(dev: &Card0, file: &DrmFile) -> VfsResult<Arc<VirglContext>> {
    if let Some(context) = file.context.lock().clone() {
        return Ok(context);
    }
    let device = device()?;
    let id = next_id(&dev.next_context_id)?;
    let context = Arc::new(VirglContext { id, device });
    context
        .device
        .virgl_command(VirglCommand::CreateContext {
            context_id: id,
            name: "xkernel-drm",
        })
        .map_err(driver_error)?;
    *file.context.lock() = Some(context.clone());
    Ok(context)
}

fn attach(context: &VirglContext, object: &DumbBuffer) -> VfsResult<()> {
    let virgl = object.virgl.as_ref().ok_or(VfsError::InvalidInput)?;
    if virgl.context.device.id() != context.device.id() {
        return Err(VfsError::InvalidInput);
    }
    context
        .device
        .virgl_command(VirglCommand::AttachContext {
            context_id: context.id,
            resource_id: object.resource_id,
        })
        .map_err(driver_error)?;
    Ok(())
}

pub(super) fn dispatch(
    dev: &Card0,
    file: &DrmFile,
    cmd: u32,
    arg: usize,
) -> Option<VfsResult<usize>> {
    macro_rules! ioctl {
        ($ty:ty, $nr:literal) => {
            if cmd == iowr::<$ty>(DRM_TYPE, $nr) {
                return Some(<$ty as DrmIoctl>::handle_raw(dev, file, UserPtr::from(arg)));
            }
        };
    }
    ioctl!(VirtgpuMap, 0x41);
    ioctl!(VirtgpuExecbuffer, 0x42);
    ioctl!(VirtgpuGetparam, 0x43);
    ioctl!(VirtgpuResourceCreate, 0x44);
    ioctl!(VirtgpuResourceInfo, 0x45);
    // Transfer directions have identical Linux carrier layouts.
    if cmd == iowr::<VirtgpuTransfer>(DRM_TYPE, 0x46)
        || cmd == iowr::<VirtgpuTransfer>(DRM_TYPE, 0x47)
    {
        let result = (|| {
            let request = UserPtr::<VirtgpuTransfer>::from(arg)
                .read_vm()
                .map_err(|_| VfsError::BadAddress)?;
            transfer(dev, file, &request, cmd & 0xff == 0x46)?;
            Ok(0)
        })();
        return Some(result);
    }
    ioctl!(VirtgpuWait, 0x48);
    ioctl!(VirtgpuGetCaps, 0x49);
    // BLOB, CONTEXT_INIT and unknown extension ioctls fail via normal dispatch.
    None
}

fn param_value(
    param: u64,
    implemented: bool,
    caps: Option<&[display::VirglCapsetInfo]>,
) -> VfsResult<u32> {
    Ok(match param {
        1 | 2 => u32::from(implemented),
        3..=6 | 8 => 0,
        7 => {
            if implemented {
                caps.map_or(0, |caps| {
                    caps.iter()
                        .filter(|cap| matches!(cap.id, 1 | 2))
                        .fold(0, |mask, cap| mask | (1u32 << cap.id))
                })
            } else {
                0
            }
        }
        _ => return Err(VfsError::InvalidInput),
    })
}

impl DrmIoctl for VirtgpuGetparam {
    const CMD: u32 = iowr::<Self>(DRM_TYPE, 0x43);

    fn handle(_dev: &Card0, _file: &DrmFile, request: &mut Self) -> VfsResult<usize> {
        let device = primary_scanout()?;
        let implemented = device.has_virgl_commands() && !device.virgl_command_status().is_failed;
        let caps = device.virgl_capsets();
        let value = param_value(request.param, implemented, caps.as_deref())?;
        UserPtr::<u32>::from(request.value as usize)
            .write_vm(value)
            .map_err(|_| VfsError::BadAddress)?;
        Ok(0)
    }
}

impl DrmIoctl for VirtgpuGetCaps {
    const CMD: u32 = iowr::<Self>(DRM_TYPE, 0x49);

    fn handle(_dev: &Card0, _file: &DrmFile, request: &mut Self) -> VfsResult<usize> {
        if request.pad != 0 || !matches!(request.cap_set_id, 1 | 2) || request.size == 0 {
            return Err(VfsError::InvalidInput);
        }
        let bytes = device()?
            .virgl_capset(request.cap_set_id, request.cap_set_ver)
            .map_err(driver_error)?;
        let count = (request.size as usize).min(bytes.len());
        UserPtr::<u8>::from(request.addr as usize)
            .write_vm_slice(&bytes[..count])
            .map_err(|_| VfsError::BadAddress)?;
        Ok(0)
    }
}

impl DrmIoctl for VirtgpuMap {
    const CMD: u32 = iowr::<Self>(DRM_TYPE, 0x41);

    fn handle(_dev: &Card0, file: &DrmFile, request: &mut Self) -> VfsResult<usize> {
        if request.pad != 0 {
            return Err(VfsError::InvalidInput);
        }
        request.offset = lookup(file, request.handle)?.offset;
        Ok(0)
    }
}

impl DrmIoctl for VirtgpuResourceInfo {
    const CMD: u32 = iowr::<Self>(DRM_TYPE, 0x45);

    fn handle(_dev: &Card0, file: &DrmFile, request: &mut Self) -> VfsResult<usize> {
        let object = lookup(file, request.bo_handle)?;
        request.res_handle = object.resource_id;
        request.size = object.size as u32;
        request.blob_mem = 0;
        Ok(0)
    }
}

fn validate_create(request: &VirtgpuResourceCreate) -> VfsResult<()> {
    if request.bo_handle != 0
        || request.size == 0
        || request.size as usize > DUMB_BUFFER_MAX_SIZE
        || request.target > 8
        || request.format == 0
        || request.width == 0
        || (request.target != 0 && request.width > 16384)
        || request.height == 0
        || request.height > 16384
        || request.depth == 0
        || request.depth > 2048
        || request.array_size == 0
        || request.array_size > 2048
        || request.last_level > 14
        || !matches!(request.nr_samples, 0 | 1 | 2 | 4 | 8 | 16)
        || request.flags & !1 != 0
        || request.stride > request.size
    {
        return Err(VfsError::InvalidInput);
    }
    if request.target == 0 && request.width > request.size {
        return Err(VfsError::InvalidInput);
    }
    Ok(())
}

impl DrmIoctl for VirtgpuResourceCreate {
    const CMD: u32 = iowr::<Self>(DRM_TYPE, 0x44);

    fn handle_raw(dev: &Card0, file: &DrmFile, arg: UserPtr<Self>) -> VfsResult<usize> {
        let mut request = arg.read_vm().map_err(|_| VfsError::BadAddress)?;
        Self::handle(dev, file, &mut request)?;
        if arg.write_vm(request).is_err() {
            let _ = file.close_handle(request.bo_handle);
            return Err(VfsError::BadAddress);
        }
        Ok(0)
    }

    fn handle(dev: &Card0, file: &DrmFile, request: &mut Self) -> VfsResult<usize> {
        validate_create(request)?;
        device()?;
        let length = (request.size as usize).next_multiple_of(PAGE_SIZE_4K);
        let mut pages = GlobalPage::alloc_contiguous(length / PAGE_SIZE_4K, PAGE_SIZE_4K)
            .map_err(|_| VfsError::NoMemory)?;
        pages.zero();
        let pages = Arc::new(pages);
        let handle = next_id(&file.next_dumb_handle)?;
        let resource_id = next_id(&dev.next_resource_id)?;
        let params = VirglResourceCreate {
            resource_id,
            target: request.target,
            format: request.format,
            bind: request.bind,
            width: request.width,
            height: request.height,
            depth: request.depth,
            array_size: request.array_size,
            last_level: request.last_level,
            nr_samples: request.nr_samples,
            flags: request.flags,
        };
        let _admission = dev.context_admission.lock();
        let context = context(dev, file)?;
        // SAFETY: GlobalPage is contiguous, immovable, coherent guest RAM; this
        // allocation-only Arc covers the complete DMA extent without a device cycle.
        let backing = unsafe {
            GpuBacking::new(
                v2p(pages.start_va()).as_usize() as u64,
                length as u32,
                pages.clone(),
            )
        };
        let object = Arc::new(DumbBuffer {
            width: request.width,
            height: request.height,
            bpp: 32,
            pitch: request.stride,
            size: length as u64,
            offset: dev
                .next_offset
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |offset| {
                    offset.checked_add(DUMB_BUFFER_OFFSET_STRIDE)
                })
                .map_err(|_| VfsError::NoMemory)?,
            resource_id,
            device: Some(context.device.clone()),
            pages,
            reservation: Mutex::new(()),
            virgl: Some(VirglObject {
                params,
                context: context.clone(),
            }),
        });
        context
            .device
            .virgl_command(VirglCommand::CreateResource { params, backing })
            .map_err(driver_error)?;
        attach(&context, &object)?;
        file.dumbs.lock().insert(handle, object);
        request.bo_handle = handle;
        request.res_handle = resource_id;
        request.size = length as u32;
        Ok(0)
    }
}

fn validate_transfer(
    params: &VirglResourceCreate,
    backing_size: u64,
    request: &VirtgpuTransfer,
) -> VfsResult<()> {
    // Linux v6.12 rejects explicit strides for traditional non-blob resources.
    if request.stride != 0 || request.layer_stride != 0 {
        return Err(VfsError::InvalidInput);
    }
    let region = &request.box_;
    let width = (params.width >> request.level.min(31)).max(1);
    let height = (params.height >> request.level.min(31)).max(1);
    let depth = if matches!(params.target, 3) {
        (params.depth >> request.level.min(31)).max(1)
    } else {
        params.depth.max(params.array_size)
    };
    if request.level > params.last_level
        || region.w == 0
        || region.h == 0
        || region.d == 0
        || region.x.checked_add(region.w).is_none_or(|end| end > width)
        || region
            .y
            .checked_add(region.h)
            .is_none_or(|end| end > height)
        || region.z.checked_add(region.d).is_none_or(|end| end > depth)
        || request.offset as u64 >= backing_size
    {
        return Err(VfsError::InvalidInput);
    }
    // BGRA/XRGB protocol defaults are tightly packed mip rows. Other virgl
    // formats are bounded by the backend's attached iov and host format validation.
    let extent = if matches!(params.format, 1 | 2) {
        let stride = u64::from(width).checked_mul(4);
        let layer = stride.and_then(|stride| stride.checked_mul(u64::from(height)));
        stride.zip(layer).and_then(|(stride, layer)| {
            layer
                .checked_mul(u64::from(region.d - 1))?
                .checked_add(stride.checked_mul(u64::from(region.h - 1))?)?
                .checked_add(u64::from(region.w).checked_mul(4)?)
        })
    } else {
        Some(1)
    };
    let end = extent.and_then(|extent| extent.checked_add(u64::from(request.offset)));
    if end.is_none_or(|end| end > backing_size) {
        return Err(VfsError::InvalidInput);
    }
    Ok(())
}

fn transfer(
    dev: &Card0,
    file: &DrmFile,
    request: &VirtgpuTransfer,
    from_host: bool,
) -> VfsResult<()> {
    let object = lookup(file, request.bo_handle)?;
    let params = &object.virgl.as_ref().ok_or(VfsError::InvalidInput)?.params;
    validate_transfer(params, object.size, request)?;
    let _reservation = object.reservation.lock();
    let _admission = dev.context_admission.lock();
    let context = context(dev, file)?;
    attach(&context, &object)?;
    let params = VirglTransfer {
        context_id: context.id,
        resource_id: object.resource_id,
        box_: VirglBox {
            x: request.box_.x,
            y: request.box_.y,
            z: request.box_.z,
            width: request.box_.w,
            height: request.box_.h,
            depth: request.box_.d,
        },
        offset: u64::from(request.offset),
        level: request.level,
        stride: request.stride,
        layer_stride: request.layer_stride,
    };
    context
        .device
        .virgl_command(if from_host {
            VirglCommand::TransferFromHost(params)
        } else {
            VirglCommand::TransferToHost(params)
        })
        .map_err(driver_error)
}

fn validate_commands(commands: &[u32]) -> VfsResult<()> {
    let mut position = 0usize;
    while position < commands.len() {
        let length = (commands[position] >> 16) as usize;
        position = position
            .checked_add(1 + length)
            .ok_or(VfsError::InvalidInput)?;
        if position > commands.len() {
            return Err(VfsError::InvalidInput);
        }
    }
    Ok(())
}

impl DrmIoctl for VirtgpuExecbuffer {
    const CMD: u32 = iowr::<Self>(DRM_TYPE, 0x42);

    fn handle(dev: &Card0, file: &DrmFile, request: &mut Self) -> VfsResult<usize> {
        if request.flags != 0
            || request.ring_idx != 0
            || request.num_in_syncobjs != 0
            || request.num_out_syncobjs != 0
            || request.in_syncobjs != 0
            || request.out_syncobjs != 0
            || request.syncobj_stride != 0
            || request.size == 0
            || request.size as usize > MAX_COMMAND_BYTES
            || !request.size.is_multiple_of(4)
            || request.num_bo_handles as usize > MAX_SUBMISSION_OBJECTS
        {
            return Err(VfsError::InvalidInput);
        }
        let commands = UserPtr::<u32>::from(request.command as usize)
            .load_vm_vec(request.size as usize / 4)?;
        validate_commands(&commands)?;
        let handles = UserPtr::<u32>::from(request.bo_handles as usize)
            .load_vm_vec(request.num_bo_handles as usize)?;
        let mut objects = handles
            .iter()
            .map(|handle| lookup(file, *handle))
            .collect::<VfsResult<Vec<_>>>()?;
        objects.sort_unstable_by_key(|object| object.resource_id);
        objects.dedup_by_key(|object| object.resource_id);
        let _reservations = objects
            .iter()
            .map(|object| object.reservation.lock())
            .collect::<Vec<_>>();
        let _admission = dev.context_admission.lock();
        let context = context(dev, file)?;
        for object in &objects {
            attach(&context, object)?;
        }
        let bytes = bytemuck::cast_slice::<u32, u8>(&commands);
        context
            .device
            .virgl_command(VirglCommand::Submit {
                context_id: context.id,
                commands: bytes,
            })
            .map_err(driver_error)?;
        Ok(0)
    }
}

impl DrmIoctl for VirtgpuWait {
    const CMD: u32 = iowr::<Self>(DRM_TYPE, 0x48);

    fn handle(_dev: &Card0, file: &DrmFile, request: &mut Self) -> VfsResult<usize> {
        if request.flags & !1 != 0 {
            return Err(VfsError::InvalidInput);
        }
        let object = lookup(file, request.handle)?;
        let _reservation = if request.flags & 1 != 0 {
            object
                .reservation
                .try_lock()
                .ok_or(VfsError::ResourceBusy)?
        } else {
            object.reservation.lock()
        };
        if let Some(backend) = &object.device
            && backend.virgl_command_status().is_failed
        {
            return Err(VfsError::Io);
        }
        Ok(0)
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    fn request() -> VirtgpuResourceCreate {
        VirtgpuResourceCreate {
            target: 2,
            format: 1,
            width: 64,
            height: 64,
            depth: 1,
            array_size: 1,
            size: 16384,
            stride: 256,
            ..Default::default()
        }
    }

    #[def_test]
    fn ioctl_layouts_match_fixed_width_linux_carriers() {
        assert_eq!(core::mem::size_of::<VirtgpuMap>(), 16);
        assert_eq!(core::mem::size_of::<VirtgpuExecbuffer>(), 64);
        assert_eq!(core::mem::size_of::<VirtgpuResourceCreate>(), 56);
        assert_eq!(core::mem::size_of::<VirtgpuTransfer>(), 44);
        assert_eq!(core::mem::size_of::<VirtgpuGetCaps>(), 24);
        assert_eq!(iowr::<VirtgpuExecbuffer>(DRM_TYPE, 0x42), 0xc0406442);
    }

    #[def_test]
    fn query_capabilities_require_implemented_execution_and_reject_extensions() {
        let caps = [
            display::VirglCapsetInfo {
                id: 1,
                max_version: 1,
                max_size: 64,
            },
            display::VirglCapsetInfo {
                id: 4,
                max_version: 0,
                max_size: 64,
            },
        ];
        assert_eq!(param_value(1, false, Some(&caps)), Ok(0));
        assert_eq!(param_value(7, false, Some(&caps)), Ok(0));
        assert_eq!(param_value(1, true, Some(&caps)), Ok(1));
        assert_eq!(param_value(7, true, Some(&caps)), Ok(2));
        for param in 3..=6 {
            assert_eq!(param_value(param, true, Some(&caps)), Ok(0));
        }
        assert_eq!(param_value(8, true, Some(&caps)), Ok(0));
        assert_eq!(
            param_value(9, true, Some(&caps)),
            Err(VfsError::InvalidInput)
        );
    }

    #[def_test]
    fn resource_creation_rejects_unbounded_or_unsupported_inputs() {
        let mut request = request();
        assert_eq!(validate_create(&request), Ok(()));
        request.size = DUMB_BUFFER_MAX_SIZE as u32 + 1;
        assert_eq!(validate_create(&request), Err(VfsError::InvalidInput));
        request = self::request();
        request.bo_handle = 1;
        assert_eq!(validate_create(&request), Err(VfsError::InvalidInput));
        request = self::request();
        request.last_level = 32;
        assert_eq!(validate_create(&request), Err(VfsError::InvalidInput));
        request = self::request();
        request.flags = 2;
        assert_eq!(validate_create(&request), Err(VfsError::InvalidInput));
    }

    #[def_test]
    fn command_framing_rejects_truncated_payload_without_reading_past_it() {
        assert_eq!(validate_commands(&[0x00020001, 1, 2, 0]), Ok(()));
        assert_eq!(
            validate_commands(&[0x00020001, 1]),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(
            validate_commands(&[0xffff0001]),
            Err(VfsError::InvalidInput)
        );
    }

    #[def_test]
    fn transfer_bounds_reject_wrapping_geometry_and_backing_overrun() {
        let params = VirglResourceCreate {
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
        let mut request = VirtgpuTransfer {
            box_: VirtgpuBox {
                w: 64,
                h: 64,
                d: 1,
                ..Default::default()
            },
            stride: 0,
            ..Default::default()
        };
        assert_eq!(validate_transfer(&params, 16384, &request), Ok(()));
        request.box_.x = u32::MAX;
        assert_eq!(
            validate_transfer(&params, 16384, &request),
            Err(VfsError::InvalidInput)
        );
        request.box_.x = 0;
        request.offset = 4096;
        assert_eq!(
            validate_transfer(&params, 16384, &request),
            Err(VfsError::InvalidInput)
        );
        request.offset = 0;
        request.stride = u32::MAX;
        assert_eq!(
            validate_transfer(&params, 16384, &request),
            Err(VfsError::InvalidInput)
        );
    }

    #[def_test]
    fn nowait_reports_busy_for_the_requested_object_reservation() {
        let dev = Card0::new();
        let file = DrmFile::new();
        let object = super::super::tests::object();
        file.dumbs.lock().insert(1, object.clone());
        let reservation = object.reservation.lock();
        let mut request = VirtgpuWait {
            handle: 1,
            flags: 1,
        };
        assert_eq!(
            VirtgpuWait::handle(&dev, &file, &mut request),
            Err(VfsError::ResourceBusy)
        );
        drop(reservation);
        assert_eq!(VirtgpuWait::handle(&dev, &file, &mut request), Ok(0));
        request.flags = 2;
        assert_eq!(
            VirtgpuWait::handle(&dev, &file, &mut request),
            Err(VfsError::InvalidInput)
        );
    }
}
