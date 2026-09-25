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
  `retained_pages` — each behind its own `Mutex` (`Mutex<BTreeMap<u32,
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
- Dumb-buffer backing pages: retained in `retained_pages` so a user
  mapping that outlives its framebuffer still references live memory.

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
| T-03 | Use-after-free of a mapped dumb buffer | High — UAF on a user mapping | `rmfb`/close while a mmap lives | `retained_pages` keeps backing pages alive independent of the object table; retention is the mitigation. |
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
