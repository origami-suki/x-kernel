# inputdev — Design

## Purpose

`inputdev` is the runtime hub for input devices: it collects
class-registered input devices (`kclass` `ClassDevice<InputDeviceImpl>`
handles) into one global registry, keeps them in sync with driver-layer
device-removal events, and hands the set to the input consumer through a
single drain call.

## Responsibilities

- Own the process-wide input device registry
  (`LazyInit<Mutex<Vec<ClassDevice<InputDeviceImpl>>>>`), initialized by
  `init_input`.
- Seed the registry from `kclass` (`class_input_devices`), then keep
  accepting late arrivals by subscribing to
  `subscribe_input_available`; `register_input_device` deduplicates by
  device id and logs each activation.
- Track hot-unplug: subscribes to `kdevice::subscribe_device_removed` and
  `swap_remove`s matching handles.
- Serve the consumer: `input_drain_devices` moves every registered handle
  out of the registry into the caller's `Vec`.

## Non-Responsibilities

- No device detection or driver binding: devices arrive as already-built
  class handles from `kclass`/`kdevice`; this crate only warehouses them.
- No event reading or evdev semantics: consumers use the drained
  `InputDeviceImpl` handles directly; this crate never calls
  `read_event`.
- No wait/queue management: there is no blocking wait for new devices;
  arrival is push-based via the subscription callbacks.
- No user-space node management.

## Scope

```text
io/inputdev/
├── src/
│   └── lib.rs        # global registry, init, register/unsubscribe, drain
└── Cargo.toml
```

## Architecture

```text
kclass (class registration) --class_input_devices()--> seed list
        |                                                v
subscribe_input_available callback ---------> INPUT_DEVICES registry
kdevice::subscribe_device_removed callback -> swap_remove by id
consumer -----------------------------------> input_drain_devices()
```

The crate is a registry plus three wiring calls; all device semantics
stay inside the handles it stores.

## Execution Context

- `init_input` runs once during device subsystem bring-up, after `kclass`
  has devices and before the input consumer drains.
- Registration callbacks (`subscribe_input_available`,
  `subscribe_device_removed`) run in whatever context the driver layer
  invokes them (device probe / removal); the registry lock is held only
  for a short push or remove, with no allocation on the removal path.
- `register_input_device` is a no-op before initialization
  (`is_inited` guard), so late callbacks are safe at any point.
- Draining is intended for the single input consumer; after a drain the
  registry is empty and refills only through new arrivals.

## Concurrency Model

- One `ksync::Mutex` guards the handle vector; callbacks and the drain
  all take it briefly. `LazyInit` makes pre-init callbacks no-ops
  instead of panics.
- Deduplication (by `id()`) happens under the same lock, so a device
  reported twice (bus re-probe) registers once.
- `swap_remove` is used deliberately: handle order is not significant to
  consumers, and removal stays O(1) without shifting.

## Error Model

Infallible by design: registration of an unknown or duplicate device is
silently ignored (logged), removal of an absent id does nothing, and
there are no fallible operations. Device-level errors remain inside the
`InputDeviceImpl` handles and surface when consumers use them.

## Design Decisions

- Registry-of-handles instead of direct device dispatch: the input
  consumer owns policy (which device to read, how to wait); the hub only
  guarantees a consistent snapshot of what exists.
- Push-based subscriptions rather than polling: new devices become
  visible without consumer-side rescan loops, and hot-unplug is handled
  at the source of truth (`kdevice` removal events).
- Drain semantics for handoff: the input layer wants each device exactly
  once at startup; draining avoids keeping a second list in sync and
  leaves late-arrival delivery intact through the subscription.
- `swap_remove` for unregistration: order stability is worthless here,
  and O(1) removal keeps the device-removal callback cheap.
