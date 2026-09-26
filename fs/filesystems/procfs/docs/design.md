# Procfs design

Procfs registers a nodev `proc` filesystem backed by `kvfs::SimpleFs`.
`root::builder` composes static node groups with dynamic process directories.
Per-process visibility and lookup belong to kprocess; filesystem nodes adapt
those owner queries into VFS operations. Trace, optional lock/scheduler
statistics, SysRq and memory/device nodes keep their respective owners.

## Syscall observation node

`syscall_profile::add_root_entry` installs `/proc/syscall_profile` as a regular
0600 `SimpleFile` backed by `CommandFile`. Each write is one complete control
request; reads delegate to the stopped snapshot API. Procfs retains no counter,
epoch or task state. Kprocess checks current effective UID at every operation,
serializes control and owns collection limits; its design documents timing,
loss and boundary semantics. Reading an active collector returns EBUSY.

The operation runs in the calling user thread's VFS context and may allocate
or sleep during control/export. It is never used from IRQ context. The node
is observational and does not affect directory process membership or syscall
ABI behavior. No raw user pointer crosses this adapter boundary.
