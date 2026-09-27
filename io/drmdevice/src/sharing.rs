// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Same-device PRIME export/import and coherent anonymous DMA-buffer files.

use kvfs::{FileOperations, OpenFlags};

const SEEK_SET: i32 = 0;
const SEEK_END: i32 = 2;

use super::*;

/// One handle per device resource in an open description, with reverse lookup.
#[derive(Default)]
pub(super) struct GemHandles {
    entries: BTreeMap<u32, Arc<DumbBuffer>>,
    resource_handles: BTreeMap<u32, u32>,
}

impl GemHandles {
    pub(super) fn insert(&mut self, handle: u32, object: Arc<DumbBuffer>) {
        if let Some(previous) = self.entries.insert(handle, object.clone()) {
            self.resource_handles.remove(&previous.resource_id);
        }
        self.resource_handles.insert(object.resource_id, handle);
    }

    pub(super) fn remove(&mut self, handle: &u32) -> Option<Arc<DumbBuffer>> {
        let object = self.entries.remove(handle)?;
        self.resource_handles.remove(&object.resource_id);
        Some(object)
    }

    pub(super) fn clear(&mut self) {
        self.resource_handles.clear();
        self.entries.clear();
    }

    pub(super) fn get(&self, handle: &u32) -> Option<&Arc<DumbBuffer>> {
        self.entries.get(handle)
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &Arc<DumbBuffer>> {
        self.entries.values()
    }

    #[cfg(unittest)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    fn handle_for(&self, object: &DumbBuffer) -> Option<u32> {
        self.resource_handles.get(&object.resource_id).copied()
    }
}

/// Unlock before waking pollers; no callback runs under the resource mutex.
pub(super) struct ReservationGuard<'a> {
    guard: Option<ksync::MutexGuard<'a, ()>>,
    poll: &'a PollSet,
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        self.poll.wake();
    }
}

impl DumbBuffer {
    pub(super) fn reserve(&self) -> ReservationGuard<'_> {
        ReservationGuard {
            guard: Some(self.reservation.lock()),
            poll: &self.completion_poll,
        }
    }

    pub(super) fn try_reserve(&self) -> Option<ReservationGuard<'_>> {
        Some(ReservationGuard {
            guard: Some(self.reservation.try_lock()?),
            poll: &self.completion_poll,
        })
    }
}

struct DmaBuffer {
    object: Arc<DumbBuffer>,
}

struct DmaBufferOperations;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead)]
struct DmaSync {
    flags: u64,
}

const DMA_BUF_IOCTL_SYNC: u32 = iow::<DmaSync>(b'b', 0);
const DRM_CLOEXEC: u32 = OpenFlags::CLOSE_ON_EXEC.bits();
const DRM_RDWR: u32 = OpenFlags::READ_WRITE.bits();

fn sync_flags(flags: u64) -> VfsResult<()> {
    if flags & !7 != 0 || flags & 3 == 0 {
        return Err(VfsError::InvalidInput);
    }
    Ok(())
}

fn dma_buffer(file: &VfsFile) -> VfsResult<Arc<DmaBuffer>> {
    file.private_data_get::<DmaBuffer>()
        .ok_or(VfsError::InvalidInput)
}

impl FileOperations for DmaBufferOperations {
    fn llseek(&self, file: &VfsFile, offset: i64, whence: i32) -> VfsResult<u64> {
        if offset != 0 || !matches!(whence, SEEK_SET | SEEK_END) {
            return Err(VfsError::InvalidInput);
        }
        let offset = if whence == SEEK_END {
            dma_buffer(file)?.object.size
        } else {
            0
        };
        *file.position_lock() = offset;
        Ok(offset)
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        let object = dma_buffer(file)?.object.clone();
        let range =
            PhysAddrRange::from_start_size(v2p(object.pages.start_va()), object.pages.size());
        let offset = mapper.offset();
        mapper.map_physical_owned(range, offset, object)
    }

