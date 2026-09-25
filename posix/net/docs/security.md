# POSIX network trust boundaries

User msghdr/cmsghdr pointers, lengths, iovecs, flags and descriptor numbers are
untrusted. Checked pointer/length arithmetic and UserPtr accessors enforce the
memory-copy boundary. Headers must fit the supplied control buffer; negative or
missing sender descriptors return EBADF. Rights count is capped at 253.

## Ownership and failure

Queued ancillary data owns Arc<VfsFile> references. Receiver fd allocation is
performed only for entries that fit; remaining references are dropped on control
truncation, absent buffers or fd-table exhaustion. MSG_CMSG_CLOEXEC applies when
the receiver descriptor is installed. A control copy failure closes descriptors
installed by that CMsgBuilder operation before returning the error.

No new unsafe sites are introduced. Existing unaligned sockaddr serialization
and ABI byte conversions remain confined to cmsg.rs. The receive operation is
not a transaction spanning every user write: faults in the final msghdr/address
copy after a successful control copy retain the existing syscall behavior.

## Limits and audit

Unix non-consuming MSG_PEEK and cyclic Unix socket-reference collection are
transport limitations; this layer does not fix them. Verify successful receipt,
short buffers, no buffer, CLOEXEC, bad sender fd, segmented control delivery,
receiver close and process exit against Linux. Raw runner success is insufficient
unless guest assertions and their exit codes also pass.
