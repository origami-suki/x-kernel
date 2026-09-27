# drmdevice — Security And Reliability

## Scope

This analysis covers the entire crate: `src/lib.rs` (module wiring and
`available()`), `src/card0.rs` (`Card0` device, ioctl dispatch, KMS
object state), `src/drm.rs` (user-space wire types and `DrmIoctl` copy
wrappers), and `src/consts.rs` (protocol constants). No modules are
excluded; `src/tests/` content is test-only and reachable exclusively
through the audited entry points. External responsibility: pixel transport
and mode state execution are delegated to the registered display scanout
backend (`ClassDevice<DisplayDeviceImpl>` from `kclass`), which is trusted
kernel-internal code audited in its own crate.

## Trust Model

`Card0` is a user-facing DRM device: every ioctl payload is untrusted
user input. The crate's posture is copy-in with validated types, validate
object ids against kernel-held tables before use, copy out only within
user-declared capacities, and never let user data select a kernel
address. The display scanout backend on the other side is trusted
kernel-internal state.

## External Boundaries

- **Ioctl payloads**: `repr(C)` structs copied from user pointers via
  `UserPtr` + `bytemuck` pod types (`UserRead`/`UserWrite`). User
  pointers inside structs (`UserPtr<u8>`, `UserPtr<u32>` arrays) are
  dereferenced only by the copy helpers, never cast to kernel pointers.
- **User arrays**: `report_user_array` implements the DRM two-phase
  protocol — report the required count, then copy out at most the
  caller-provided capacity. A small capacity is not an error; it
  truncates.
- **mmap**: dumb-buffer offsets created by this crate map to its
  retained backing pages; arbitrary offsets do not select arbitrary
  kernel memory.
- **Object ids** (fb id, dumb handle, blob id, crtc ids): looked up in
  kernel-owned `BTreeMap`s; unknown ids return errors, never wild
  references.

## Unsafe Code

None. The crate contains no `unsafe` blocks, no FFI, and no inline
assembly. All user-memory access goes through the safe `UserRead` /
`UserWrite` abstractions of `posix_types`, and all MMIO-free device
interaction goes through the `DisplayDevice` trait object.

## Protected Resources

- Kernel object tables: `dumbs`, `fbs`, `blobs`, `system_blobs`,
  each behind its own `Mutex` (`Mutex<BTreeMap<u32,
  ...>>`), keyed by kernel-allocated ids.
- Mode state: `state: Mutex<ModesetState>` — the active mode/scanout
  configuration that GETCRTC, SETCRTC, page-flip, and the atomic ioctls
  read and mutate; must stay internally consistent so a rejected atomic
  commit leaves the previous mode intact.
- Legacy CRTC state: `legacy_crtc: Mutex<LegacyCrtcState>` — the
  legacy-libdrm CRTC view that must agree with `state` after every
  legacy ioctl.
- Event queue: `events: Mutex<VecDeque<DrmEventVblank>>` capped at
  `MAX_EVENTS`, woken through `poll_rx: PollSet`.
- The event queue and `PollSet`: user `read` drains bounded queued
  events (`MAX_EVENTS`); poll registration is reference-counted and
  revoked on drop.
- Backing pages: VMA and GEM object references keep CPU mappings alive;
  the backend's separate allocation owner covers DMA until fenced teardown.

## Invariants

- Every user-pointer dereference goes through a `UserPtr` copy helper
  sized by the struct or array bound — no raw pointer arithmetic on user
  addresses.
- Copy-out never exceeds the user-declared capacity (`report_user_array`
  truncates to `count`).
- Id allocation is kernel-side (`fetch_add` atomics); user input never
  supplies internal ids except as lookup keys.
- The IN-FORMATS system blob is built once (double-checked atomic +
  mutex) and shared by `Arc`; user blobs are immutable after creation.
- The event queue is bounded; `read` cannot drain unbounded kernel
  memory into a user buffer regardless of the requested length.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Malformed ioctl struct (bad counts, forged pointers) | High in general — mitigated to error returns | User passes hostile `DrmModeCardRes` etc. | Copy-in/copy-out only through `posix_types::UserPtr::read_vm` / `write_vm` / `write_vm_slice` (on `UserRead`/`UserWrite` derived pods, see `src/drm.rs`), wrapped per ioctl by `DrmIoctl::handle_raw` (`src/card0.rs`); object ids resolved against kernel tables in the `DrmIoctl::handle` impls; unknown ids error out. No user value is ever used as a kernel address. |
