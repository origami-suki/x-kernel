// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File mapping callback implementation.
use alloc::sync::Arc;

use kerrno::KResult;
use khal::paging::{MappingFlags, PageSize};
use kvfs::{MmapMapper, VfsFile};
use memaddr::{MemoryAddr, PhysAddrRange, VirtAddr};
use memfs::shmem;
use memspace::{InvalidateHandle, MmObserver, VmArea, VmRuntimeRef};

use crate::{
    FileMappingMode,
    runtime::{FileRuntimeContext, FileVmaSpec, build_file_runtime, build_file_vma},
};

struct FileMapper {
    req: FileMmapRequest,
    mode: FileMappingMode,
    runtime: Option<VmRuntimeRef>,
}

impl FileMapper {
    fn into_runtime(self) -> KResult<VmRuntimeRef> {
        self.runtime.ok_or(kerrno::KError::NoSuchDevice)
    }
}

pub struct FileMmapRequest {
    pub start: VirtAddr,
    pub length: usize,
    pub offset: usize,
    pub page_size: PageSize,
    pub flags: MappingFlags,
    pub max_flags: MappingFlags,
    pub file: Arc<VfsFile>,
    pub mm_id: u64,
    pub observer: MmObserver,
    pub invalidate: InvalidateHandle,
}

impl MmapMapper for FileMapper {
    fn offset(&self) -> usize {
        self.req.offset
    }

    fn map_physical(&mut self, mut range: PhysAddrRange) -> kvfs::VfsResult<()> {
        range.start += self.req.offset;
        if range.is_empty() {
            return Err(kvfs::VfsError::InvalidInput);
        }
        self.req.length = self
            .req
            .length
            .min(range.size().align_down(self.req.page_size));
        self.runtime = Some(VmRuntimeRef::new_linear(
            self.req.start.as_usize() as isize - range.start.as_usize() as isize,
        ));
        Ok(())
    }

    fn map_physical_owned(
        &mut self,
        range: PhysAddrRange,
        buffer_offset: usize,
        owner: Arc<dyn Send + Sync>,
    ) -> kvfs::VfsResult<()> {
        // The device resolves req.offset as its token. Only the explicitly
        // supplied resource-relative byte offset participates in translation.
        let page_size = self.req.page_size as usize;
        let length = self.req.length;
        if range.start >= range.end
            || self.req.page_size != PageSize::Size4K
            || !range.start.is_aligned_4k()
            || !buffer_offset.is_multiple_of(page_size)
            || length == 0
            || !length.is_multiple_of(page_size)
            || buffer_offset
                .checked_add(length)
                .is_none_or(|end| end > range.size())
        {
            return Err(kvfs::VfsError::InvalidInput);
        }
        let physical_start = range
            .start
            .checked_add(buffer_offset)
            .ok_or(kvfs::VfsError::InvalidInput)?;
        let runtime =
            VmRuntimeRef::new_linear_owned(self.req.start, physical_start, length, owner)?;
        self.runtime = Some(runtime);
        Ok(())
    }

    fn map_file_backed(&mut self) -> kvfs::VfsResult<()> {
        self.runtime = Some(build_file_runtime(
            self.req.start,
            self.req.length,
            &self.req.file,
            self.req.offset,
            self.req.page_size,
            self.mode,
            FileRuntimeContext {
                mm_id: self.req.mm_id,
                observer: &self.req.observer,
                invalidate: self.req.invalidate.clone(),
            },
        )?);
        Ok(())
    }

    fn map_anonymous_shared(&mut self) -> kvfs::VfsResult<()> {
        self.runtime = Some(VmRuntimeRef::new_anon_shared(
            self.req.start,
            self.req.length,
            self.req.page_size,
        )?);
        Ok(())
    }
}

/// Resolves a file-backed `mmap` request into VMA metadata and runtime ops.
///
/// This keeps the VFS callback-driven `MmapMapper` object inside
/// `filemap` so syscall code only expresses Linux-facing policy:
/// shared/private mode, offset, and target address range.
fn mmap_file(req: FileMmapRequest, mode: FileMappingMode) -> KResult<(VmArea, VmRuntimeRef)> {
    let file = req.file.clone();
    let inode = file.inode().inode();
    let path = file
        .path()
        .absolute_path()
        .ok()
        .map(|it| it.as_str().into());
    let mut mapper = FileMapper {
        req,
        mode,
        runtime: None,
    };
    file.mmap(&mut mapper)?;
    let length = mapper.req.length;
    let start = mapper.req.start;
    let page_size = mapper.req.page_size;
    let flags = mapper.req.flags;
    let max_flags = mapper.req.max_flags;
    let offset = mapper.req.offset as u64;
    let runtime = mapper.into_runtime()?;
    let vma = build_file_vma(
        FileVmaSpec {
            start,
            len: length,
            page_size,
            flags,
            max_flags,
            offset,
            inode,
            path,
        },
        runtime.backing_info(),
    );
    Ok((vma, runtime))
}

