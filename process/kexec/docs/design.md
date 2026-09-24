# kexec design

## Purpose and background

`kexec` loads user executables and prepares their address-space layout. It is an
MM client: `filemap` creates private file VMA/runtime pairs, `memspace::MmSpace`
installs mappings and coordinates page tables, and underlying MM owners manage
page cache and anonymous/COW pages. Syscall ABI copying, process publication,
credential commit, and post-exec policy belong to callers.

## Source scope and architecture

`src/lib.rs` re-exports `ExecSource`, `ExecRequest`, `BinPrm`,
`load_user_app_request`, and `clear_elf_cache`. `src/loader.rs` contains loading,
request preparation, the global `ElfLoader`, and `ElfCacheEntry`.
`src/lru_cache.rs` is a private capacity-bounded cache using vector storage and
indexed MRU/LRU links; its production capacity is 32.

```text
ExecRequest -> prepare -> BinPrm (owned args/env/cred + pinned VfsFile/Path)
    -> ELF cache / script redirects / PT_INTERP resolution
    -> PreparedExecImage
    -> MmSpace::clear -> signal trampoline -> main/interpreter PT_LOAD
    -> anonymous stack -> stack population/write -> anonymous heap
    -> (entry point, stack pointer)
```

## Execution context

Loading needs task context, allocator, VFS, MM, and scheduler/lock services.
String paths, scripts, and dynamic interpreters consult the current process
filesystem context; its root/pwd must be initialized. Resolved requests retain a
`kvfs::Path` and optionally a display path, but still check execute permission
against the supplied credential. Operations can block on filesystem reads and
sleepable locks and are unsuitable for interrupt context. Initial-process
loading may run from a kernel bootstrap task: `kprocess::current_fs_context`
then uses the initialized global `INIT_FS`, so no user Thread is required. A
user-task caller instead needs its process filesystem context still attached.
The caller supplies exclusive `&mut MmSpace` and owns execution handoff.

## Request preparation and image algorithms

`ExecRequest` owns arguments, environment, source, and a credential snapshot.
`prepare` resolves a string via `LookupIntent::Exec`, or reuses the resolved
`Path`, checks `MAY_EXEC`, obtains a display path when absent, and pins an opened
`VfsFile`. It does not receive or modify the target address space. Display paths
are used for script argument reconstruction, not as a replacement authority for
an already resolved object.

`ElfCacheEntry::load_file` reads up to 4096 bytes, parses ELF/program headers,
and reads an additional program-header range when necessary. `ouroboros`
encapsulates a self-referential header view over owned data. Under `tee_ta_sign`,
TA verification is requested before accepting an entry; failure maps to
`PermissionDenied`. Entries are keyed by `Path::ptr_eq`, not file content version.

A non-ELF head beginning with `#!` is parsed from at most the first 256 bytes.
The interpreter and optional argument prefix are followed by the script display
path and original argument tail. At most four redirects are permitted.
For PT_INTERP, the loader reads the specified bytes, requires a complete read and
NUL-terminated UTF-8 path, resolves it using the same credentials, and caches the
interpreter before replacing the address space.

Commit clears the address space, maps the signal trampoline, and maps PT_LOAD
segments for the main executable and optional dynamic interpreter. Segment
virtual/file starts are aligned down to 4 KiB; a mismatching page offset is
rejected as `InvalidExecutable`.
`new_file_private_vma` receives file offset and `p_offset + p_filesz` as the
file-data boundary, preserving prefix, zero-tail, and private-mapping semantics
through MM rather than implementing a second loader-specific memory owner.
Auxiliary entries describe the main image and interpreter base; the interpreter
entry is selected when present.

The main image always loads at `USER_SPACE_BASE`. The interpreter is
position-independent, so its load bias is a placement decision rather than an
ELF-provided constant: `load_segments` collects the PT_LOAD placement facts,
`load_range_for_segments` computes the page-rounded span the image occupies, and
`select_interpreter_base` asks `MmSpace::find_free_area` for the first free
start inside `[USER_INTERP_BASE, USER_HEAP_BASE)` that fits, with the reserved
size rounded up to the first segment's `p_align`. `USER_INTERP_BASE` is
therefore a preferred hint, not a fixed address; the heap and brk keep their
fixed placement below and above that window. An image whose pages fill the
window fails the load with `NoMemory`.

The outer flow maps an anonymous stack, builds argc/argv/env/aux data with
`app_stack_region`, checks its configured stack capacity, populates/writes pages,
and maps an anonymous heap. The returned tuple is entry address and user SP.

## Commit boundary and error handling

Preparation failures leave the previous image intact. `MmSpace::clear` is the
point after which the old image is gone. Mapping, initial-stack construction,
population, writes, and interpreter placement still return errors after that
point. The public result does not distinguish pre-clear and post-clear failures
and provides no rollback; callers must not assume an arbitrary `Err` permits
resuming the previous image. Do not describe this as a fully transactional exec
implementation.

File-derived ELF metadata reaches no assertion in this crate. A program-header
table that overflows or declares a zero entry size, a short PT_INTERP read, and a
PT_LOAD whose page offset disagrees with `p_offset` all become `KError` values.
Interpreter placement returns `NoMemory` when the interpreter window has no room.
The remaining panic sites are cache-lifetime `expect` calls, not input handling.

## Concurrency and cache lifecycle

The global sleepable `ELF_LOADER` mutex serializes each cache/prepare or commit
operation, including associated VFS work. It is released between preparation
and commit. `PreparedExecImage` pins files but does not itself pin cache entries;
other loads or `clear_elf_cache` can invalidate the later cache `expect`
assumption. There is no single transaction lock spanning the full public load.
The address space is independently protected by the caller's exclusive borrow.

Cache eviction/flush releases header buffers and file references. VMAs retain
mapped files through MM runtime ownership, so clearing the header cache does not
unmap an executable. `clear_elf_cache` also clears the TA header cache when the
signing feature is active. No file-change invalidation is integrated.

## Decisions and limitations

Common file-private VMA construction keeps ELF mappings aligned with mmap/COW
ownership. Stack and heap use normal anonymous MM APIs. Metadata work is prepared
before destructive commit where implemented; fallible MM and stack operations
remain after it. Known constraints include stale cache contents, cache-entry
lifetime assumptions across the lock gap, an interpreter search window that does
not fall back above `USER_HEAP_BASE`, and no complete exec credential/namespace
transaction within this crate.
