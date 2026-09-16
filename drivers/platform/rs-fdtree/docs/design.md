# rs_fdtree — Design

## Purpose

`rs_fdtree` is a minimal, dependency-free, `no_std` parser for the Linux
flattened devicetree (FDT/DTB) format. It turns a DTB blob in memory into
borrowed, `Copy`-able views: the tree of nodes, their properties, memory
regions, and a few well-known kernel nodes (`/chosen`, Open DICE, the
interrupt controller). It is the structural layer beneath the `of` crate,
which adds kernel-specific semantics.

## Responsibilities

- Validate the FDT container: header magic (`0xd00dfeed`), declared total
  size versus the available buffer (`LinuxFdt::new`), and block offsets
  (structs, strings, memreserve) derived from the header.
- Iterate and find nodes: depth-first `all_nodes`, path-based `find_node`,
  alias-aware `resolve_node` (via `/aliases`, ignoring `:`-suffixed
  cell suffixes), `find_compatible`.
- Read properties: raw bytes (`property`), C-string and string-list views
  (`property_str`, `compatibles`), big-endian `u32` (`property_u32`),
  parent-node lookups (`parent_property`, `parent_property_u32`), and
  `cell_sizes` (`#address-cells` / `#size-cells`).
- Decode `reg` entries into `MemoryRegion`s through `RegIter`, honoring the
  node's cell sizes.
- Enumerate memory layout sources: `memory_regions` (`/memory`),
  `mem_reservations` (memreserve block, `0,0`-terminated),
  `reserved_memory_regions` / `reserved_memory_nodes` (`/reserved-memory`).
- Expose typed wrappers for well-known nodes: `Chosen` (`bootargs`,
  `stdout_path`), `Dice` (`regions` under `/reserved-memory/dice` or
  `google,open-dice` / `kylin,open-dice` compatibles), and
  `InterruptController` (first node carrying an `interrupt-controller`
  property).
- Report failures as `FdtError` (`BadMagic`, `BadPtr`, `BufferTooSmall`).

## Non-Responsibilities

- No kernel semantics: interrupt-cell decoding, PCI ranges, CPU status
  filtering, and phandle-to-node policy live in `of`, not here.
- No global state: the crate has no statics; a tree is whatever
  `LinuxFdt` borrows, for as long as the caller keeps the blob alive.
- No mutation, patching, or DTB generation: read-only views over
  firmware-provided bytes.
- No allocation: everything borrows from the input buffer; no `alloc`
  dependency at all.
- No alignment guarantees: unaligned big-endian field reads are supported
  on purpose (`FdtData`), because boot loaders do not always align the blob.

## Scope

```text
drivers/platform/rs-fdtree/
├── src/
│   ├── lib.rs            # LinuxFdt, MemReserveIter, re-exports
│   ├── header.rs         # FdtHeader layout, magic and block ranges
│   ├── parsing.rs        # FdtData cursor, big-endian ints, CStr
│   ├── node.rs           # FdtNode, NodeProperty, RegIter, region iterators
│   ├── error.rs          # FdtError, Result alias
│   └── kernel_nodes/     # Chosen, Dice, InterruptController wrappers
└── Cargo.toml
```

## Architecture

```text
&[u8] DTB blob (or *const u8 via unsafe from_ptr)
      |
  FdtHeader (big-endian fields; magic + totalsize validated)
      |
      +-- structs block --> all_nodes / find_node / reserved_memory_*
      +-- strings block --> property name resolution (string_at_offset)
      +-- memrsv block  --> MemReserveIter (address/size pairs)
      |
  FdtNode<'b, 'a>  -- properties, compatibles, reg --> MemoryRegion
```

All types carry the borrow of the blob: `LinuxFdt<'a>`,
`FdtNode<'b, 'a>` (node lifetime vs blob lifetime), and property values as
`&'a [u8]` / `&'a str`. Nodes and values remain valid without keeping the
`LinuxFdt` value itself.

## Parsing Model

- A single forward cursor (`FdtData`) reads big-endian scalars, C strings,
  and skips padding; every accessor returns `Option`/`Result` instead of
  indexing unchecked.
- The header is the only trusted length source: block ranges are computed
  from header offsets, and the blob slice is clamped to the declared
  `totalsize` at construction.
- Structure-block walking stops at the end token; unknown property or node
  content is passed through as raw bytes rather than rejected.

## Execution Context

- Callable from the earliest boot stage: no allocator, no globals, no
  hardware access, no environment assumptions beyond the blob being
  readable at the given address.
- `LinuxFdt::new` is the safe entry for an already-sliced blob;
  `from_ptr` is the unsafe boot-loader-pointer entry (null check, header
  read, then the header-declared total size).

## Concurrency Model

- No shared state; every value is `Copy` or a shared borrow, so concurrent
  readers of one DTB need no synchronization.

## Error Model

- Construction is fallible: `FdtError::BadMagic` for a wrong magic,
  `BufferTooSmall` when the buffer is shorter than the declared total size,
  `BadPtr` for a null pointer in `from_ptr`.
- Accessors return `Option`: absent nodes, properties, or undecodable
  entries yield `None`; iterators simply end. No panics on malformed tree
  contents.

## Firmware Trust Boundary

- What is validated: header magic, total size against the buffer, and
  bounds on every field read (a truncated stream yields `None`, not a
  fault).
- What is trusted: header offsets pointing at plausible blocks. A malicious
  header can name offsets inside the blob that decode to nonsense nodes;
  the result is absent or odd data, never memory-unsafe access, because all
  reads are clamped to the borrowed slice.

## Unsafe Code

One unsafe entry point: `LinuxFdt::from_ptr(ptr)`. It forms two
`slice::from_raw_parts` views — header-sized, then `totalsize`-sized after
re-reading the header from the first view. The `# Safety` contract: `ptr`
must point to a readable DTB whose header and total-size range remain
accessible for the returned lifetime. All other code is safe and bounds-
checked.

## Design Decisions

- Zero dependencies and no `alloc`: this parser runs before the kernel
  heap exists and is reused by any subsystem that needs raw DTB access.
- Borrowed, `Copy`-heavy views instead of an owned tree: boot-time code
  passes nodes and properties around freely without cloning, and the
  two-lifetime `FdtNode<'b, 'a>` lets iterators borrow the tree while
  values keep pointing into the blob.
- Unaligned reads over alignment preconditions: boot firmware does not
  guarantee 4-byte alignment for the DTB, so `FdtData` reads big-endian
  fields byte-wise.
- Well-known node wrappers (`Chosen`, `Dice`, `InterruptController`) live
  in `kernel_nodes/` to keep node policy discoverable without pushing
  kernel conventions into the generic parser.
- `#![allow(rustdoc::bare_urls)]` is set crate-wide for spec-reference URLs
  in documentation.
