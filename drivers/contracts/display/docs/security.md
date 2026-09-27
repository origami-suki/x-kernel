# display — Security and reliability

## Scope and trust boundaries

The crate defines display metadata, owned 2D backing, typed legacy virgl
commands, and host execution/deferred-work contracts. It does not access user
pointers, queues or hardware. Kernel callers translate validated userspace
requests into these types; concrete drivers validate the remaining protocol
bounds and device replies.

## Unsafe boundary

`GpuBacking::new` in `src/virgl.rs` is unsafe. Its owner must retain the entire
stable, coherent physical allocation until the driver releases it; CPU access
must obey GPU reservation/transfer synchronization. Private fields prevent
constructing an unowned backing through safe struct syntax. The owner must not
retain the display device, to avoid registry/device reference cycles.

## Lifecycle invariants

- The driver pins owned backing before publishing host attachment.
- Timeout or an ioctl error is not DMA cancellation. A driver may retain the
  owner after returning an error; only fenced teardown or acknowledged reset
  permits releasing its final DMA reference.
- Mapping/framebuffer/in-flight ownership and resource reservations remain the
  consumer's responsibility. A userspace handle is never a device resource ID.
- Destructor cleanup enqueue must not wait or execute callbacks inline. A
  failed scheduling attempt must preserve the marked object and allocation.
  `flush_virgl_cleanup` permits a task-context retry.
- `GpuExecution::run` validates sleepability and serializes the whole callback
  with interrupts enabled. No callback recursively takes its own gate.
- Cached capset snapshots are immutable. Hardware virgl support is distinct
  from `has_virgl_commands`, and neither establishes userspace acceleration.

## Threats and failure handling

| Threat | Consequence | Required control |
| --- | --- | --- |
| Unowned/reused physical backing | Host DMA into unrelated memory | Unsafe owned-backing constructor; pin through fenced teardown/reset |
| Long GPU wait under IRQ lock | Timer/scheduler starvation | Host sleepable gate; short queue critical sections; wait outside locks |
| Failed teardown treated as release | DMA use after free | Retain and expose pending objects; explicit retry/status |
| Cyclic device/allocation owner | Permanent normal-path retention | Allocation-only Arc owner and weak deferred callback |
| Opaque/unbounded user command | Excess allocation or invalid host protocol | Copy at ABI boundary; driver length/ID/bounds checks |

No blob, explicit sync-fd, context-init or syncobj support is implied. The raw
physical-address-only scanout method remains a compatibility trait default;
drivers requiring safe timeout ownership return Unsupported for it.

Scanout-disable success means device completion, not just a guest state update.
An error must retain the old active state and resource ownership; callers may
release their active framebuffer reference only after successful disable.
