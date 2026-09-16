# block — Design

## Purpose

`block` is the X-Kernel block core. It defines driver-private I/O
operations, the published `Gendisk`, the `BlockDevice` identified by
`dev_t`, and owns the single resident block-device registry.

## Linux Object Mapping

| Linux | X-Kernel | Ownership |
|---|---|---|
| `struct block_device_operations` | `BlockDeviceOperations` | driver/backend algorithm, open-mode callback |
| `struct gendisk` | `Gendisk` | disk name, major/minor range, state, operations |
| `struct block_device` | `BlockDevice` | `dev_t`, disk view, capacity |
| `bd_holder` / exclusive `bdev_open` | `BlockDeviceClaim` | exclusive holder lifetime |
| `add_disk` / `del_gendisk` | same-named functions | explicit publish/unpublish |
| `blkdev_get_no_open` | `lookup_block_device` | canonical `dev_t` lookup |
| `set_capacity` | `BlockDevice::set_capacity` | mutable media capacity publication |
| `set_disk_ro` / `get_disk_ro` | `BlockDevice::set_disk_read_only` / `is_read_only` | canonical disk state |

`BlockDeviceOperations` does not inherit the generic `Device`, because disk
identity belongs to `Gendisk`; the backend only expresses I/O plus the
open/release/ioctl layer of Linux block-device operations. `Gendisk`
composes that algorithm object, and `BlockDevice` composes `Gendisk`,
without duplicating driver identity. `BlockOpenMode` corresponds to Linux
`blk_mode_t` and is passed from the KVFS opened-file mode into open/ioctl;
concrete drivers such as loop do not build their own open semantics.

Only the whole-disk `part0` is created today. `BlockDevice` already
expresses view bounds with `start_block + capacity`, so a later partition
scan can publish more views without introducing another device object.

## Publication And Lookup

Driver probe constructs a `Gendisk` and calls `add_disk` through the block
class lifecycle. Publication validates that major is non-zero and that the
full minor range of a major does not overlap an existing one, then creates
`part0` and inserts the device into the single registry keyed by
`DeviceNumber`. Devfs, KVFS block-special open, filesystem mount, and boot
root selection all read that registry; none of them keeps its own mapping.

The registry itself is a single
`Mutex<BTreeMap<DeviceNumber, Arc<BlockDevice>>>` (`BLOCK_DEVICES`,
`src/lib.rs`): `add_disk`, `del_gendisk`, `lookup_block_device`, and
`block_devices` all take that lock for their insert, remove, and lookup
steps. The lock protects only the map — device objects' own state
(read-only state, claim token, capacity) is guarded by their own
primitives described below, never by the registry lock.

`del_gendisk` resolves the owning disk by `part0` `dev_t` and removes all
device views pointing at it. Existing `Arc<BlockDevice>` holders keep the
object memory alive; new lookups no longer return the withdrawn object.
Re-publishing the same `dev_t` later produces a fresh canonical
`BlockDevice` object, and users distinguish media generations by object
identity.

`BlockDevice::claim_exclusive()` returns the RAII `BlockDeviceClaim`,
corresponding to the Linux block holder ownership. One canonical device
admits a single holder at a time; a filesystem superblock holds that token
directly and releases it on init failure or on final shutdown entering the
dead state. Different filesystem instances therefore cannot own the same
media concurrently, and the block core needs no knowledge of VFS or
filesystem types.

## I/O Boundary

`BlockCompletionOperations` expresses device completion reclamation: one
call must be finite, non-sleeping, and must not submit new requests. The
host guarantees serial execution of callbacks registered for the same
target; activation keeps a strong reference to the target until polling is
synchronously stopped. A target registers at most once. Queuing and
stopping ownership stays with the host; no kernel API is exposed to
drivers for it.

The `completion` module defines the OS-neutral `PrepareBlockWait`,
`BlockWaiter`, and `BlockSignals`. They do not alter
`BlockDeviceOperations` and do not directly introduce kernel scheduler
dependencies. The host injects an ordinary prepare function that returns a
task-bound waiter; the waiter's `signals()` exports the `Arc` of the same
notification state, and drivers await admission hints and final completion
through that contract. An admission hint does not grant submission rights;
queue/FIFO predicates and request results remain owned by the concrete
transaction implementation. Final-state notification must be published
only after the device has released the data buffers.

