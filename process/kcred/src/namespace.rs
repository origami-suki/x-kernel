// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Namespace identity and user namespace ownership.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use klazy::Once;

static INIT_USER_NS: Once<Arc<UserNamespace>> = Once::new();

/// Kernel namespace identifier allocated from a global counter.
///
/// This is used for namespace identities that are externally rendered as
/// `/proc/[pid]/ns/*` inode-style identifiers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct NamespaceId(u64);

impl Default for NamespaceId {
    fn default() -> Self {
        Self::new()
    }
}

impl NamespaceId {
    /// Allocates the next namespace ID.
    ///
    /// The relaxed `u64` counter has no wrap check; uniqueness is limited
    /// to allocations before counter exhaustion. This is not an authorization token.
    pub fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the raw ID value.
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for NamespaceId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// User namespace.
///
/// This is the owner namespace for credentials and namespace-scoped privilege
/// checks. The current implementation models identity and parentage only.
#[derive(Debug)]
pub struct UserNamespace {
    id: NamespaceId,
    parent: Option<Arc<UserNamespace>>,
}

impl UserNamespace {
    fn new_root() -> Self {
        Self {
            id: NamespaceId::new(),
            parent: None,
        }
    }

    /// Returns the namespace ID.
    pub fn id(&self) -> NamespaceId {
        self.id
    }

    /// Returns the parent user namespace, if this is not the root user namespace.
    pub fn parent(&self) -> Option<&Arc<UserNamespace>> {
        self.parent.as_ref()
    }
}

/// Returns the global initial user namespace.
pub fn initial_user_namespace() -> Arc<UserNamespace> {
    Arc::clone(INIT_USER_NS.call_once(|| Arc::new(UserNamespace::new_root())))
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, def_test};

    use super::*;

    #[def_test]
    fn test_namespace_id_monotonic() {
        let id1 = NamespaceId::new();
        let id2 = NamespaceId::new();
        let id3 = NamespaceId::new();
        assert!(id2.as_u64() > id1.as_u64());
        assert!(id3.as_u64() > id2.as_u64());
    }

    #[def_test]
    fn test_namespace_id_display() {
        let id = NamespaceId::new();
        let displayed = alloc::format!("{}", id);
        assert!(!displayed.is_empty());
    }

    #[def_test]
    fn test_initial_user_namespace_is_singleton() {
        let first = initial_user_namespace();
        let second = initial_user_namespace();
        assert!(Arc::ptr_eq(&first, &second));
    }
}
