# alloc-engine: buddy page allocation

## Purpose and boundaries

`BuddyAllocator` owns free-space bookkeeping for caller-supplied writable memory.
`BuddyPageAllocator` exposes it through `PageAllocator`; `SlabHeap` uses a separate
instance without reserved bitmap storage for backing its size classes. Other byte-allocation adapters retain
their existing algorithms. This document describes the buddy implementation.

The caller owns memory discovery, virtual mappings, backing-storage lifetime,
synchronization and permission to reuse a page. PCP policy belongs to `kalloc`.
TLB invalidation, page pinning and VMA/object lifetimes belong to higher MM layers.

## State and representation

Each `Region` owns per-order `intrusive_collections::LinkedList<FreeNodeAdapter>`
lists and an optional `FreeHeadBitmap`. List elements are `UnsafeRef<FreeNode>` handles that
own no storage; the free chunk itself holds the node. A set bit identifies an
initialized free-block **head**, whose header carries its order and intrusive
link. Allocated pages and interior pages of a free block have clear bits. Header contents alone never establish membership.
In particular, an arena allocated by an outer buddy may contain valid free-node
headers belonging to a nested `SlabHeap` buddy; its outer head bit stays clear.

Up to four machine words of bits are stored inline. Larger regions reserve a
page-rounded prefix for one bit per registered page, before publishing any free
blocks. There is no recursive heap allocation. With 4 KiB pages, a 1 GiB region
reserves 32 KiB. `total_pages`, free/used counts and byte statistics exclude this
prefix. Region-overlap checks include it; allocations cannot return it.

## Operations and complexity

- Allocation removes a free block, clears its head bit and returns unused halves
  through `push_free`. Each newly free half gets initialized links before its bit
  is published.
- Deallocation computes the buddy address at each order. `Region::is_free` checks
  range, alignment and the bitmap before reading the header's order.
  `remove_free` directly unlinks a matching buddy and clears its bit. In the physical allocator, at most
  `MAX_ORDER` buddy probes are needed, regardless of the free-list lengths.
- Fixed-address allocation checks aligned ancestors through the same membership
  check and uses `split_towards` to return each unselected half exactly once.
- Requests whose alignment exceeds their rounded allocation size retain the
  existing list search. Region selection is bounded by `MAX_REGIONS`.

## Concurrency and lifecycle

The implementation does not sleep, perform I/O or require a current task. It
can initialize during early boot without a working heap, provided the supplied
memory is already mapped. IRQ-context use depends on the caller's IRQ-safe
serialization; concurrent or reentrant access to one instance is not permitted.

All changes require exclusive allocator access. `kalloc` supplies its existing
allocator lock; this implementation introduces neither a new lock nor a new
cross-CPU ownership protocol. Intrusive nodes stay at their free chunk addresses
until removal, and no reference to a node escapes an allocator operation.

A returned allocation may overwrite the old header after its bit has been
cleared. `reset` and dropping the allocator discard bookkeeping; they do not
release backing storage. Retiring a `Region` clears only its list headers
(`Region::drop` calls `fast_clear` on each list); it never writes into caller
memory, so a region can be retired after its pages have been handed back. The owner must first retire every allocation and then
reclaim the original region, including its reserved bitmap prefix.

## Verification

Unit tests cover sequential, reverse and fixed-random frees, preservation of
live page contents, recovery of maximal coalesced blocks, inline and reserved
bitmaps, overlapping registration, non-power-of-two region edges, alignment,
fixed-address splitting and reset. Timing and scaling measurements belong in
external benchmarks, not timing assertions in unit tests.

## Keeping the byte heap capacity unchanged

The physical allocator keeps the indexed implementation. `SlabHeap` constructs
its nested buddy with `new_for_slab_heap`: it reserves no bitmap pages and finds
membership by walking the requested order's free list before unlinking. A live
payload is never accepted as a free node in either mode. The list mode preserves
the baseline region capacity, maximal block and allocation ordering; its existing
linear membership cost remains confined to the nested byte heap. No new lock,
heap allocation or runtime option is introduced.

This is deliberate scope control for the palloc contention fix. Upstream !821
reserves a prefix in both allocators. An aligned 2 MiB SlabHeap then reports
2 MiB minus one page and cannot serve two 1 MiB allocations, which the baseline
can. Moving the bitmap to the tail alone does not restore that capacity: for an
aligned power-of-two region the prefix/tail maximal-order histograms are mirrors
with the same counts. Local host and guest tests preserve full slab capacity,
large blocks and repeated growth-region reuse while retaining the physical index.

Upstream issue IKHSF8 reports an x86_64 fs_racer OOM and hypothesizes byte-heap
growth amplification. The capacity loss is reproduced here; the complete OOM
mechanism is not claimed proven or fixed without the exact workload. Other MM
lifetime/concurrency behavior and page-cache reclaim are outside this change.
