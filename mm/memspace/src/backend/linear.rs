// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linear mapping backend.
use alloc::sync::Arc;

use kerrno::{KError, KResult};
use khal::paging::{MappingFlags, PageSize, PageTableMut, PagingError};
use ksync::Mutex;
use memaddr::{MemoryAddr, PhysAddr, PhysAddrRange, VirtAddr, VirtAddrRange};

use crate::{
    FaultContext, ForkCloneTarget, InvalidateHandle, MmSpace, VmBackingInfo, VmBackingKind,
    backend::{BackendOps, FaultCompletion, FaultCompletionResult, map_paging_err, pages_in},
    vma::VmRuntimeOps,
};

/// Linear mapping backend.
///
/// The offset between the virtual address and the physical address is
/// constant. Legacy mappings use a signed offset; owned mappings use checked
/// virtual/physical extents and retain the physical backing owner.
#[derive(Clone)]
pub struct LinearBackend {
    translation: LinearTranslation,
}

#[derive(Clone)]
enum LinearTranslation {
    Legacy(isize),
    Owned {
        virtual_range: VirtAddrRange,
        physical_start: PhysAddr,
        // Never release backing before the last VMA runtime is torn down.
        _owner: Arc<dyn Send + Sync>,
    },
}

impl LinearBackend {
    pub fn new(offset: isize) -> Self {
        Self {
            translation: LinearTranslation::Legacy(offset),
        }
    }

    /// Retains a physical resource for a checked, page-aligned virtual extent.
    ///
    /// The owner must keep the physical range allocated, without owning this
    /// runtime or its address space. Split and fork clones share this owner.
    /// Destruction can run under the mm lock: it must not block, wait for a
    /// device, or re-enter that mm; enqueue deferred device cleanup elsewhere.
    /// Returns `InvalidInput` for empty, unaligned or overflowing extents.
    pub fn new_owned(
        start: VirtAddr,
        physical_start: PhysAddr,
        length: usize,
        owner: Arc<dyn Send + Sync>,
    ) -> KResult<Self> {
        let virtual_range =
            VirtAddrRange::try_from_start_size(start, length).ok_or(KError::InvalidInput)?;
        if length == 0
            || !start.is_aligned_4k()
            || !physical_start.is_aligned_4k()
            || !length.is_multiple_of(PageSize::Size4K as usize)
            || physical_start.as_usize().checked_add(length).is_none()
        {
            return Err(KError::InvalidInput);
        }
        Ok(Self {
            translation: LinearTranslation::Owned {
                virtual_range,
                physical_start,
                _owner: owner,
            },
        })
    }

    fn validate_range(&self, range: VirtAddrRange) -> KResult {
        if let LinearTranslation::Owned { virtual_range, .. } = &self.translation
            && (range.is_empty()
                || range.start < virtual_range.start
                || range.end > virtual_range.end)
        {
            return Err(KError::InvalidInput);
        }
        Ok(())
    }

    pub fn clone_for_fork_runtime(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pgtbl: &mut PageTableMut,
        _new_pgtbl: &mut PageTableMut,
        _new_aspace: &Arc<Mutex<MmSpace>>,
        _invalidate: Option<InvalidateHandle>,
    ) -> KResult<Self> {
        Ok(self.clone())
    }

    fn pa(&self, va: VirtAddr) -> PhysAddr {
        match &self.translation {
            LinearTranslation::Legacy(offset) => {
                let pa = (va.as_usize() as isize)
                    .checked_sub(*offset)
                    .expect("linear address translation overflow");
                assert!(pa >= 0, "linear address translation produced negative PA");
                PhysAddr::from(pa as usize)
            }
            LinearTranslation::Owned {
                virtual_range,
                physical_start,
                ..
            } => {
                // Constructor and operation boundary checks ensure this is in range.
                *physical_start + (va - virtual_range.start)
            }
        }
    }
}

impl BackendOps for LinearBackend {
    fn page_size(&self) -> PageSize {
        PageSize::Size4K
    }