A waiter is not shared across tasks; signals may be called from IRQ context
and retain no request/data/device pointers. Waiting-resource preparation
and admission registration may return errors, but the final-state wait
after submission no longer registers or returns wait errors. The VirtIO
implementation lives in `virtio::blk`; waiting plus IRQ/softirq wiring is
provided by `kdriver`. Other drivers adopt these contracts without
filesystem changes.

`BlockDevice` validates before delegating to the backend that the buffer
is a multiple of the block size, that the full I/O extent stays within
capacity, and performs checked arithmetic on block offsets.
`Gendisk::new` requires a non-zero block size and a representable
byte-size product of the initial capacity; `set_capacity` repeats those
bounds checks on every dynamic update.

The backend reports the immutable, device-lifetime inherent read-only
capability through `BlockDeviceOperations::is_inherently_read_only()`,
and `Gendisk::new` records it before publication. The administratively
set read-only state controlled by `BLKROSET` is kept separately in
`Gendisk` atomic state; the effective read-only state is "inherent
read-only OR administrative read-only". `BLKROSET 0` can therefore only
clear the administrative state and can never turn inherently read-only
media writable. `BlockDevice::write_block` uniformly rejects writes to
effectively read-only devices before entering the backend.

KVFS owns the byte-level adaptation corresponding to Linux
`blkdev_read_iter` / `blkdev_write_iter`: fully aligned blocks are passed
straight through from the caller's buffer, and leading/trailing partial
blocks reuse a single read-modify-write scratch buffer. An ordinary write
is not a durability barrier; only an explicit `fsync` calls the backend
`flush`.

## Non-Responsibilities

- No device I/O execution: reading, writing, and flushing media belong
  to the concrete backend behind `BlockDeviceOperations` (for example
  `virtio::blk`); the core only validates and routes.
- No IRQ wiring or completion threading: waiting, admission, and
  IRQ/softirq delivery are owned by `kdriver`'s block host providers;
  this crate defines the contracts only.
- No filesystem or page-cache policy: byte-level adaptation, durability
  semantics, and mount ownership live in KVFS and the filesystem layer.
- No partition scanning: only whole-disk `part0` is published today.
- No media encryption or access control beyond the read-only and
  exclusive-claim rules documented here.

## IRQ Completion Requirements

These frozen requirements describe caller-visible behavior, architecture
constraints (08-09), and delivery obligations (11), not a particular completion
mechanism. Sources are the user's blk_irq scope decisions, the block API at
baseline 80574592, and the repository driver-boundary rules. PR 727 supplied
problem context only; its implementation is not a design or code input.

Scope: ordinary reads, writes and flushes through filesystem/block interfaces;
virtio-blk is the first converted driver. The synchronous API remains unchanged.
Automatic polling fallback, conversion of every driver, forced DMA cancellation,
and unrelated scheduler/preemption repairs are not included. IRQ setup failure
fails activation without publishing a disk (accepted integration policy).

An accepted request has passed block validation and been accepted for device
processing. Continuous polling means repeatedly checking device completion for
the duration of outstanding I/O instead of waiting for a completion event.

### REQ-BLKIRQ-01: Preserve caller-visible block operation semantics.

- Source: current block API and user clarification.
- Behavior: ordinary callers continue to observe synchronous read, write, and
  flush operations. Success means the requested data transfer or flush effect
  required by the existing block contract has completed before the call returns.
- Acceptance: existing filesystem and block-device call sites compile without
  API changes; existing block read/write/flush behavior tests remain valid.

### REQ-BLKIRQ-02: Keep ordinary filesystem block I/O on the general block path.

- Source: user clarification and current X-Kernel call paths.
- Behavior: ordinary filesystem block reads, writes, and flushes reach concrete
  block drivers only through the normal block interfaces used by the rest of
  the system. Filesystems do not need to know that virtio-blk is the backend in
  order to benefit from interrupt-driven completion.
- Acceptance: code review shows filesystem and block-device-file paths do not
  call virtio-blk-specific APIs for ordinary block I/O.

### REQ-BLKIRQ-03: Complete virtio-blk requests without continuous polling.

- Source: user clarification and current virtio-blk polling behavior.
- Behavior: for an accepted virtio-blk read, write, or flush on an IRQ-capable
  device, the waiting caller does not continuously poll device completion state
  until the request finishes.
