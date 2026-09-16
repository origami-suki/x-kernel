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
  `DeviceFileOps`; blocking is not performed — poll integration is
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
  use-after-free hazard into a bounded memory cost.
- Build-once system blobs behind double-checked atomics: the IN-FORMATS
  blob is read on every atomic check but must be allocated once; the
  atomic fast path keeps per-frame ioctls lock-free on the hot path.