    fn ioctl(&self, file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        if cmd != DMA_BUF_IOCTL_SYNC {
            return Err(VfsError::NotATty);
        }
        let request = UserPtr::<DmaSync>::from(arg)
            .read_vm()
            .map_err(|_| VfsError::BadAddress)?;
        sync_flags(request.flags)?;
        let object = dma_buffer(file)?.object.clone();
        let _reservation = object.reserve();
        if object
            .device
            .as_ref()
            .is_some_and(|device| device.virgl_command_status().is_failed)
        {
            return Err(VfsError::Io);
        }
        // QEMU AArch64 backing is coherent guest RAM (also the GpuBacking
        // contract). A full memory barrier orders both CPU bracket boundaries;
        // the reservation drains prior fenced GPU work without retaining a
        // kernel lock across userspace CPU access. SYNC is not a mutual exclusion
        // session: clients must not submit GPU work before their END bracket.
        core::sync::atomic::fence(Ordering::SeqCst);
        Ok(0)
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        let Ok(buffer) = dma_buffer(file) else {
            return IoEvents::ERR;
        };
        if buffer
            .object
            .device
            .as_ref()
            .is_some_and(|device| device.virgl_command_status().is_failed)
        {
            return IoEvents::ERR;
        }
        if buffer.object.reservation.is_locked() {
            IoEvents::empty()
        } else {
            IoEvents::IN | IoEvents::OUT
        }
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.intersects(IoEvents::IN | IoEvents::OUT)
            && let Some(buffer) = file.private_data_get::<DmaBuffer>()
        {
            context.register(&buffer.object.completion_poll)?;
        }
        Ok(())
    }
}

fn export_flags(flags: u32) -> VfsResult<(OpenFlags, bool)> {
    if flags & !(DRM_CLOEXEC | DRM_RDWR) != 0 {
        return Err(VfsError::InvalidInput);
    }
    let mode = if flags & DRM_RDWR != 0 {
        OpenFlags::READ_WRITE
    } else {
        OpenFlags::empty()
    };
    Ok((mode, flags & DRM_CLOEXEC != 0))
}

fn export_file(object: Arc<DumbBuffer>, flags: u32) -> VfsResult<Arc<VfsFile>> {
    let (open_flags, _) = export_flags(flags)?;
    let mut exported = object.exported.lock();
    if let Some(file) = exported.upgrade() {
        return Ok(file);
    }
    let file = anon_inodefs::AnonInodeFs::global().get_file_with_size(
        "dma-buf",
        Arc::new(DmaBufferOperations),
        Arc::new(DmaBuffer {
            object: object.clone(),
        }),
        object.size,
        open_flags,
        kprocess::current_cred(),
    )?;
    *exported = Arc::downgrade(&file);
    Ok(file)
}

fn verify_device(object: &DumbBuffer) -> VfsResult<()> {
    let device = object.device.as_ref().ok_or(VfsError::InvalidInput)?;
    if primary_scanout()?.id() != device.id() {
        return Err(VfsError::InvalidInput);
    }
    Ok(())
}

pub(super) fn export_raw(
    _dev: &Card0,
    file: &DrmFile,
    arg: UserPtr<DrmPrimeHandle>,
) -> VfsResult<usize> {
    let mut request = arg.read_vm().map_err(|_| VfsError::BadAddress)?;
    let object = file
        .dumbs
        .lock()
        .get(&request.handle)
        .cloned()
        .ok_or(VfsError::InvalidInput)?;
    verify_device(&object)?;
    let exported = export_file(object, request.flags)?;
    let installation =
        kprocess::current_resources().install_file(exported, request.flags & DRM_CLOEXEC != 0)?;
    request.fd = installation.fd();
    if arg.write_vm(request).is_err() {
        let _ = installation.rollback();
        return Err(VfsError::BadAddress);
    }
    installation.commit();
    Ok(0)
}

fn import_object(file: &DrmFile, object: Arc<DumbBuffer>) -> VfsResult<(u32, bool)> {
    let mut handles = file.dumbs.lock();
    if let Some(handle) = handles.handle_for(&object) {
        return Ok((handle, false));
    }
    let handle = virtgpu::next_id(&file.next_dumb_handle)?;
    handles.insert(handle, object);
    Ok((handle, true))
}

