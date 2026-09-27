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
PRIME/DMA-BUF follows below; render discovery and 3D KMS are separate modules.

## Traditional virtgpu UAPI: first execution module

`card0.rs::virtgpu` carries the fixed-width Linux virtgpu ABI and implements
GETPARAM/CAPS, MAP, RESOURCE_CREATE/INFO, TRANSFER_TO/FROM_HOST, EXECBUFFER and
WAIT. It is a separate execution module; same-device PRIME/DMA-BUF follows
below, with render discovery and 3D KMS developed separately. The later scanout path below admits only supported 3D textures and prevents
legacy 2D upload from overwriting host-rendered pixels.
The former numeric PRIME placeholder is replaced by the real file implementation below.

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

## Same-device PRIME and DMA-buffer files

PRIME export now installs a real anonymous VfsFile descriptor holding a typed
DMA-buffer private object and an Arc GEM resource. CLOEXEC is installed through
the process fd table. Export copyout failure removes only that unpublished fd
when it still refers to the original file, preserving a concurrent close/reuse.
The GEM export cache is weak, preventing GEM/file reference cycles. The first
export establishes immutable file access mode, as in Linux dma_buf export.

Import rejects non-DMA-buffer descriptors and a different physical device.
An indexed resource-to-handle table atomically deduplicates repeated imports;
GEM_CLOSE prunes the reverse entry so re-import creates a fresh local handle.
New-handle copyout failure removes the new handle without destroying an existing
one. Same-device imported objects keep their original allocation/host resource,
so MAP_DUMB and GEM mmap remain valid after sender handle/file close. This
matches Linux v6.12 virtgpu PRIME's same-device fastpath (virtgpu_prime.c145–160)
and GEM mmap-offset lookup (virtgpu_gem.c102–114). Cross-device sharing remains
unsupported; no UUID/blob/PRIME external attachment capabilities are advertised.

Anonymous DMA-buffer fstat/SEEK_END report the real page-aligned allocation size.
DMA-buffer mmap treats its offset as a byte offset into the actual allocation
and retains the GEM through owned physical mapping. Generic mmap checks enforce
file read/write access before dispatch; mappings survive descriptor close and
SCM_RIGHTS uses the existing shared Arc<VfsFile> transport, without network edits.

DMA_BUF_IOCTL_SYNC validates direction/START/END flags, waits for the object's
prior fenced reservation, reports a poisoned backend as I/O failure, and orders
coherent QEMU AArch64 RAM with a full memory barrier. It does not retain a kernel
lock over userspace CPU access, and does not synthesize host-rendered readback:
clients still use TRANSFER_FROM_HOST where required by the resource protocol.
Poll waits share each GEM's completion PollSet and are woken after resource
reservation unlock; readiness represents completed synchronous fenced work,
with backend failure reported as ERR. It is the client's responsibility to
coordinate overlapping CPU/GPU access and bracket CPU writes before submission.
BLOB, cross-device imports, explicit sync_file/syncobj and fence flags remain
unsupported, rather than fabricating descriptors or signaling nonexistent work.

Primary references: Linux v6.12 virtgpu_prime.c, virtgpu_gem.c and dma-buf.c;
https://docs.kernel.org/driver-api/dma-buf.html#cpu-access-to-dma-buffer-objects
Actual immutable source snapshots and verified hashes are retained in evidence.

## Render discovery and host-rendered scanout

Primary/render devfs nodes use the same registered Card0 per actual physical
device, including separate devfs mounts. Their VfsFile private state remains
independent. Host resource IDs, context IDs and mmap lookup tokens use checked
kernel-global allocators, so a fresh adapter or imported resource cannot reuse
a still-live host ID/token. The registry is retired on device removal and drops
its adapter outside registry locks; backing owners never point back to it.

RenderNode whitelists rendering/query/PRIME/GEM operations and denies all KMS,
authentication and master requests before touching user memory. A healthy actual
command backend is required to publish renderD128. VERSION identifies the actual
virtio GPU transport/backend as virtio_gpu compatibility 0.0: explicit fence-fd
flags remain unsupported. This matches Mesa 25.1.9's real legacy path (driver
major must be zero; minor >=1 enables explicit fences). Other minimal 2D adapters
retain their prior simpledrm version. Identity is captured when the adapter is
constructed, so later device poisoning does not rename an existing description.
Legacy Mesa fence buffers legitimately use target=PIPE_BUFFER and array_size=0;
textures keep a positive array-size requirement. Resource/context cleanup is
armed only after successful registration, avoiding an unaccepted creation's
Drop targeting another existing host ID. Failed creation ownership remains the
backend's explicit responsibility under the owned-create contract.

