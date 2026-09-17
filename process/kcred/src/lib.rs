// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX credential snapshots and Linux-style set-ID transitions.
//!
//! Use [`Cred::prepare`] to derive an unpublished copy, apply checked changes,
//! and let the process owner publish it. [`initial_cred`] supplies the shared root
//! credential. Namespace types model identity/parentage, not complete ID mapping.
//!
//! # Example
//!
//! ```
//! use kcred::Cred;
//! let original = Cred::root();
//! let mut prepared = original.prepare();
//! prepared.set_uid(1000).unwrap();
//! assert_eq!(prepared.euid(), 1000);
//! assert_eq!(original.euid(), 0);
//! ```

#![no_std]
#![warn(missing_docs)]

extern crate alloc;

use alloc::sync::Arc;

use klazy::Once;

mod credentials;
mod namespace;

pub use credentials::{Cred, Gid, Uid};
pub use namespace::{NamespaceId, UserNamespace, initial_user_namespace};

static INITIAL_CRED: Once<Arc<Cred>> = Once::new();

/// Returns the credentials shared by the initial task and kernel-owned VFS objects.
pub fn initial_cred() -> Arc<Cred> {
    Arc::clone(INITIAL_CRED.call_once(|| Arc::new(Cred::root())))
}

#[cfg(unittest)]
mod tests;
