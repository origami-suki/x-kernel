# inputdev — Security And Reliability

## Scope

This analysis covers the entire crate — the single `src/lib.rs` (the
global input registry, `init_input`, register/remove wiring, and
`input_drain_devices`). No modules are excluded; the crate holds no
unsafe code and no device semantics of its own.

Registry-only crate: it stores handles produced by `kclass` and hands
them to the input consumer. It processes no untrusted input, holds no
device state of its own, and adds no boundary — the security posture is
"keep the registry consistent and never fabricate handles".

## External Boundaries

None of its own. The `ClassDevice<InputDeviceImpl>` handles it stores are
created by trusted kernel driver code; the callbacks it subscribes
(`subscribe_input_available`, `subscribe_device_removed`) are invoked by
the trusted driver core.

## Unsafe Code

None. The crate contains no `unsafe`, no FFI, and no inline assembly.

## Invariants

- The registry holds at most one handle per device id (deduplication
  under the registry lock).
- A handle in the registry is either available or removed exactly once;
  `swap_remove` preserves the map semantics (no duplicate removals).
- `register_input_device` before `init_input` is a no-op (`is_inited`
  guard), so early callbacks cannot create a half-initialized registry.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Duplicate registration flooding the registry | Low — unbounded growth | Bus re-probe storms | Id deduplication under the lock: a device registers once. |
| T-02 | Stale handle used after removal | Low — I/O to a removed device | Consumer drained a handle that was then removed | Accepted by design: `Arc` handles keep the object alive; removal only detaches it from the registry. The device core owns post-removal I/O error semantics. |
| T-03 | Registry lock held across a blocking callback | Medium — system stall | Future callback doing slow work | Current callbacks are push/remove only; the notify callback design keeps work out of the lock (documented in the design doc). |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | Removal of an absent id | No-op (logged) | None | 4 | Position lookup fails silently. |
| F-02 | Drain before any device registered | Empty `Vec` returned | Consumer sees no devices | 3 | Normal empty-state behavior. |

## Known Limitations

- No wait/queue mechanism: consumers cannot block awaiting new devices;
  they rely on the subscription callbacks.
- Order after `swap_remove` is not stable; consumers must not assume
  registration order.

## Audit Checklist

- Registry mutations stay under the single lock and keep the
  dedup-by-id invariant.
- New subscription points keep the "no work under the lock" pattern.