- Acceptance: test, trace, or instrumentation demonstrates an accepted
  virtio-blk request whose completion is observed after device interrupt
  notification and whose waiting caller is not on the continuous polling path.

### REQ-BLKIRQ-04: Return exactly one result for each accepted request.

- Source: synchronous block API expectation and PR review discussion about
  completion races.
- Behavior: each accepted read, write, or flush returns one completion result to
  its original caller. The result is not duplicated or lost when multiple
  requests or multiple block devices complete close together.
- Acceptance: tests or review evidence cover multiple completions and show every
  accepted request returns exactly once.

### REQ-BLKIRQ-05: Do not lose completion across wait races.

- Source: PR review discussion and requirement for accepted requests to finish
  visibly.
- Behavior: if a request completes before, during, or after the caller begins
  waiting for it, the caller can still observe the completion result.
- Acceptance: tests or reviewed interleavings cover completion-before-wait,
  wait-before-completion, and concurrent completion/wait setup.

### REQ-BLKIRQ-06: Preserve validation and read-only outcomes.

- Source: current block contract and user requirement that FS I/O remains on the
  general block path.
- Behavior: invalid block size or extent requests are rejected according to
  existing block behavior. Writes rejected because the effective block device is
  read-only are observed as `DriverError::ReadOnly`.
- Acceptance: existing block validation and read-only tests remain valid; tests
  or review evidence show the IRQ-capable virtio-blk path cannot change these
  caller-visible outcomes.

### REQ-BLKIRQ-07: Preserve caller-visible error meaning.

- Source: current `DriverError` contract and PR review discussion.
- Behavior: ordinary device I/O failures are reported as I/O failures.
  Temporary inability to accept more device work is reported as a retryable or
  backpressure condition, not as internal state corruption.
- Acceptance: tests or review evidence cover ordinary device failure and
  queue/backpressure failure mapping.

### REQ-BLKIRQ-08: Keep the new completion behavior reusable for future block drivers.

- Source: user clarification and Linux reference.
- Behavior: adding interrupt-driven completion for virtio-blk does not require
  filesystem-facing changes that would have to be repeated for another
  IRQ-capable block driver.
- Acceptance: design review can trace how another concrete block driver would
  expose the same caller-visible completion behavior without filesystem changes.

### REQ-BLKIRQ-09: Preserve reusable-driver portability boundaries.

- Source: project driver boundary rule.
- Behavior: reusable concrete driver crates remain independent from X-Kernel
  host-kernel implementation crates for host-specific capabilities.
- Acceptance: dependency metadata and imports show no new direct dependency from
  `drivers/devices/virtio` to host crates such as `kirq`, `ktask`, `kwork`,
  `khal`, `memspace`, `kdma`, or `kruntime`.

### REQ-BLKIRQ-10: Demonstrate reduced CPU waste under delayed I/O.

- Source: user focus on replacing polling with IRQ handling and PR performance
  motivation.
- Behavior: under an I/O-latency scenario, a task waiting for virtio-blk I/O
  should not consume CPU as if it were continuously polling for the full wait.
- Acceptance: runtime evidence, benchmark evidence, or instrumentation compares
  the new path with current polling under a delayed-I/O workload. If the
  environment cannot provide stable evidence, the missing validation and reason
  are recorded.

### REQ-BLKIRQ-11: Keep records, docs, and tests aligned with accepted behavior.

- Source: project change-completeness rules and loaded collaboration skill.
- Behavior: task records, affected design/security docs, rustdoc for touched
  public APIs, and regression tests reflect the final accepted behavior.
- Acceptance: documents are updated where behavior or contracts changed; focused
  tests are added or updated; build/lint/test commands are run according to the
  project workflow or recorded as not run with reasons.

## Completion Integration

The synchronous caller owns its buffers until return. Concrete drivers own
hardware submission, request identity and buffer retirement; host providers own
waiting and deferred execution. `block` defines the shared contracts without
owning hardware tokens or scheduler objects. Filesystem callers use
`BlockDevice`/`Gendisk`, never a VirtIO-specific wait or interrupt API.

- [VirtIO transactions and resource lifetime](../../../devices/virtio/docs/design.md)
- [Host waiting, IRQ dispatch and removal](../../../integration/kdriver/docs/design.md)
