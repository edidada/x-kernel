# rs_fdtree — Design

## Strict decoding and legacy compatibility

reg()/RegIter remain untranslated, allocation-free compatibility APIs for
existing boot consumers. They may return a valid prefix before malformed
trailing input ends iteration. New numeric MMIO discovery uses mmio_resources;
strict decoding must not delegate to the permissive iterator. Migrate callers
individually before removing legacy access, rather than changing its behavior.

Resource names reuse read_strings for complete NUL/UTF-8 validation, then
reject empty elements as a binding constraint. Encoding failures identify the
property (index None); validly decoded empty names identify their element.
ranges errors identify only an actual window tuple. No covering window or
empty-ranges width failure reports index None, never the device's reg index.

## Strict property and structural access

Strict scalar/string/array reads validate encoding and distinguish duplicate,
malformed and missing properties, without allocation or binding policy.
Node offsets are source-local structure-block offsets, not firmware identities.
Parent and offset lookup scan the validated tree without allocation. Status
interpretation and reg/ranges resource translation belong to this DT backend.

## DT address interpretation

FdtNode::is_available accepts absent/ok/okay status, rejects malformed properties,
and does not decide ancestor availability or controller readiness.
FdtNode::mmio_resources collects full CPU-physical reg intervals and reg-names
through numeric ranges. Missing ranges rejects translation; empty ranges is
identity. PCI/ISA encodings remain unsupported. The private address module
owns interval validation, including inclusive maximum-address bounds; it does
not map MMIO. GIC register roles remain in the controller driver.
Host tests run both with and without alloc; address tests require alloc.

## Purpose

`rs_fdtree` is a minimal, dependency-free, `no_std` parser for the Linux
flattened devicetree (FDT/DTB) format. It turns a DTB blob in memory into
borrowed, `Copy`-able views: the tree of nodes, their properties, memory
regions, and a few well-known kernel nodes (`/chosen`, Open DICE, the
interrupt controller). It is the DT backend. The transitional `of` crate owns boot integration and
legacy queries, not new DT interpretation.

## Responsibilities

- Validate the FDT container: header magic (`0xd00dfeed`), declared total
  size versus the available buffer (`LinuxFdt::new`), nonoverlapping block
  ranges, reservation termination, and the entire structure token stream.
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
  `stdout_path`, `x_kernel_log_level`), `Dice` (`regions` under
  `/reserved-memory/dice` or `google,open-dice` / `kylin,open-dice`
  compatibles), and `InterruptController` (first node carrying an
  `interrupt-controller` property). `x_kernel_log_level` reads the vendor
  property `x-kernel,log-level` and returns it unchanged; the `of` crate
  forwards it (`of::chosen_log_level`) to platform and logging callers, which
  own the level vocabulary. A vendor property rather than `bootargs` keeps
  verbosity out of the boot loader's command line.
- Report failures as `FdtError` (`BadMagic`, `BadPtr`, `BufferTooSmall`, `ParseError`).

## Non-Responsibilities

- No controller-specific bindings, IRQ mapping or device registration.
  Legacy OF interpreters not migrated here are follow-up debt, not the intended
  final ownership of DT semantics.
- No global state: the crate has no statics; a tree is whatever
  `LinuxFdt` borrows, for as long as the caller keeps the blob alive.
- No mutation, patching, or DTB generation: read-only views over
  firmware-provided bytes.
- Borrowed tree/property views do not allocate. The optional alloc feature
  enables resource metadata collection, not hardware resource acquisition.
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
│   ├── property.rs       # Strict property encoding reads
│   ├── error.rs          # FdtError, Result alias
│   └── kernel_nodes/     # Chosen, Dice, InterruptController wrappers
└── Cargo.toml
```

## Architecture

```text
&[u8] DTB blob (or *const u8 via unsafe from_ptr)
      |
  FdtHeader + validate_structure (container and token stream validated)
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

Construction performs one allocation-free validation pass before publishing a
`LinuxFdt`. The accepted format uses the version-17 header (or a newer version
compatible with version 17). Checked block arithmetic establishes bounds,
relative alignment, nonoverlap, and a complete memory-reservation terminator.
The returned reservation view ends immediately after that terminator.

