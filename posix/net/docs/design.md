# POSIX network syscall boundary

`posix-net` translates user socket addresses, iovecs, flags and ancillary data
into `knet::SocketOps` calls. The transport owns protocol and queue state;
`ProcessResources` owns descriptor installation and close-on-exec state.

## SCM_RIGHTS

`parse_send_cmsgs` validates header lengths, advances by native-word CMSG
alignment, resolves descriptor numbers through the sending process table and
combines rights lists into one owned `CMsg::Rights`. At most 253 references are
accepted in a syscall. The transport receives references to the same open file
descriptions, not reopened paths or integers from the sender table.

On receive, `push_socket_cmsg` installs only the descriptors fitting the user
control buffer, sets CLOEXEC when MSG_CMSG_CLOEXEC requests it, and reports
MSG_CTRUNC for discarded references or descriptor-table exhaustion. With no
control buffer, incoming references are discarded and recvmsg reports truncation.
CMsgBuilder records unpadded cmsg_len and aligned msg_controllen (limited by the
provided capacity). A failed control payload/header copy rolls back descriptors
installed for that control message.

The SCM_RIGHTS contract follows Linux unix(7) and recvmsg(2):
https://man7.org/linux/man-pages/man7/unix.7.html
https://man7.org/linux/man-pages/man2/recvmsg.2.html

## Execution context

Syscalls require the current process resources and faultable user-memory access.
No transport lock is retained while adding or removing receiver descriptors.
The syscall boundary does not implement stream byte ordering, EOF or wakeups.

## Unix sender credentials

`send_impl` captures the current process TGID and real UID/GID for Unix sends.
The VFS socket write adapter supplies the same snapshot for write/writev; neither
path uses the socket creator's PID or the file's open credentials. Parsed explicit
SCM_CREDENTIALS overrides that automatic snapshot only after authorization.
`SendOptions::credentials` transports this value without a process lookup in the
stream/datagram queue implementations. Sender metadata is retained if either
endpoint enables PASSCRED, or the sender explicitly supplies credentials.

Receiving with PASSCRED emits credentials before rights. A short credential body
is copied as a prefix and marked MSG_CTRUNC, including the header-only case.
`AncillaryData` has immutable shared ownership so MSG_PEEK can install fresh
receiver descriptors without consuming the queued references. Later reads still
receive the original message; stream short reads retain its sender identity while
rights detach only on the first consuming read.
