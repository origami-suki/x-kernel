# kexec security and reliability

## Scope, assets, and trust model

All of `src/lib.rs`, `src/loader.rs`, and `src/lru_cache.rs` is covered. Assets
are the target address space, executable identity, file-derived mapping
permissions/offsets, and owned argument/environment data. The loader trusts MM
and VFS ownership contracts but not executable bytes, interpreter paths, or ELF
metadata. Callers own syscall buffer copying and the process-wide exec protocol.

## External input boundaries

`ExecRequest::prepare` consumes owned path/argument/environment values and a
credential snapshot. String sources use current root/pwd and Exec lookup;
resolved sources retain their `Path`. Both check `Permission::MAY_EXEC` and open
the file. An absent display path is reconstructed via VFS. File reads supply ELF
headers, PT_LOAD addresses/sizes/flags, PT_INTERP bytes, and script text.
`SCRIPT_RECURSION_MAX` limits redirect count; interpreter text must be valid
UTF-8/C-string where applicable. Stack construction checks configured capacity.

There is no direct user-pointer, MMIO, DMA, firmware, FFI call, or assembly here.
The signing feature delegates TA verification to `tee_task_iface`; MM methods
own address validation, page population, and safe user-address writes.

## Unsafe inventory and invariants

There is no handwritten unsafe block/function/impl in this crate.
`#[self_referencing]` on `ElfCacheEntry` delegates generated self-reference
machinery to `ouroboros`: the owned byte vector must outlive its ELF header view,
and callers use generated borrowing/building APIs rather than extracting a
longer-lived reference. No local `SAFETY` block exists to audit for that generated
implementation; dependency soundness remains external.

`BinPrm` pins the executable and owns args/env/cred. Mapping code delegates VMA
construction and installation to `filemap`/`MmSpace`. Private LRU indices assume
valid bounded storage (production capacity 32). Safe indexing can panic on a
broken cache invariant; it does not create a raw-memory access boundary.

## Thread safety

`ELF_LOADER` protects cache operations, not the entire prepare-to-commit interval.
Files pinned by `PreparedExecImage` do not keep their cache entries present.
The caller exclusively borrows the target `MmSpace`; it must separately prevent
other threads from executing an obsolete image during a process exec protocol.
VFS work while holding loader/current-filesystem locks can block and must not
reenter the same lock chain.

## Threat analysis

| ID | Threat and asset | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Malformed executable disrupts loading | Medium | Invalid header, script loop, or bad interpreter text | Parser errors become executable/data errors; scripts stop after four redirects and text parsing returns `InvalidInput`. PT_INTERP short-read and page-offset assertions still panic. |
| T-02 | Excessive file-derived allocation/mapping | Medium | ELF advertises huge ranges or lengths | MM mapping errors propagate and stack payload is size-checked; header/interpreter allocation is not locally quota-bounded and arithmetic/assertion paths remain a denial-of-service risk. |
| T-03 | Resume of a destroyed image | High | Mapping or stack work fails after `MmSpace::clear` | Caller must treat post-clear failure as destructive and cannot resume old user state. The API has no rollback or phase-tagged error. |
| T-04 | Changed file uses stale metadata | Medium | Executable contents change after caching | Cache keys use path identity and hold files; no content-version invalidation exists. `clear_elf_cache` is explicit, not an automatic coherence protocol. |
| T-05 | Prepared entry disappears before commit | Medium | Concurrent loads evict it or explicit cache flush runs during lock gap | File references stay valid, but cache lookup uses `expect`; a panic remains possible. No per-request cache-entry pin is implemented. |
| T-06 | Wrong executable authority | High | Consumer substitutes display text for a resolved object or stale credentials | Resolved `Path` is retained and execute permission is checked with the supplied snapshot. Caller owns snapshot freshness and broader exec authorization. |
| T-07 | TA signature rejection | High | Verification under `tee_ta_sign` fails | Verification error maps to `PermissionDenied` before accepting a new cache entry; cache coherence remains a separate assumption. |

## Failure modes and effects (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity (1-4) | Handling |
|---|---|---|---|---|---|---|
| F-01 | Preparation fails | VFS resolution/open/permission or invalid image | `KResult` error before clear | Old image retained | 3 | Caller reports normal preparation failure. |
| F-02 | Script recursion exhausted | More than four redirects | `FilesystemLoop` | Exec rejected | 3 | Stop before commit. |
| F-03 | Post-clear failure | Mapping, population, write, or oversized stack | Partially rebuilt address space | Old image cannot resume | 1 | Caller handles destructive failure; no local rollback. |
| F-04 | Loader assertion fails | Segment offset mismatch, short PT_INTERP read, missing cache entry | Panic | Kernel service unavailable | 2 | Current assertions expose the assumption; do not claim recoverable validation. |
| F-05 | Stale cache | File changed without invalidation | Old parsed headers | Wrong load semantics | 2 | Explicit flush only; residual coherence limitation. |

## Failure management and privacy

Ordinary failures propagate through `KResult`. Invalid executable, invalid data,
invalid input, filesystem loop, argument-list size, MM/VFS, and optional signature
errors are distinct. Assertions and allocator failure have no local recovery.
Debug logs can expose executable/interpreter paths and mapping ranges. Arguments,
environment, and executable bytes are held in owned buffers; final release does
not explicitly erase them. The crate does not intentionally log their contents.

## Known limitations and audit checklist

- Preserve current-fs initialization and a consistent credential snapshot.
- Keep file-private mappings on the approved MM APIs and align both offsets.
- Never imply an arbitrary load error leaves the previous address space intact.
- Reassess cache lifetime across prepare/commit and file-change invalidation.
- Record all file-derived panic/allocation paths as residual risks.
- Keep generated self-reference ownership and dependency assumptions visible.