| T-02 | Integer overflow in user-supplied arithmetic (pitches, offsets, sizes) | Medium — wrong object sizes | Crafted dumb-buffer / framebuffer parameters | Creation parameters validated in the create-dumb and add-fb handlers (`src/card0.rs`) against the dimension and format limits from `src/consts.rs`; a rejected creation returns `VfsError::InvalidInput`. Mmap offsets are kernel-allocated (`next_offset` stride allocator), never user sums. |
| T-03 | Use-after-free of a mapped dumb buffer | High — UAF on a user mapping | `rmfb`/close while a mmap lives | Owned VMA/GEM references preserve CPU backing, while backend allocation ownership lasts through fenced host cleanup. |
| T-04 | Unbounded kernel memory exposure via `read` | Medium — information disclosure | Oversized read buffer | `Card0::read` (`src/card0.rs`, `DeviceFileOps::read`) copies only queued `DrmEventVblank` records; the queue is capped at `MAX_EVENTS` (`src/consts.rs`) with fixed-size records. |
| T-05 | Bloom of fake vblank events to wake compositors early | Low — scheduling noise | `DRM_IOCTL_WAIT_VBLANK` misuse | Sequence numbers and event structs are kernel-generated; user cannot forge queue entries. |
| T-06 | Blob contents confusing the atomic path | Low — rejected modes, no memory unsafety | User uploads garbage blob data | Blobs are opaque byte vectors validated by the mode parsers at use; failures reject the atomic commit. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | Unknown object id in an ioctl | VFS error code returned | Compositor retries or fails cleanly | 4 | Table lookup misses map to errors. |
| F-02 | User array capacity smaller than object count | Truncated copy-out with reported count | Client re-queries with a larger array | 4 | DRM two-phase protocol implemented. |
| F-03 | No scanout backend registered | Mode ioctls fail; `available()` false | DRM node present but inert | 3 | Explicit availability reporting. |
| F-04 | Dumb-buffer allocation failure | Error to user | Compositor falls back | 3 | Checked allocation. |

## Known Limitations

- KMS-only: no render node, no GEM execbuf; prime ioctls are protocol
  stubs without cross-device buffer sharing.
- The connector set is fixed to the primary scanout; no hotplug events.
- Concurrent ioctls take fine-grained locks per object table; a
  compositor issuing many simultaneous ioctls serializes on those locks
  (performance, not safety).

## Audit Checklist

- New ioctls keep the pattern: copy-in via pod types, validate ids and
  counts, kernel-allocate ids, copy-out within declared capacity.
- Any new mmap-visible offset comes from the kernel offset allocator.
- New object tables retain backing memory as long as user mappings can
  reference them.

## String copy boundaries

VERSION string lengths are untrusted capacities on input; output lengths do not
increase the permitted copy size. Zero/NULL queries never dereference user memory.
GET_UNIQUE must not partially overwrite a short buffer: it reports the size and
copies only a complete value. Both ioctls retain UserPtr copying and EFAULT for
invalid destinations. Guest canaries verify bytes beyond the advertised capacity
remain unchanged. Fixed identity/master-state limitations are not resolved by
this buffer-safety correction.

## Per-open DRM state and intermediate lifetime boundary

`DeviceFileOps::open` installs a typed `Arc<DrmFile>` in `VfsFile` private
state. Independent opens have independent dumb/GEM handle namespaces, event
queues, PollSets, and client capabilities. Existing dup/fork semantics share
one `Arc<VfsFile>` and therefore the same state. Client capabilities 2
(universal planes) and 3 (atomic) accept boolean values; unsupported requests
return EINVAL. Capabilities are recorded, while the existing minimal KMS
visibility policy is unchanged. GEM_CLOSE and DESTROY_DUMB remove only the
calling file's handle; unknown handles return EINVAL. Device resource IDs,
framebuffer IDs, offsets, modeset state and property blobs remain device-wide.

Framebuffers and the last attempted active scanout hold an `Arc<DumbBuffer>`.
Removing a handle cannot invalidate either. Present copies the reference out
of the framebuffer lock before the device call; mmap copies the object out
of the file lock before entering the mapper. Event delivery targets only the
requesting description and wakes that description's PollSet after unlocking.
Final file release clears handles/events/caps and does no device I/O or wait.

The original per-open prerequisite retained all backing device-wide as an
intermediate boundary. This execution module removes that table only in
combination with owned mappings and the driver's explicit fenced cleanup.
Both 2D and 3D creation transfer an allocation-only owner before host DMA;
final object Drop queues cleanup, while failure retains the backend owner.
Mapping, framebuffer and active scanout references preserve the object. A
framebuffer remains until RMFB, and the last attempted scanout remains until
replacement; these live owners are distinct from leaked close-only backing.
No synchronous GPU waits occur in Drop or under file/framebuffer/MmSpace locks.
PRIME/DMA-BUF, render discovery and 3D KMS are subsequent execution modules;
existing numeric PRIME behavior is not real DMA-BUF support.

