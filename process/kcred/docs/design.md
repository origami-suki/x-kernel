# kcred design

## Purpose and background

`kcred` owns Linux/POSIX credential values and set-ID transition policy. `Cred`
contains real, effective, saved, and filesystem UID/GID values, supplementary
groups, and supported securebits. `kprocess` owns current-task lookup and
credential publication; `kvfs` consumes explicit credentials for access checks.
The namespace types model identity and parentage, not UID/GID mappings.

## Source scope and architecture

`src/lib.rs` re-exports `Cred`, `Uid`, `Gid`, `NamespaceId`, `UserNamespace`, and
the initial user namespace accessor, and publishes `initial_cred`.
`src/credentials/{mod,model,user,group,securebits}.rs` contains credential types
and transitions; `src/namespace.rs` owns namespace identity; `src/tests.rs` and
inline namespace tests cover these values.

```text
kprocess: prepare private Cred -> checked transition -> publish Arc<Cred>
                                                       |
kvfs <--------------------------- explicit stable snapshot
```

The committed object is immutable through `Arc`. `prepare` clones scalar fields
and shares the immutable `Arc<[Gid]>`. Successful callers publish the prepared
copy once; old readers keep their original snapshot. This crate does not perform
that publication or synchronize the current task's credential pointers.

## Execution context and concurrency

Pure ID getters and comparisons require only a valid credential borrow and do
not sleep. Creation, initial globals, and supplementary-group replacement need
allocation. There is no current-process, CPU-local, device-mapping, or scheduler
dependency here; early initialization is possible after the heap is available.
Do not assume allocation is safe in interrupt context. Shared publication locks
belong to `kprocess`; local changes require `&mut Cred`. `Once` protects global
initial values and `AtomicU64` with Relaxed ordering allocates namespace IDs;
those IDs do not publish other data and have no overflow check.

## Credential flows

Ordinary VFS checks use `fsuid`, `fsgid`, and sorted supplementary groups.
`for_access` creates a copy using real IDs for filesystem checks. A caller
implementing `AT_EACCESS` can use its existing committed credential instead;
`for_access` never changes the original object. `matches_real_credential_ids`
compares a caller's real UID/GID against all of the target's real/effective/saved
IDs, excluding filesystem IDs and supplementary groups. It is a predicate, not
a complete ptrace authorization policy.

Checked UID/GID operations use `euid == 0` as the current privilege approximation.
`set_uid`/`set_gid` allow privileged replacement of all four IDs; otherwise the
new effective/filesystem ID must match real or saved ID. The re-ID and res-ID
methods validate all requested changes before mutation. `None` means unchanged.
Re-ID updates saved IDs when real ID is supplied or a supplied effective ID
differs from the old real ID, and synchronizes filesystem ID even for a no-op
request. Res-ID preserves a true no-op, including its existing filesystem ID.

`set_fsuid`/`set_fsgid` always return the old ID, leaving state unchanged on a
rejected request. `set_supplementary_groups` sorts before publishing a new array
and preserves duplicates; `in_group` checks fsgid then uses binary search.
The caller limits group count and authorizes replacing groups.

`apply_exec` synchronizes saved/filesystem IDs to effective IDs and clears
KEEP_CAPS. KEEP_CAPS enable/disable rejects the locked flag, but there is no
capability set to preserve yet. Set-ID executable and file-capability effects
are not implemented by this transition.

## Decisions and resource lifecycle

Credentials are explicit VFS inputs to avoid reverse dependencies on task state.
Immutable snapshots keep one permission operation internally consistent without
holding process locks through pathname traversal. `KError` reports policy
failures directly. `initial_cred` and `initial_user_namespace` publish shared
root objects through `Once`. Other `Cred`/group arrays are freed when their last
owner releases them; no custom drop or secret erasure is implemented.

## Known limitations

There is no full capability set, LSM, user namespace ID mapping, subjective
credential override, or set-ID executable policy. `NGROUPS_MAX` enforcement is
external. Namespace IDs wrap at `u64` exhaustion. The group-array allocation
path is not recoverably fallible.
