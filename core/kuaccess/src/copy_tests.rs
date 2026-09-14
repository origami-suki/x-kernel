// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Demand faults must reach a sleepable backing provider from user copies.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use khal::paging::{PageSize, PageTableMut};
use ksync::Mutex;
use memaddr::{PAGE_SIZE_4K, VirtAddrRange};
use memspace::{
    FaultContext, ForkCloneTarget, InvalidateHandle, MmSpace, VmBackingInfo, VmRuntimeOps,
    VmRuntimeRef,
    backend::{FaultCompletionResult, PrivateBackend},
};
use unittest::def_test;

use super::*;

struct SleepableBacking {
    pages: PrivateBackend,
    faults: AtomicUsize,
}

impl VmRuntimeOps for SleepableBacking {
    fn backing_info(&self) -> VmBackingInfo {
        VmRuntimeOps::backing_info(&self.pages)
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, table: &mut PageTableMut) -> KResult {
        VmRuntimeOps::map(&self.pages, range, flags, table)
    }

    fn unmap(&self, range: VirtAddrRange, table: &mut PageTableMut) -> KResult {
        VmRuntimeOps::unmap(&self.pages, range, table)
    }

    fn on_protect(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        table: &mut PageTableMut,
    ) -> KResult<MappingFlags> {
        VmRuntimeOps::on_protect(&self.pages, range, flags, table)
    }

    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        table: &mut PageTableMut,
    ) -> FaultCompletionResult {
        // Exercise the same context requirement as IRQ-driven block I/O, without
        // depending on disk timing. A remembered wake permits a deterministic park.
        let mut wait = ktask::future::PreparedTaskWait::prepare()?;
        if khal::context::in_exception_context() {
            return Err(KError::BadState);
        }
        wait.waker().wake_by_ref();
        wait.park();
        self.faults.fetch_add(1, Ordering::Relaxed);
        VmRuntimeOps::handle_fault(&self.pages, ctx, flags, table)
    }

    fn relocate_for_mremap(
        &self,
        start: VirtAddr,
        id: u64,
        space: &Arc<Mutex<MmSpace>>,
        invalidate: Option<InvalidateHandle>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        VmRuntimeOps::relocate_for_mremap(&self.pages, start, id, space, invalidate)
    }

    fn clone_for_fork(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old: &mut PageTableMut,
        new: &mut PageTableMut,
        target: ForkCloneTarget<'_>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        VmRuntimeOps::clone_for_fork(&self.pages, range, flags, old, new, target)
    }
}

#[def_test(user)]
fn user_copies_resolve_sleepable_backing_and_preserve_permissions() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let backing = Arc::new(SleepableBacking {
        pages: PrivateBackend::new(start, PageSize::Size4K),
        faults: AtomicUsize::new(0),
    });
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    space
        .lock()
        .map(
            start,
            PAGE_SIZE_4K * 3,
            flags,
            false,
            VmRuntimeRef::new_file_private(backing.clone()),
        )
        .unwrap();

    // The first copy crosses two initially absent pages without touching them
    // from userspace first. The third page is first faulted by copy-to-user.
    let address = start.as_usize() + PAGE_SIZE_4K - 8;
    let mut output = [MaybeUninit::<u8>::uninit(); 16];
    osvm::read_vm_bytes(address as *const u8, &mut output).unwrap();
    assert_eq!(backing.faults.load(Ordering::Relaxed), 2);
    for byte in output {
        // SAFETY: successful read_vm_bytes initialized every output byte.
        assert_eq!(unsafe { byte.assume_init() }, 0);
    }
    let write_address = start.as_usize() + PAGE_SIZE_4K * 3 - 8;
    osvm::write_vm_bytes(write_address as *mut u8, &[0x5a; 8]).unwrap();
    assert_eq!(backing.faults.load(Ordering::Relaxed), 3);
    let mut readback = [MaybeUninit::<u8>::uninit(); 8];
    osvm::read_vm_bytes(write_address as *const u8, &mut readback).unwrap();
    for byte in readback {
        // SAFETY: successful read_vm_bytes initialized every output byte.
        assert_eq!(unsafe { byte.assume_init() }, 0x5a);
    }

    space
        .lock()
        .protect(
            start,
            PAGE_SIZE_4K * 3,
            MappingFlags::USER | MappingFlags::READ,
        )
        .unwrap();
    assert_eq!(
        osvm::write_vm_bytes(write_address as *mut u8, &[1]),
        Err(MemError::NoAccess)
    );
    space.lock().unmap(start, PAGE_SIZE_4K * 3).unwrap();
    assert_eq!(
        osvm::read_vm_bytes(address as *const u8, &mut output),
        Err(MemError::NoAccess)
    );
}
