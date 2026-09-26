// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Root-only control and stopped snapshots for syscall observations.
use alloc::{borrow::Cow, sync::Arc};

use kvfs::{CommandFile, DirMapping, NodePermission, SimpleFile, SimpleFileOperation, SimpleFs};
pub(crate) fn add_root_entry(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "syscall_profile",
        SimpleFile::new_regular_with_permission(
            fs,
            NodePermission::from_bits_truncate(0o600),
            CommandFile::new(|op| match op {
                SimpleFileOperation::Read => kprocess::syscall_profile::snapshot()
                    .map(|text| Some(Cow::Owned(text.into_bytes()))),
                SimpleFileOperation::Write { data, .. } => {
                    kprocess::syscall_profile::command(data)?;
                    Ok(None)
                }
            }),
        ),
    );
}