fn publish_import(
    file: &DrmFile,
    object: Arc<DumbBuffer>,
    copyout: impl FnOnce(u32) -> VfsResult<()>,
) -> VfsResult<u32> {
    let _publication = file.prime_publication.lock();
    let (handle, is_new) = import_object(file, object)?;
    if let Err(error) = copyout(handle) {
        if is_new {
            let _ = file.close_handle_unpublished(handle);
        }
        return Err(error);
    }
    Ok(handle)
}

pub(super) fn import_raw(
    _dev: &Card0,
    file: &DrmFile,
    arg: UserPtr<DrmPrimeFdToHandle>,
) -> VfsResult<usize> {
    let mut request = arg.read_vm().map_err(|_| VfsError::BadAddress)?;
    let exported = kprocess::current_resources().get_file(request.0.fd)?;
    let object = dma_buffer(&exported)?.object.clone();
    verify_device(&object)?;
    publish_import(file, object, |handle| {
        request.0.handle = handle;
        arg.write_vm(request).map_err(|_| VfsError::BadAddress)
    })?;
    Ok(0)
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn repeated_import_reuses_handle_and_close_prunes_reverse_lookup() {
        let file = DrmFile::new();
        let object = super::super::tests::object();
        let (first, is_new) = import_object(&file, object.clone()).unwrap();
        assert_eq!(is_new, true);
        assert_eq!(
            import_object(&file, object.clone()).unwrap(),
            (first, false)
        );
        file.close_handle(first).unwrap();
        let (second, is_new) = import_object(&file, object).unwrap();
        assert_eq!(is_new, true);
        assert_eq!(second != first, true);
        assert_eq!(file.dumbs.lock().len(), 1);
    }

    #[def_test]
    fn import_failure_cannot_publish_or_remove_a_peer_successful_handle() {
        let file = DrmFile::new();
        let object = super::super::tests::object();
        let failed = publish_import(&file, object.clone(), |handle| {
            // A is paused after insertion: B cannot enter publication, although
            // the namespace mutex is free for lookups and no usercopy lock inversion.
            core::assert!(file.prime_publication.try_lock().is_none());
            core::assert!(file.dumbs.lock().get(&handle).is_some());
            Err(VfsError::BadAddress)
        });
        assert_eq!(failed, Err(VfsError::BadAddress));
        assert_eq!(file.dumbs.lock().len(), 0);
        let winner = publish_import(&file, object.clone(), |_| Ok(())).unwrap();
        // A later failing duplicate import must not roll back B's committed slot.
        assert_eq!(
            publish_import(&file, object, |_| Err(VfsError::BadAddress)),
            Err(VfsError::BadAddress)
        );
        assert_eq!(file.dumbs.lock().get(&winner).is_some(), true);
    }

    #[def_test]
    fn prime_export_flags_use_linux_open_bits_not_descriptor_flag_bits() {
        assert_eq!(DRM_CLOEXEC, 0x80000);
        assert_eq!(DRM_RDWR, 2);
        assert_eq!(export_flags(0x80002), Ok((OpenFlags::READ_WRITE, true)));
        assert_eq!(export_flags(0x80000), Ok((OpenFlags::empty(), true)));
        assert_eq!(export_flags(2), Ok((OpenFlags::READ_WRITE, false)));
        assert_eq!(export_flags(0), Ok((OpenFlags::empty(), false)));
        for flags in [1, 3, 0x80001, 0x80002 | 0x40000, u32::MAX] {
            assert_eq!(export_flags(flags), Err(VfsError::InvalidInput));
        }
    }

    #[def_test]
    fn dma_sync_only_accepts_direction_and_start_end_flags() {
        for flags in [1, 2, 3, 5, 6, 7] {
            assert_eq!(sync_flags(flags), Ok(()));
        }
        for flags in [0, 4, 8, u64::MAX] {
            assert_eq!(sync_flags(flags), Err(VfsError::InvalidInput));
        }
    }
}
