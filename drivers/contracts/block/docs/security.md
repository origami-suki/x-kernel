# block — Security And Reliability

## Scope

This analysis covers the block-core contract in `src/lib.rs` and
`src/completion.rs`. The concrete backends (`src/ahci.rs`,
`src/bcm2835sdhci.rs`, `src/sdmmc.rs`, `src/ramdisk*.rs`) are hardware
adapters excluded from this document and audited separately; the
registry, validation, and completion invariants below are the core's
own.


## Trust Boundaries

Driver-reported disk identity, capacity, block size, and I/O completion
cross the block-core boundary. External media contents are untrusted;
format validation belongs to the filesystem consuming the device.

## Invariants

- Major must be non-zero; the minor range must be non-empty and must not
  overflow or overlap an already-published disk;
- Block size must be non-zero, and `num_blocks * block_size` must be
  representable in `u64`;
- One `dev_t` maps to exactly one `BlockDevice` in the resident registry;
- A canonical `BlockDevice` has at most one exclusive holder at a time;
  the claim token owns the release;
- Every I/O buffer length is a multiple of the block size and the full
  extent lies within the current capacity;
- Block-offset arithmetic is checked;
- The operations object, `Gendisk`, and `BlockDevice` are all
  `Send + Sync`;
- No driver I/O or callback runs while holding the registry lock;
- The backend's inherent read-only capability is recorded by `Gendisk`
  before publication and cannot be cleared through the administrative
  interface;
- The administrative read-only state is owned only by the owning
  `Gendisk`; the effective read-only state is the union of both;
- Every `BlockDevice` write checks the effective read-only state before
  entering the backend.

## Unsafe

`drivers/contracts/block/src/lib.rs` contains no `unsafe` blocks. The
MMIO/DMA safety boundaries of concrete hardware backends are owned by
each backend's own documentation.

## Failure Handling

`BlockCompletionOperations::process_completed_requests` is a trusted
driver callback and must be finite and non-sleeping. The host's
serial-execution guarantee is bounded by a single registration; no
registration may be created while assuming an earlier one still provides
mutual exclusion. Completion callbacks must not wake callers while
holding a device lock; the target strong reference must span the
synchronous stop before device resources are destroyed.

The completion-wait contract requires provider signals to own only the
notification state — never requests or DMA data. Callers notify outside
device/registry locks; a `BlockWaiter` is used only by the task that
prepared it, in sleepable context. A failed admission wait cancels the
pending node at the transaction layer and passes the admission slot on;
the terminal wait has no recoverable-error exit, so a request still in
DMA cannot be freed early through a wait error. The contract itself
implements neither queues nor DMA lifecycle and does not verify that the
device has finished; drivers must publish the real result before raising
the terminal notification.

| Failure | Result |
|---|---|
| Identity/range conflict | `AlreadyExists`; no partial disk is published |
| Existing exclusive holder | `ResourceBusy`; no second holder is created |
| Invalid or out-of-range I/O | `InvalidInput`; backend not called |
| Backend I/O/flush failure | `DriverError` propagated unchanged |
| Write to a read-only disk | `ReadOnly`; KVFS maps it to Linux `EPERM` |
| `BLKROSET 0` on an inherently read-only disk | `ReadOnlyFilesystem` (Linux `EROFS`); state stays read-only |
| Unknown disk-specific ioctl | `NotATty` |
| New open/mount after disk withdrawal | Canonical lookup fails |

## Known Limitations

There is no partition scan today; only whole-disk part0 is published.
Hot removal does not actively freeze already-mounted filesystems; further
I/O through existing references relies on the backend returning device
errors.
