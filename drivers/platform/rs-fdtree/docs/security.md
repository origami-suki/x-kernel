# rs_fdtree — Security And Reliability

## Scope

This analysis covers the entire crate: `src/lib.rs` (`LinuxFdt`,
`MemReserveIter`), `src/header.rs`, `src/parsing.rs`, `src/node.rs`,
`src/error.rs`, and `src/kernel_nodes/`. No modules are excluded;
everything is reachable from `LinuxFdt::new` / `LinuxFdt::from_ptr` and
the iterators they produce.


## Trust Model

The DTB blob is external, untrusted input until validated. This crate is
the structural validation gate for the whole device-tree stack: once
`LinuxFdt::new` accepts a blob (magic, declared size versus buffer), all
later reads are bounds-checked against that clamped view. Callers
(`of`, boot code) are trusted to keep the blob alive as long as borrowed
views exist.

## External Boundaries

- `LinuxFdt::new(&[u8])` — safe entry: validates header magic
  (`0xd00dfeed`) and clamps the working slice to the declared
  `totalsize`.
- `LinuxFdt::from_ptr(*const u8)` — unsafe boot entry: reads a header-
  sized view, then the header-declared total size. The precondition (see
  Unsafe Code) covers pointer validity and lifetime.
- All property/node bytes handed upward are slices into the validated
  blob; semantic interpretation happens in `of`, which re-checks lengths.

## Unsafe Code

One unsafe function: `LinuxFdt::from_ptr`, containing two
`slice::from_raw_parts` calls with `# Safety` documentation:

1. header-sized view (`size_of::<FdtHeader>()` bytes) — used to read
   `totalsize`;
2. full view (`totalsize` bytes) — handed to the safe `new` for magic and
   size validation.

Preconditions: `ptr` is non-null (checked, `FdtError::BadPtr`), points to
a readable FDT, and the header plus total-size range remain accessible
for the returned lifetime. A blob shorter than its own declared
`totalsize` is rejected (`FdtError::BufferTooSmall`) — but note the
memory-safety burden for those bytes stays with the caller, since the
slice is formed before validation.

## Invariants

- No read ever leaves the clamped blob slice: header offsets select
  sub-slices of `&data[..totalsize]`, and the parsing cursor returns
  `None` instead of indexing past a block.
- Iterators terminate: node walking stops at the structure-block end
  token; `MemReserveIter` stops at the `0,0` terminator pair; every
  scalar read checks remaining length first.
- The crate keeps no global state; safety never depends on call ordering.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | `from_ptr` on a short or unmapped buffer | High — out-of-bounds read while reading the header/size | Boot pointer to truncated or freed memory | Owned by the caller via the `# Safety` contract; the crate cannot probe length safely. Residual risk accepted at the boot boundary. |
| T-02 | Header offsets pointing inside the blob but at garbage | Low — nonsense nodes/properties, never unsafety | Corrupted structure block | All reads clamp to the validated slice; garbage decodes to odd values that `of` rejects semantically. |
| T-03 | Declared `totalsize` exceeding the real blob | High — OOB slice formation | Forged header | `new` checks `data.len() >= totalsize` and rejects (`BufferTooSmall`); for `from_ptr` the caller owns the real length. |
| T-04 | Infinite loop on a malformed node stream | Medium — boot hang | Structure block without an end token | Node walking is driven by the block range derived from the validated header; the end-token check terminates iteration. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | Bad magic | `FdtError::BadMagic` | Caller aborts DTB path | 3 | Explicit error at construction. |
| F-02 | Buffer smaller than declared size | `FdtError::BufferTooSmall` | Caller aborts or re-slices | 3 | Size check before slicing. |
| F-03 | Truncated property mid-read | `None` from the accessor | Property treated as absent | 3 | Cursor returns `Option`; no panic, no OOB. |

## Known Limitations

- The crate validates structure, not semantics: it cannot detect a
  well-formed tree describing wrong hardware.
- `from_ptr` necessarily trusts the header before it can validate it; the
  initial header-sized read is the irreducible trusted step.

## Audit Checklist

- Any new block offset math stays within `&data[..totalsize]`.
- New iterators keep the "check length, then read" discipline and
  terminate on malformed streams.
- `from_ptr`'s `# Safety` text still matches the two slice formations.
