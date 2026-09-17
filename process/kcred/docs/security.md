# kcred security and reliability

## Scope, assets, and trust boundaries

All credential and namespace source modules are covered. Assets are real,
effective, saved, filesystem IDs, supplementary groups, securebits, and namespace
identity. Syscall adapters copy user data, decode sentinel IDs, validate prctl
arguments, authorize group updates, and cap group count. `kcred` checks local
set-ID rules; `kprocess` publishes complete credential snapshots; KVFS performs
DAC using explicit credentials and filesystem metadata.

No direct user pointer, file-content input, MMIO, DMA, device, firmware, FFI, or
assembly is consumed. Incoming IDs and group lists can represent user requests,
but arrive as owned/borrowed Rust values after the syscall boundary.

## Unsafe inventory and invariants

There is no unsafe code. Mutable operations require exclusive access to a
prepared `Cred`; committed `Arc<Cred>` snapshots remain unchanged. All checked
set-ID rejection branches precede mutation. Supplementary groups remain sorted,
and exec synchronizes saved/filesystem IDs and clears KEEP_CAPS. These semantic
invariants complement Rust memory safety; Rust alone does not validate policy.

## Thread safety

`Cred` and group arrays are immutable when shared. Thread-owner locks and
publication of `Arc<Cred>` are external. One access/namei operation should use
one snapshot throughout rather than resampling current credentials.
Initial objects use `Once`; namespace IDs use Relaxed atomic increments only
for number allocation, not inter-object ordering.

## Threat analysis

| ID | Threat and asset | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Unauthorized UID/GID transition | High | Nonprivileged request selects an ID outside the permitted old-ID set | Checked setters reject with `OperationNotPermitted` before mutation. Privilege still uses effective UID zero rather than capabilities. |
| T-02 | Mixed-identity access decision | High | A multi-step operation reads credentials again after a concurrent commit | Caller must retain one `Arc<Cred>`; immutable snapshots and explicit VFS inputs enable this but cannot force all consumers to do so. |
| T-03 | Incorrect group membership | High | An update bypasses sorted-group construction or is not authorized | Private storage and `set_supplementary_groups` preserve ordering; group-count and permission checks remain in the syscall layer. |
| T-04 | Locked securebit bypass | High | Request changes KEEP_CAPS after lock | Enable/disable check the lock and return `OperationNotPermitted`; exec clears KEEP_CAPS as its defined transition. Capability-set semantics remain unsupported. |
| T-05 | Wrong access/ptrace identity policy | High | Caller confuses real/effective/filesystem roles | `for_access` and asymmetric `matches_real_credential_ids` expose specific predicates; the caller must combine them with its complete policy. |
| T-06 | Global root object altered | High | Caller attempts to mutate the committed initial object | Shared `Arc<Cred>` plus private fields require an unpublished mutable copy; publishing altered credentials remains an authorized kernel-owner operation. |
| T-07 | Namespace ID collision | Medium | Relaxed `AtomicU64` allocation wraps | No wrap prevention is implemented. Consumers must not treat these IDs alone as an unbounded authorization capability. |

## Failure modes and effects (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity (1-4) | Handling |
|---|---|---|---|---|---|---|
| F-01 | Checked ID/securebit rejected | Policy disallows change | `OperationNotPermitted`, old object intact | Syscall fails | 3 | Propagate error and do not publish the copy. |
| F-02 | Filesystem ID rejected | Target not in allowed set | No mutation, old ID returned | Caller sees Linux-style old value | 4 | Do not reinterpret the scalar return as unconditional success. |
| F-03 | Group-array allocation fails | Memory pressure | Replacement cannot complete | Task operation unavailable | 2 | Global allocator policy; no local allocation error result. |
| F-04 | Bad snapshot publication | Upper layer publishes partial/inconsistent state | Wrong credential view | Authorization failure | 1 | Complete transitions before process-owner commit. |

## Failure management, privacy, and limitations

The crate uses `KResult` for checked policy failures and returns old IDs for
filesystem-ID setters. It does not log credential data or automatically retry.
IDs and group lists are sensitive process metadata; consumers decide how to
expose them. Final drop frees storage without explicit zeroization.

Effective-UID-zero checks are not full capability authorization. User namespace
mapping, file capabilities, and subjective override remain unsupported. Direct
construction and supplementary replacement assume trusted caller authorization;
no namespace or privilege model should be inferred from a numeric ID alone.

## Audit checklist

- Keep every rejected checked transition before mutation and publication.
- Preserve real/effective/saved/filesystem distinctions and res-ID no-op behavior.
- Keep group sorting, duplicates, and external size/permission checks aligned.
- Use one committed snapshot per multi-step permission operation.
- Reaudit KEEP_CAPS and exec together when adding capability sets.
- Do not turn namespace IDs into unchecked authorization tokens.
