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

Cyclic Unix socket-reference collection remains a transport limitation; this
layer does not fix it. Unix MSG_PEEK shares immutable queued ownership and
installs separate receiver descriptors on each peek. Verify successful receipt,
short buffers, no buffer, CLOEXEC, bad sender fd, segmented control delivery,
receiver close and process exit against Linux. Raw runner success is insufficient
unless guest assertions and their exit codes also pass.

## Credential authorization and limits

Explicit SCM_CREDENTIALS requires exactly three 32-bit fields. PID must equal
the calling process TGID, UID/GID must match one of its real/effective/saved IDs,
and the invalid all-ones UID/GID returns EINVAL. Unauthorized identities return
EPERM before payload publication. This implements Linux's unprivileged contract;
capability-based arbitrary PID/UID/GID impersonation is not supported, including
for root. There is no added capability or user/PID namespace implementation.
Automatic credentials always use real IDs. Numeric sender identities are captured
at send time and remain valid as recorded values after the sender exits.

SO_PEERCRED's pre-existing creation-time placeholder, Unix autobind semantics,
and the existing mapping of SEQPACKET to the datagram transport are unchanged;
PASSCRED success does not certify those separate contracts. Missing credentials
on bytes queued before either side enabled PASSCRED are represented by Linux's
PID 0 / overflow UID and GID 65534 when subsequently received with PASSCRED.