The structure validator checks balanced node tokens, exactly one empty root,
NUL-terminated UTF-8 names, properties preceding children, complete property
headers, name offsets, payload lengths, and alignment padding bounds. It requires
`FDT_END = 9` after the root closes, following the
[Devicetree flattened format](https://devicetree-specification.readthedocs.io/en/stable/flattened-format.html).
Nesting is limited to 63 nodes including the root, matching the existing
iterator's fixed parent storage and bounding recursive path lookup. Padding
bytes are ignored for compatibility with the checked-in crosvm fixture; their
ranges must still be present. Property names may share suffixes in the strings
block.

Traversal subsequently trusts these immutable structural invariants. Its
narrow lint exceptions document constructor-proven complete tokens, UTF-8
names, and delimiters. Property payloads remain uninterpreted bytes: device
semantics, cell formats, and reserved physical-address overlap are separate
checks owned by callers.

## Execution Context

- Borrowed parsing, structural queries and status work before allocator setup.
  Resource collection (mmio_resources) requires the optional alloc feature and
  an initialized allocator; it performs no hardware access.
- RISC-V calls the constructor at physical addresses before enabling paging.
  Token validation consumes NOPs separately and checks the final `FDT_END`
  after the root closes, leaving only the three node/property tokens in the
  dispatch. This avoids the absolute virtual-address jump table generated for
  the former five-token dispatch. Changes to this control flow need optimized
  normal and unit-test binary inspection and early-boot checks; source-level
  branch spelling alone does not constrain compiler lowering.
- `LinuxFdt::new` is the safe entry for an already-sliced blob;
  `from_ptr` is the unsafe boot-loader-pointer entry (null check, header
  read, then the header-declared total size).

## Concurrency Model

- No shared state; every value is `Copy` or a shared borrow, so concurrent
  readers of one DTB need no synchronization.

## Error Model

Construction returns `BadMagic` for a wrong magic, `BufferTooSmall` for a
truncated header or a buffer shorter than the declared total size, and `BadPtr`
for a null pointer in `from_ptr`. `ParseError` reports malformed container or
structure data, unsupported header compatibility, or excessive nesting.
Malformed structure therefore never becomes an absent-node result.

On a successfully constructed tree, accessors return `None` for absent nodes or
properties and for property values that cannot be decoded in the requested
semantic format. Iterators traverse already validated structural data.

## Firmware Trust Boundary

Both constructors validate container ranges and the complete structure before
returning a tree. Callers must keep the borrowed bytes alive and immutable;
raw-pointer callers must additionally establish readability of the complete
header-declared range before calling `from_ptr`. Structural validation does not
establish that firmware describes the actual hardware correctly or that each
property's value obeys its device binding.

## Unsafe Code

One unsafe entry point: `LinuxFdt::from_ptr(ptr)`. It forms two
`slice::from_raw_parts` views — header-sized, then `totalsize`-sized after
re-reading the header from the first view. The `# Safety` contract: `ptr`
must point to a readable DTB whose header and total-size range remain
accessible and immutable for the returned lifetime. All other code is safe and bounds-
checked.

## Design Decisions

- Zero dependencies and no allocation: raw parsing remains usable before
  the kernel heap exists.
- Borrowed, `Copy`-heavy views instead of an owned tree: boot-time code
  passes nodes and properties around freely without cloning, and the
  two-lifetime `FdtNode<'b, 'a>` lets iterators borrow the tree while
  values keep pointing into the blob.
- Version-17 headers only: a header with `version < 17` or
  `last_comp_version > 17` is rejected with `ParseError`. The structure
  block bounds check needs `size_dt_struct`, which older header versions do
  not define, so such blobs cannot be validated before use.
- Unaligned reads over alignment preconditions: boot firmware does not
  guarantee 4-byte alignment for the DTB, so `FdtData` reads big-endian
  fields byte-wise.
- Well-known node wrappers (`Chosen`, `Dice`, `InterruptController`) live
  in `kernel_nodes/` to keep node policy discoverable without pushing
  kernel conventions into the generic parser.
- `#![allow(rustdoc::bare_urls)]` is set crate-wide for spec-reference URLs
  in documentation.