## Traditional virtgpu UAPI: first execution module

`card0.rs::virtgpu` carries the fixed-width Linux virtgpu ABI and implements
GETPARAM/CAPS, MAP, RESOURCE_CREATE/INFO, TRANSFER_TO/FROM_HOST, EXECBUFFER and
WAIT. It is a separate execution module; PRIME/DMA-BUF, render discovery and
3D KMS are subsequent modules. ADDFB2 rejects 3D objects in this intermediate
state to prevent the legacy 2D upload path overwriting host-rendered pixels.
No existing numeric PRIME placeholder is evidence of sharing support.

Each file lazily creates one legacy context, protected by a dedicated sleepable
context-admission gate. Host context/resource IDs are device-global, while GEM
handles remain per description. Resource/context Drops only enqueue prebuilt
backend cleanup, without GPU waits or user-memory access. GPU backing ownership
uses only `Arc<GlobalPage>`; it does not retain the display device, preventing a
registry/resource/device cycle. Object references retain their originating
context and allocation. The backend independently pins the allocation before
host submission and retains it on failure until fenced detach/unref succeeds;
poisoned backends report I/O errors rather than successful waits.

EXECBUFFER copies at most 1 MiB of aligned commands and 1024 handle entries
before acquiring reservations. Header payload lengths must consume exactly the
copied dwords. Handles are resolved to Arcs, sorted by device resource ID and
deduplicated before locking per-object reservations, avoiding duplicate and
multi-object lock inversions. Transfer/submit hold these dedicated sleepable
reservations until fenced completion; general handle/context/FB/MmSpace state
locks are never held during GPU waits. WAIT takes the requested object's same
reservation; NOWAIT returns EBUSY when it is occupied. Thus a synchronous API
still preserves exclusion between concurrent clients and truthful busy status.
Commands remain an untrusted renderer protocol, interpreted by the host's
per-context resource namespace; this is not a guest-side virgl shader validator.

Creation dimensions/mip levels/samples/flags, 8 MiB backing limits, transfer
regions and arithmetic are validated before dispatch. Resource creation copyout
failure revokes the unpublished handle and queues cleanup. BLOB, HOST_VISIBLE,
CROSS_DEVICE, CONTEXT_INIT, syncobj and fence/ring flags remain unsupported.
3D_FEATURES and CAPSET_QUERY_FIX require an implemented, healthy command backend;
SUPPORTED_CAPSET_IDS includes only advertised virgl/virgl2 sets. Opaque caps are
copied from actual cached device bytes with the requested length bounded by the
snapshot, never synthesized. Exact ioctl carrier sizes match local DRM headers.

DRM mmap now supplies the actual physical allocation range, byte offset zero
and an Arc object owner to `map_physical_owned`; lookup tokens never participate
in physical address arithmetic. VMA split/fork and final TLB invalidation retain
the owner through the separately validated mapping contract. Both 2D and 3D
objects use allocation-only backend ownership and deferred fenced cleanup;
there is no permanent DRM backing table. Dumb creation requires the owned
scanout entry point and fails closed if a backend lacks that contract. This
module depends on the exact final tested driver lifecycle implementation, and
its provisional display contract alone does not establish guest execution.

The dedicated sleepable KMS reservation serializes modeset/FB mutations while
state locks are dropped before presentation. Presentation errors propagate to
the ioctl; flip events and committed legacy/atomic state are updated only after
success. The attempted scanout Arc is conservatively retained on failure until
backend fenced cleanup can prove it no longer participates in host scanout.

## Linux v6.12 ABI correction

GETPARAM writes exactly a 32-bit value to its 64-bit pointer carrier, matching
`virtgpu_ioctl.c::virtio_gpu_getparam_ioctl` (`sizeof(int)`, lines 123–124), so
an adjacent user canary or a page boundary is not overwritten. Traditional
non-blob TRANSFER_TO/FROM_HOST rejects nonzero stride/layer_stride, matching
that file's lines 250–252 and 309–311. Protocol-default BGRA/XRGB transfer
extents use checked mip row/layer arithmetic; the attached iov remains the
backend boundary for other host-validated formats.

Reference: https://github.com/torvalds/linux/blob/v6.12/drivers/gpu/drm/virtio/virtgpu_ioctl.c
Actual source snapshots, line references and hashes are retained with module evidence.