Discovery obtains BDF and matching identity from kdevice, reads actual PCI vendor,
device, revision, class and subsystem IDs through the safe PCI configuration
mapping provider, and verifies those words against the registered identity.
It publishes the real physical parent, bound-driver and bus links, plus both
existing device numbers and class/dev/char links. It does not label the virtual
guest GPU with the host NVIDIA identity. The current project's segment-zero
AArch64 ECAM configuration is the supported PCI snapshot; unsupported mappings
or identity mismatches fail closed. Publication is a boot-time snapshot rather
than full sysfs/hotplug/udev emulation.

ADDFB2 accepts supported single-level, single-layer, nonmultisampled BGRA/XRGB
3D textures with matching geometry/pitch and retains their GEM Arc. A missing
traditional row stride uses the real tightly packed 32-bit resource layout.
Presentation reserves that object and calls the actual object's backend.
VirglCommand::Present never invokes the legacy 2D guest-page upload, which would
overwrite host-rendered pixels. Errors propagate; KMS state and requested flip
events are published only after successful fenced presentation. Attempted
scanout references survive uncertainty and host cleanup retains backing on error.

These frontend snapshots still require the independently accepted final driver
revision and real unit/lifetime/PRIME/3D/renderer runs. Compilation alone does not
prove DMA ordering, scanout pixels or Mesa/Weston/Chromium acceptance.

Primary KMS disable is a real fenced backend operation: SETCRTC with no FB,
atomic inactive/zero-FB, and RMFB of the active framebuffer blank host scanout
before clearing committed state or dropping active object references. Timeout
retains prior state/refs and returns the error without a successful event.
Legacy SETCRTC/page flip updates the same committed state read by GETCRTC;
GETCRTC reports disabled with no FB/mode after disable and reads retained mode
blob bytes for atomic modes. MODE_ID rejects a malformed/zero-dimension mode
blob before GPU side effects. RMFB rejects unknown IDs and drops retired
references outside the framebuffer mutex.

## Failed-copyout transactions and framebuffer file ownership

ADDFB2 publication holds a dedicated sleepable KMS transaction gate through
copyout, without holding the framebuffer/state mutex. Failure removes the
unpublished FB and its per-file owner entry before releasing the gate, so no
other KMS operation can activate a guessed unpublished ID. A successful FB is
owned by the creating open description, even though IDs remain device-global.
RMFB removes that owner entry. Final description release marks its surviving
FBs pending and schedules a prebuilt weak-Card work item: it performs no GPU wait
from VfsFile Drop or under an MmSpace lock. Task-context retirement fences a
real active scanout disable before removing global/active references. Failed
schedule/disable leaves explicit pending FB ownership/backing and emits an error;
it never silently drops a host DMA owner or claims reclaimed memory. Later
owner-close scheduling retries pending entries. Copyout rollback of an
unpublished FB requires no GPU transaction because the KMS gate prevented use.

PRIME import has a dedicated per-description publication gate through
lookup/insert/copyout/rollback. The GEM namespace mutex is released before
faultable copying. Other imports and GEM_CLOSE cannot publish/remove a handle
across that boundary; a failed duplicate import preserves an earlier committed
winner. PRIME export uses ProcessResources::install_file's exact installation
ticket, not Arc<VfsFile> equality. Failed copyout cannot close a replacement
created by dup2/re-export of the same cached file or a fork/table-unshare copy.
All fd rollback closes occur outside the table writer guard. Source-reproven
failures are fixed with deterministic transaction/token tests; guest fault/race
and framebuffer retirement acceptance remains separate pending execution.

## PRIME open-flag ABI correction

Linux drm.h defines DRM_CLOEXEC as O_CLOEXEC, not FD_CLOEXEC. Export validation
now derives DRM_CLOEXEC and DRM_RDWR from the repository's OpenFlags: AArch64
0x80000 and 2. Standard 0x80002 requests produce a writable descriptor with
CLOEXEC installation; former bit 1 and unrelated open flags are rejected.
The parsed CLOEXEC boolean is passed to the existing exact-installation ticket,
so the fd limit/rollback policy is unchanged. The independent original probe's
EINVAL at standard flags is retained as a real failed guest result; C callers,
backend sources and prior failed evidence were not modified to conceal it.