/// Resolves a Linux `MAP_SHARED`-style file-backed mapping request.
pub fn mmap_shared_file(req: FileMmapRequest) -> KResult<(VmArea, VmRuntimeRef)> {
    if req.flags.contains(MappingFlags::WRITE) {
        shmem::check_shared_writable_mapping_allowed(req.file.path())?;
    }
    mmap_file(req, FileMappingMode::Shared)
}

/// Resolves a Linux `MAP_PRIVATE`-style file-backed mapping request.
pub fn mmap_private_file(req: FileMmapRequest) -> KResult<(VmArea, VmRuntimeRef)> {
    mmap_file(req, FileMappingMode::Private)
}

#[cfg(unittest)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use khal::paging::PagingError;
    use ksync::Mutex;
    use memaddr::{PAGE_SIZE_4K, PhysAddr};
    use memspace::MmSpace;
    use unittest::def_test;

    use super::*;

    static NEXT_FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

    fn test_mapper(token: usize, length: usize) -> FileMapper {
        // shmem test files occupy a shared namespace, including parallel tests.
        let fixture_id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let fixture_name = alloc::format!("owned-physical-mapper-test-{fixture_id}");
        let mm = Arc::new(Mutex::new(
            MmSpace::new_empty_user(VirtAddr::from_usize(0x1000), PAGE_SIZE_4K * 16)
                .expect("test mm"),
        ));
        let observer = MmObserver::new(&mm);
        let invalidate = observer.invalidate_handle();
        let mm_id = mm.lock().mm_id();
        FileMapper {
            req: FileMmapRequest {
                start: VirtAddr::from_usize(0x4000),
                length,
                offset: token,
                page_size: PageSize::Size4K,
                flags: MappingFlags::READ | MappingFlags::USER,
                max_flags: MappingFlags::READ | MappingFlags::USER,
                file: crate::test_support::page_cache_file(&fixture_name),
                mm_id,
                observer,
                invalidate,
            },
            mode: FileMappingMode::Shared,
            runtime: None,
        }
    }

    #[def_test]
    fn owned_physical_mapper_uses_buffer_offset_independent_of_token() {
        let mut mapper = test_mapper(usize::MAX & !(PAGE_SIZE_4K - 1), PAGE_SIZE_4K * 2);
        let owner: Arc<dyn Send + Sync> = Arc::new(());
        let weak = Arc::downgrade(&owner);
        mapper
            .map_physical_owned(
                PhysAddrRange::from_start_size(PhysAddr::from_usize(0x100000), PAGE_SIZE_4K * 3),
                PAGE_SIZE_4K,
                owner,
            )
            .expect("resolve owned buffer independently of token");
        assert_eq!(mapper.req.length, PAGE_SIZE_4K * 2);
        let runtime = mapper.into_runtime().expect("owned runtime");
        let start = VirtAddr::from_usize(0x4000);
        let mut mm = MmSpace::new_empty_user(start, PAGE_SIZE_4K * 4).expect("mapping mm");
        mm.map(
            start,
            PAGE_SIZE_4K * 2,
            MappingFlags::READ | MappingFlags::USER,
            false,
            runtime,
        )
        .expect("install owned mapping");
        assert!(weak.upgrade().is_some());
        assert_eq!(
            mm.page_table().query(start).expect("physical address").0,
            PhysAddr::from_usize(0x101000)
        );
        mm.clear();
        assert!(weak.upgrade().is_none());
        assert!(matches!(
            mm.page_table().query(start),
            Err(PagingError::NotMapped)
        ));
    }

    #[def_test]
    fn owned_physical_mapper_rejects_bad_extents_without_truncation() {
        let resource =
            PhysAddrRange::from_start_size(PhysAddr::from_usize(0x100000), PAGE_SIZE_4K * 2);
        for (length, offset) in [
            (0, 0),
            (1, 0),
            (PAGE_SIZE_4K, 1),
            (PAGE_SIZE_4K * 3, 0),
            (PAGE_SIZE_4K * 2, PAGE_SIZE_4K),
            (PAGE_SIZE_4K, usize::MAX & !(PAGE_SIZE_4K - 1)),
        ] {
            let mut mapper = test_mapper(0xdead000, length);
            let owner: Arc<dyn Send + Sync> = Arc::new(());
            let weak = Arc::downgrade(&owner);
            assert_eq!(
                mapper.map_physical_owned(resource, offset, owner),
                Err(kvfs::VfsError::InvalidInput)
            );
            assert_eq!(mapper.req.length, length);
            assert!(mapper.runtime.is_none());
            assert!(weak.upgrade().is_none());
        }
        let mut mapper = test_mapper(0, PAGE_SIZE_4K);
        mapper.req.start = VirtAddr::from_usize(usize::MAX & !(PAGE_SIZE_4K - 1));
        assert_eq!(
            mapper.map_physical_owned(resource, 0, Arc::new(())),
            Err(kvfs::VfsError::InvalidInput)
        );
        mapper.req.start = VirtAddr::from_usize(0x4000);
        assert_eq!(
            mapper.map_physical_owned(
                PhysAddrRange::from_start_size(PhysAddr::from_usize(0x100001), PAGE_SIZE_4K),
                0,
                Arc::new(())
            ),
            Err(kvfs::VfsError::InvalidInput)
        );
    }
}
