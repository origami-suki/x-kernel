# drmdevice — Design

## Purpose

`drmdevice` implements a minimal resource-backed DRM/KMS device (`Card0`,
driver identity `simpledrm`) over the registered display scanout backend.
It speaks the Linux DRM user-space protocol — legacy libdrm ioctls and the
atomic-KMS paths modern compositors use — so unmodified user-space DRM
clients can drive the kernel's display output.

## Responsibilities

- Provide `Card0`, a `DeviceFileOps` character device: `ioctl` dispatch
  over the DRM ioctl set, `read` (vblank event consumption), `mmap`
  (dumb-buffer backing), `poll` (vblank/event readiness via `kpoll`
  `PollSet`), and node flags.
- Implement the legacy DRM ioctls: version/unique/set_version, getcap /
  setclientcap, auth / authmagic, dirtyfb, prime handle exchange,
  mode-card resource listing, get/set CRTC, page flip, encoder and
  connector queries, framebuffer create/remove, and dumb-buffer
  creation/allocation.
- Maintain the KMS object model: dumb buffers (`dumbs` map with handle
  and `mmap` offset allocators), framebuffers (`fbs` map), property blobs
  (user blobs plus synthesized system blobs such as the IN-FORMATS blob,
  built once behind double-checked locking), and mode state
  (`ModesetState`, `LegacyCrtcState`).
- Generate vblank events (`events` queue plus a monotonic `sequence`
  counter) and expose them to poll/read.
- Register display scanout backends: seed from the `kclass` display
  registry and expose `available()` for whether a scanout backend exists.
- Define the user-space wire types (`drm.rs`, layout-compatible with
  Linux `include/uapi/drm/drm.h`, derived `UserRead`/`UserWrite` for
  user-pointer copies) and the protocol constants (`consts.rs`: ioctl
  encoding, capability ids, property ids, format FourCCs, driver
  identity, limits).

## Non-Responsibilities

- No rendering or GPU command submission: this is KMS-only (modesetting
  and scanout); there is no render node and no GEM execbuf.
- No hardware access: mode state and pixels flow through the registered
  display scanout backend (`ClassDevice<DisplayDeviceImpl>`); the crate
  never touches MMIO.
- No connector hotplug notification: the connector set is fixed to the
  primary scanout.
- No DMA-BUF implementation: prime ioctls exchange handles at the
  protocol level without cross-device buffer sharing semantics.

## Scope

```text
io/drmdevice/
├── src/
│   ├── lib.rs      # module wiring, available()
│   ├── card0.rs    # Card0 device, ioctl handlers, KMS object state
│   ├── drm.rs      # uapi struct definitions, DrmIoctl dispatch trait
│   └── consts.rs   # ioctl numbers, caps, property ids, FourCCs, limits
└── Cargo.toml
```

## Architecture

```text
user space (libdrm / compositor)
   |  ioctl/read/mmap/poll on the DRM node
   v
Card0: DeviceFileOps (kvfs)
   |  DrmIoctl::handle / handle_raw  (drm.rs: user struct copies)
   |
   +-- dumb buffers -- offsets --> mmap (retained GlobalPage backing)
   +-- framebuffers / blobs ----> ModesetState / LegacyCrtcState
   +-- vblank events -----------> events queue + PollSet (kpoll)
   |
   v
primary ClassDevice<DisplayDeviceImpl> (display scanout backend, via kclass)
```

## Execution Context

- All entry points run in process syscall context through `kvfs`
  `DeviceFileOps`; vblank waits and device submission may block in task context, while poll integration is
  non-blocking readiness via `kpoll`.
- `read` copies queued vblank events to user buffers; the event queue is
  bounded (`MAX_EVENTS`).
- State is mutated only under its per-object locks; no task/workqueue is
  spawned by this crate.

## Concurrency Model

- Fine-grained state: separate `Mutex`es for modeset state, legacy CRTC
  state, dumb buffers, framebuffers, user blobs, system blobs,
  and the per-file event queue; id and offset allocation use fetch-add
  atomics.
- The IN-FORMATS system blob uses double-checked locking: an
  `AtomicU32` fast path (`Acquire`/`Release`) guards a `Mutex`-protected
  build-once section, so concurrent ioctls build it exactly once.
- The event queue plus `PollSet` pair event production (vblank ticks)
  with poll/read consumers; sequence numbers are monotonic via atomic.
- Lock ordering is local (each handler takes the locks it needs without
  nesting beyond blob-build and system-blob insert).

## Error Model

- Handlers return `VfsResult<usize>`; user-space mistakes (bad ids,
  small user arrays, unknown ioctls) map to VFS error codes.
- `report_user_array` implements the DRM user-array protocol: report the
  required count, then copy out at most the caller-provided capacity.
- Absent scanout backend: `available()` is false and mode ioctls fail;
  the device node may exist without a working display.

## External Boundary And Inputs

- All ioctl payloads arrive from user memory as raw bytes and are copied
  in/out through `UserPtr` with `bytemuck` pod types; struct layouts are
  fixed by the Linux uapi contract, and counts/pointers from user space
  are validated before copy-out (capacity checks in
  `report_user_array`).
- `mmap` maps dumb-buffer backing pages into the caller's address space
  through the VFS mmap mapper; backing pages are retained
  by VMA object ownership, so a mapping outliving its framebuffer keeps valid
  memory.
- The display backend side is trusted kernel-internal: scanout handles
  come from the `kclass` registry, not from user space.

## Design Decisions

- One fixed device (`Card0`, identity `simpledrm`): the goal is a
  protocol-compatible modesetting surface over the scanout backend, not a
  full DRM core; a static device keeps the object model small.
- Re-declare uapi structs instead of binding headers: the set is small
  and stable, `repr(C)` plus `bytemuck` gives checked copies, and the
  kernel stays header-free.
- Legacy plus atomic ioctls over one state model: compositors use
  atomic paths, simple clients use legacy CRTC calls; both mutate the
  same `ModesetState` so the two interfaces cannot diverge.
- Owned backing and mappings: user-space may keep a mapping after removing
  its handle/framebuffer. The VMA and backend DMA owners keep it alive until
  unmapping and fenced detach/unref complete; Drop queues without waiting.
- Build-once system blobs behind double-checked atomics: the IN-FORMATS
  blob is read on every atomic check but must be allocated once; the
  atomic fast path keeps per-frame ioctls lock-free on the hot path.

## Driver string query ABI

VERSION records each input capacity before reporting the full string length and
copies at most that capacity. A NULL pointer or zero capacity requests only the
length; no terminating NUL is added. GET_UNIQUE uses the distinct Linux rule:
copy only when the complete value fits, otherwise leave the buffer untouched
and report its required size. Invalid non-NULL copy destinations return EFAULT.
The minimal fixed driver identity remains unchanged; this does not implement
Linux per-open master or SET_VERSION-dependent bus-id lifetime.

Reference: Linux v6.12 drivers/gpu/drm/drm_ioctl.c (drm_copy_field, drm_getunique).
Regression: the same AArch64 ioctl probe on the supplied Linux and x-kernel,
including zero buffers, short-buffer canaries, full values and bad pointers.

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
