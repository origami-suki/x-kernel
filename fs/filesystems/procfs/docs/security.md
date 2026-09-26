# Procfs security notes

Procfs exposes kernel/process observations through VFS paths. Its dynamic task
views are not lifetime pins or authorization checks; owner APIs and ordinary
VFS permissions remain responsible for access. Optional diagnostics have their
own owner-side constraints. These notes describe the new observer boundary,
not a completed audit of every existing proc node.

## Syscall observer boundary

The inode mode is 0600 and the kprocess owner verifies effective UID 0 on both
read and write, including use of a descriptor inherited from root. The adapter
passes copied command bytes to the owner, which bounds their size and accepts
only `start`/`stop` or empty truncation writes. Active reads and duplicate starts
fail, and malformed input cannot silently enable collection.

Snapshots contain process IDs and aggregated kernel timings, not syscall
arguments, paths or user buffers. Memory is bounded by the fixed owner tables;
export allocations and formatting occur after collection has stopped. The
adapter adds no unsafe code, mutable globals, locks or task references. Review
permission handling together with kprocess when changing this node; do not
replace the owner check with inode mode alone.