    fn backing_info(&self) -> VmBackingInfo {
        VmBackingInfo::new(VmBackingKind::Linear, self.page_size())
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        self.validate_range(range)?;
        let pa_range = PhysAddrRange::from_start_size(self.pa(range.start), range.size());
        debug!("Linear::map: {range:?} -> {pa_range:?} {flags:?}");
        if matches!(&self.translation, LinearTranslation::Owned { .. }) {
            // map_region leaves a partial prefix on failure. Roll back only
            // pages installed by this call, keeping pre-existing PTEs intact.
            for vaddr in pages_in(range, PageSize::Size4K)? {
                if let Err(err) = pgtbl.map(vaddr, self.pa(vaddr), PageSize::Size4K, flags) {
                    for mapped in
                        pages_in(VirtAddrRange::new(range.start, vaddr), PageSize::Size4K)?
                    {
                        pgtbl
                            .unmap(mapped)
                            .expect("owned map rollback must unmap its installed PTE");
                    }
                    // The caller may release the final owner as soon as we return.
                    pgtbl.finish();
                    return Err(map_paging_err(err));
                }
            }
            return Ok(());
        }
        pgtbl
            .map_region(range.start, |va| self.pa(va), range.size(), flags, false)
            .map_err(map_paging_err)?;
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        self.validate_range(range)?;
        let pa_range = PhysAddrRange::from_start_size(self.pa(range.start), range.size());
        debug!("Linear::unmap: {range:?} -> {pa_range:?}");
        for vaddr in pages_in(range, PageSize::Size4K)? {
            match pgtbl.unmap(vaddr) {
                Ok(_) | Err(PagingError::NotMapped) => {}
                Err(err) => return Err(map_paging_err(err)),
            }
        }
        Ok(())
    }

    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult {
        let addr = ctx.address().align_down(self.page_size());
        self.validate_range(
            VirtAddrRange::try_from_start_size(addr, PageSize::Size4K as usize)
                .ok_or(KError::InvalidInput)?,
        )?;
        let expected = self.pa(addr);
        match pgtbl.query(addr) {
            Ok((paddr, page_flags, _)) => {
                if paddr != expected {
                    return Err(KError::BadAddress);
                }
                if !page_flags.contains(ctx.access_flags()) {
                    pgtbl.protect(addr, flags).map_err(map_paging_err)?;
                }
            }
            Err(PagingError::NotMapped) => {
                pgtbl
                    .map(addr, expected, self.page_size(), flags)
                    .map_err(map_paging_err)?;
            }
            Err(_) => return Err(KError::BadAddress),
        }
        Ok(FaultCompletion::from_populate((1, None)))
    }
}

impl VmRuntimeOps for LinearBackend {
    fn backing_info(&self) -> VmBackingInfo {
        BackendOps::backing_info(self)
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        BackendOps::map(self, range, flags, pgtbl)
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        BackendOps::unmap(self, range, pgtbl)
    }

    fn on_protect(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult<MappingFlags> {
        BackendOps::on_protect(self, range, flags, pgtbl)
    }

    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult {
        BackendOps::handle_fault(self, ctx, flags, pgtbl)
    }

    fn relocate_for_mremap(
        &self,
        _new_start: VirtAddr,
        _new_mm_id: u64,
        _aspace: &Arc<Mutex<MmSpace>>,
        _invalidate: Option<InvalidateHandle>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        Err(KError::OperationNotSupported)
    }

    fn clone_for_fork(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pgtbl: &mut PageTableMut,
        _new_pgtbl: &mut PageTableMut,
        _target: ForkCloneTarget<'_>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        Ok(Arc::new(self.clone()))
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn owned_linear_constructor_rejects_invalid_and_overflowing_extents() {
        let top_page = usize::MAX & !(PageSize::Size4K as usize - 1);
        for (virtual_start, physical_start, length) in [
            (0x4000, 0x100000, 0),
            (0x4001, 0x100000, 0x1000),
            (0x4000, 0x100001, 0x1000),
            (0x4000, 0x100000, 1),
            (top_page, 0x100000, 0x1000),
            (0x4000, top_page, 0x1000),
        ] {
            let owner: Arc<dyn Send + Sync> = Arc::new(());
            let weak = Arc::downgrade(&owner);
            assert!(matches!(
                LinearBackend::new_owned(
                    VirtAddr::from_usize(virtual_start),
                    PhysAddr::from_usize(physical_start),
                    length,
                    owner
                ),
                Err(KError::InvalidInput)
            ));
            assert!(weak.upgrade().is_none());
        }
    }
}
