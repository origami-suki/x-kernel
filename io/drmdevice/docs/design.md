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
  state, dumb buffers, framebuffers, user blobs, system blobs, retained
  pages, and the event queue; id and offset allocation use fetch-add
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
  (`retained_pages`) so a mapping outliving its framebuffer keeps valid
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
- Dumb buffers with retained backing pages: user-space may keep a
  mapping after removing the framebuffer; retention turns that from a
  use-after-free hazard into a retained memory cost; repeated creation is unbounded until fenced reclamation exists.
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

This is an intermediate safety boundary, **not resource reclamation**.
All buffers submitted for creation now retain backing in the device-wide
`retained_pages` table, including never-mapped buffers and failed backend commands whose partial host
attachment cannot be disproved. Neither GEM_CLOSE nor
DESTROY_DUMB sends host detach/unref. Host resource teardown is deliberately
pending a fenced, explicit cleanup path that owns backing until completion;
no synchronous GPU waits occur in Drop or under file/framebuffer/MmSpace
locks. Physical mappings still depend on device-lifetime retention until the
separate owned-mapping module is integrated. Repeated create/close consumes
memory and host resources until device/kernel lifetime ends; there is no
bounded reclamation claim. DRM master/access control, PRIME/DMA-BUF, 3D UAPI
and context ownership are outside this change; existing PRIME behavior is not
real DMA-BUF support.
