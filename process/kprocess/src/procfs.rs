// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use ktask::KtaskRef;

use crate::{Pid, Process, Tid, lookup};

fn has_representative_task(process: &Arc<Process>) -> bool {
    lookup::representative_task_for_process(process).is_ok()
}

/// Returns the process identities that should appear in `/proc`.
pub fn visible_processes() -> alloc::vec::Vec<Arc<Process>> {
    lookup::published_processes()
        .into_iter()
        .filter(has_representative_task)
        .collect()
}

/// Resolves the representative task used for a `/proc/<pid>` lookup.
///
/// # Errors
///
/// Returns `kerrno::KError::NoSuchProcess` when the requested published target
/// is absent or no longer has the required live backing.
pub fn process_task(pid: Pid) -> KResult<KtaskRef> {
    let process = lookup::published_process(pid)?;
    lookup::representative_task_for_process(&process).map_err(|_| KError::NoSuchProcess)
}

/// Resolves a thread task used for `/proc/<pid>/task/<tid>` lookups.
///
/// # Errors
///
/// Returns `kerrno::KError::NoSuchProcess` when a nonzero TID has no live
/// published task. TID zero selects the current task.
pub fn thread_task(tid: Tid) -> KResult<KtaskRef> {
    lookup::task(tid)
}

/// Returns the published thread IDs currently visible under `/proc/<pid>/task`.
pub fn thread_ids(process: &Arc<Process>) -> alloc::vec::Vec<Tid> {
    process.threads()
}

/// Counts currently published threads without allocating a thread-ID snapshot.
///
/// Uses the same membership view as [`thread_ids`]. Call from task context;
/// the count is observational and does not pin membership against clone/exit.
pub fn thread_count(process: &Process) -> usize {
    process.thread_count()
}
