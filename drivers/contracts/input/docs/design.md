# input — Design

## Purpose

`input` defines the contract between input device drivers (keyboards,
mice, tablets) and the kernel input layer: Linux-evdev-compatible event
types and payloads (`EventType`, `Event`, `InputDeviceId`, `AbsInfo`) and
the `InputDevice` trait that drivers implement on top of the shared
`driver_base::Device` base.

## Responsibilities

- Define `InputDevice: Device`: identity reporting (`device_id`,
  `physical_location`, `unique_id`), capability bitmap queries
  (`get_event_bits` per `EventType`), and non-blocking event fetch
  (`read_event`).
- Define the evdev-compatible wire types: `Event` (`event_type`, `code`,
  `value` — `repr(C)`, matching the Linux `input_event` layout),
  `InputDeviceId` (`bus_type`/`vendor`/`product`/`version`), and
  `AbsInfo` (absolute-axis `min`/`max`/`fuzz`/`flat`/`res`).
- Define `EventType`, the Linux input subsystem event categories, with the
  per-category bitmap lengths (`bits_count`) that size capability queries.

## Non-Responsibilities

- No event queueing or synthesis: the input layer owns buffering,
  repetition, and filtering; drivers hand over one event at a time.
- No user-space interface: `/dev/input` node exposure and the evdev
  `ioctl` surface belong to the device/fs layers; this crate only fixes
  the data layout.
- No device discovery or binding: the `driver_base` pipeline owns probe
  and activation.
- No bitmap storage: `get_event_bits` fills a caller-provided buffer; the
  crate does not allocate or cache capability bitmaps.

## Scope

```text
drivers/contracts/input/
├── src/
│   └── lib.rs        # EventType, Event, InputDeviceId, AbsInfo, InputDevice
└── Cargo.toml
```

## Architecture

```text
driver_base (Device base, DriverError/Result, discovery pipeline)
      ^
      |  impl InputDevice for <driver>
      |
input (this crate: evdev-compatible types + trait)
      ^
      |  read_event() / get_event_bits()
      |
kernel input layer (queues events, exposes them upward)
```

`repr(C)` on `Event`, `InputDeviceId`, and `AbsInfo` keeps the structs
layout-compatible with Linux evdev definitions so the user-facing wire
format can be produced without repacking.

## Data Model

- `EventType` (`repr(u8)`, derived `FromRepr`): the Linux categories
  (`Synchronization = 0x00` … `ForceFeedback = 0x15`), `MAX = 0x1f`,
  `COUNT = MAX + 1` slots, and `bits_count` returning the bitmap length
  per category (e.g. `Key` = 0x300 bits, `Absolute` = 0x40 bits).
- `Event`: category (as `u16`), code, and value; `is_type` compares
  against an `EventType`.
- `InputDeviceId`: `UNKNOWN` constant (all zeros) for anonymous devices.

## Execution Context

- `read_event` is non-blocking: no events means
  `Err(DriverError::WouldBlock)`, so the input layer decides how to wait.
- Methods take `&self`; drivers with shared hardware must synchronize
  internally.
- No allocator, no sleeping, no context assumptions beyond the driver
  pipeline being active.

## Known Limitations

- `EventType` defines only a subset of the Linux input categories:
  within the `0x00..=0x1f` slot range, values `0x06`-`0x10`, `0x13`,
  `0x14`, and `0x16`-`0x1f` have no variant today. Adding a category
  requires extending both the `EventType` enum and the `bit_len_of`
  bitmap-length table together; `bits_count` of an existing category
  never covers the new one.
- Event codes beyond a category's `bits_count` cannot be reported
  through `get_event_bits` bitmaps; consumers must size their buffers
  from `bits_count(ty)`.

## Error Model

All fallible operations return `driver_base::DriverResult`:

- `get_event_bits` — `Ok(true)` when the event type is supported and the
  bitmap was written into `out`; `Ok(false)` when the type is not
  supported; a `DriverError` when the hardware query itself fails.
- `read_event` — `Ok(Event)` or `Err(DriverError::WouldBlock)` when no
  event is pending.

## Design Decisions

- Mirror the Linux evdev definitions (event types, codes, ID tuple, abs
  bits, `repr(C)` layouts): the kernel can expose a compatible interface
  to user space without translation, and Linux driver knowledge transfers
  directly.
- Caller-provided capability buffers (`get_event_bits(ty, out)`) instead
  of owned bitmaps: bitmap sizes vary per event type and the input layer
  already knows its own allocation policy.
- `WouldBlock` instead of blocking reads: consistent with the other
  driver contract crates (`char`, `net`) and keeps scheduler policy out
  of drivers.
- Re-export `driver_base` types: one vocabulary and one import root for
  implementers, matching the sibling contract crates.
