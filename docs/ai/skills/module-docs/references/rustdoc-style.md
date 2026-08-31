# X-Kernel Rustdoc Writing Style

Use this reference when creating or updating rustdoc for X-Kernel. It turns
the recurring documentation patterns used by Rust `std` and `core` into
concrete project rules. Follow the requirement levels defined in the parent
`SKILL.md`; this reference adds detail without weakening them.

The standard library is a model for organization, precision, linking, and
examples. It is not text to copy. The X-Kernel source and verified behavior
remain the source of truth.

## Reader and Content Model

Write for a caller who understands Rust but does not yet know the component.
Document the observable contract and the information needed to use the API
correctly. Explain implementation details only when they affect that contract,
such as performance, blocking, ordering, safety, or lifecycle behavior.

Every rustdoc item:

- `SHOULD` open with one concise, complete summary sentence;
- `MUST` use the same names and terminology as the code;
- `MUST` distinguish implemented behavior from requirements placed on callers;
- `SHOULD` use present tense and active voice;
- `SHOULD` keep one main idea per paragraph;
- `MUST` put Rust identifiers, values, flags, and short expressions in code
  spans;
- `MUST NOT` repeat the signature, translate the identifier into prose, or
  describe the implementation line by line;
- `MUST NOT` claim behavior, guarantees, or supported contexts that the code
  does not establish.

Prefer specific statements such as "Returns `Error::ReadOnly` when the inode
belongs to a read-only mount" over vague statements such as "Returns an error
on failure."

## Default Information Order

Use the following flow when the information applies. This is a writing order,
not a requirement to create a heading for every step:

1. summarize the observable purpose;
2. explain important semantics and relationships;
3. explain the normal usage sequence or caller interaction;
4. state applicable errors, panics, and safety obligations;
5. show a practical example for a core public API.

Keep introductory prose before contract sections. Keep related contract
sections together. Do not add empty headings, and do not force a fixed heading
order when another order reads more clearly.

## Choose the Comment Kind Before Writing

Choose the comment according to its reader and subject:

| Comment | Subject and reader | Placement |
|---------|--------------------|-----------|
| `//!` | Crate or file-backed module introduction | At the top of the crate or module body, after the license header and before ordinary items |
| `///` | Public API or a documented module declaration | Immediately before the item; put the doc comment before the item's attributes |
| `//` | Implementation rationale, algorithm steps, state transitions, lock ordering, and local invariants | At the narrowest statement, block, field, or helper affected by the explanation |
| `// SAFETY:` | Why one `unsafe` block satisfies its required invariants | Immediately before the corresponding `unsafe` block |

Document a module either in its body with `//!` or on its declaration with
`///`, never both. Do not use item rustdoc as a replacement for an
implementation comment: callers need contracts, while maintainers need the
reasoning next to the code that depends on it.

## Crate-Level Documentation

Crate documentation `MUST` let a new caller answer all of these questions:

1. What capability does this crate provide within X-Kernel?
2. What state, resources, or abstraction does it own?
3. What important responsibility is outside its boundary?
4. Which public type, trait, or function is the normal starting point?
5. What is the normal call sequence or integration path?
6. Which initialization, lifecycle, blocking, concurrency, or safety
   constraints affect correct use?

The first paragraph `MUST` identify the crate's role, rather than merely
expanding its name. Later paragraphs `SHOULD` move from responsibilities and
boundaries to entry points and usage. A crate introduction `SHOULD NOT` be an
inventory of modules or exported symbols.

For an internal kernel crate, "how to use it" may be concrete prose rather
than a compilable example. Name the real caller, starting entry point, call
order, and required context. Do not invent standalone setup that the kernel
does not support.

## Module-Level Documentation

Every hand-written module `MUST` have module documentation that states its
specific responsibility within the parent crate. Documentation depth then
depends on the module's role.

### Public modules

A public module is a reader-facing API boundary. Its documentation `MUST`
explain its abstraction, main public items, normal usage, and applicable
caller constraints. Describe its relationship to the crate instead of listing
all exported items or repeating the crate introduction.

### Non-trivial internal modules

A private module `MUST` receive additional explanation when it owns state or
resources, defines a cross-module boundary, manages lifecycle or concurrency,
or implements a non-trivial algorithm. Include only the applicable parts:

- owned state, resource, policy, or abstraction;
- interactions with parent or sibling modules;
- the main entry point or operation sequence;
- non-obvious initialization, lifecycle, execution-context, concurrency,
  algorithmic, or safety constraints.

### Small private modules

A narrow declaration, codec, adapter, or pure-helper module normally needs one
precise sentence, for example:

```rust
//! Bounds-checked little-endian codecs for ext4 disk structures.
```

Do not force such a module to discuss ownership, callers, entry points,
lifecycle, or concurrency when none of those facts improves understanding. Do
not add a second paragraph that merely says which sibling calls the helpers or
which state the module does not own.

Document either the `mod` declaration with `///` or the module body with `//!`.
Do not document both locations with duplicate content.

## Normal Implementation Comments

Use ordinary comments for facts that maintainers need at a particular
implementation point but callers do not need as an API contract. Good subjects
include:

- why validation must precede publication or I/O;
- why a particular ordering, retry, barrier, or rollback rule exists;
- a representation or arithmetic invariant not encoded by the types;
- lock ordering, callback restrictions, or state transitions;
- an algorithm choice whose tradeoff affects maintenance.

Place the comment beside the code whose correctness depends on it. Prefer a
short explanation of the reason and consequence over a narration of the next
statement. Do not move all implementation reasoning into the module header,
and do not convert obvious private helpers into rustdoc solely to increase
coverage.

## Public Item Rules

### Structs, Unions, and Enums

The opening sentence `SHOULD` say what the type represents, not how its fields
are laid out. The remaining text `MUST`, when applicable, explain:

- where callers obtain or construct the value;
- the operations for which the value is used;
- ownership, lifetime, validity, and state invariants;
- lifecycle or synchronization behavior that callers can observe.

Every public field and enum variant `MUST` explain its semantic meaning. Do
not merely restate its name or Rust type. A public union `MUST` document which
fields may be read in each valid state and place any caller obligations under
`# Safety` where applicable.

For an error type, name the operation or API family that produces it and
explain what the value represents. When failure behavior is central to using
the type, a representative failure example `SHOULD` be included. This follows
the pattern used by standard-library error types such as
`alloc::string::FromUtf16Error`.

### Traits

Trait documentation `MUST` describe the capability or semantic contract that
implementations provide. It `MUST` identify:

- obligations shared by all implementers;
- relationships or ordering requirements among required methods;
- invariants that callers may rely on;
- provided-method behavior that is not evident from its signature.

Each required method `MUST` document its own caller-facing contract. An
`unsafe trait` `MUST` contain a `# Safety` section that states every invariant
an implementation must maintain and why safe code relies on it.

### Functions and Methods

Start with an action-oriented summary such as "Creates...", "Returns...",
"Checks...", "Converts...", or "Performs..." when that wording is natural.
The documentation `MUST`, when applicable, explain:

- behavior not evident from the name and signature;
- the meaning and units of non-obvious inputs and outputs;
- externally visible state changes and other side effects;
- ownership, borrowing, allocation, cleanup, and partial-effect behavior;
- whether the operation may block, sleep, retry, or require a specific
  execution context;
- how this operation relates to the preceding or following operation in the
  normal call sequence.

Do not create a parameter list that only repeats parameter names and types.
Use `# Arguments` or `# Returns` only when integrated prose cannot state the
contract clearly.

For a simple getter or field-like accessor, a one-sentence summary is usually
enough. Add units, range, sentinel meaning, snapshot semantics, or locking
behavior only when a caller cannot infer them safely from the type and name.
Do not add a mechanical example or multi-section contract merely because the
item is public.

### Constants, Statics, and Type Aliases

Documentation `MUST` state the semantic meaning of the item. Include units,
valid range, scope, sentinel meaning, lifetime, initialization, mutability, or
synchronization behavior when applicable. Do not restate only the literal
value or aliased type.

### Macros

Macro documentation `MUST` state its purpose and the result of expansion. It
`MUST`, when applicable, explain accepted invocation forms, evaluation and
ownership behavior, side effects, generated names, scope or hygiene
constraints, and panic or safety conditions. A core public macro `SHOULD`
include a practical example.

### Re-exports

Document the original item as the primary source of its contract. A re-export
`SHOULD` add documentation only when its placement gives it a different
user-facing role or when callers need guidance about which path to use. Do not
duplicate the full original contract at the re-export site.

## Contract Sections

Only add a contract section when it applies. Section text `MUST` state concrete
conditions and effects rather than a generic promise.

### `# Errors`

A public function or method returning `Result` `MUST` have `# Errors`.
Describe failures by trigger condition, not only by variant name.

When relevant, say whether failure leaves state unchanged, performs a partial
update, consumes an input, or requires caller cleanup. Group conditions when
they have the same observable result; do not enumerate internal errors that
cannot reach the caller.

Good shape:

```rust
/// # Errors
///
/// Returns `FsError::InvalidInput` if `name` contains a path separator, and
/// returns `FsError::AlreadyExists` if the directory already has that entry.
/// Neither failure modifies the directory.
```

### `# Panics`

A public API that can panic during supported use `MUST` have `# Panics`. Name
the exact caller-visible trigger and any state already changed before the
panic. State when a panic is debug-only if that distinction is stable and
relevant.

Do not document impossible internal assertions as a supported caller contract.
If a panic indicates an undocumented invariant or reachable bug, document the
real contract and fix or report the defect rather than normalizing it as
expected behavior.

### `# Safety`

Every `unsafe fn`, unsafe method, and `unsafe trait` `MUST` have `# Safety`.
State all obligations the caller or implementer must uphold. Cover every
applicable dimension:

- pointer validity, alignment, provenance, and bounds;
- initialized bytes and valid bit patterns;
- aliasing and mutability;
- referenced-object and mapping lifetimes;
- ownership transfer and double-release prevention;
- CPU, interrupt, scheduler, locking, pinning, and concurrency context;
- required ordering relative to initialization or teardown.

Explain why each obligation is required for soundness or kernel correctness.
Do not write only "the caller must ensure this is safe." Safe wrappers
`SHOULD` document the invariant they enforce when callers need it to reason
about behavior.

### `# Examples`

A core public API `SHOULD` include a minimal, realistic example that
demonstrates observable behavior. Standard-library examples are the model:
they usually show setup, one focused operation, and an assertion or visible
result.

Examples:

- `MUST` use the real public API and supported inputs;
- `SHOULD` demonstrate the normal path before exceptional paths;
- `SHOULD` include a representative failure when failure is central to the
  API, as standard-library conversion error types do;
- `SHOULD` compile as doctests when the repository environment permits;
- `MUST NOT` use invented types, unavailable initialization, or assertions
  that do not follow from the code;
- `MUST NOT` add an example that demonstrates syntax but not useful behavior.

Use ordinary runnable fences by default. Use `no_run` only when the example
can compile but cannot safely or practically execute in a doctest. Use
`ignore` only when platform-specific setup prevents compilation, and explain
the reason in nearby prose. Hidden `#` lines `SHOULD` be limited to incidental
setup that would distract from the contract being demonstrated.

An example is not mandatory when a kernel-only API cannot be constructed in a
meaningful doctest. In that case, the prose `MUST` still name the concrete call
sequence and context needed to use it.

## Intra-Doc Links and Code Formatting

Use an intra-doc link for the first meaningful mention of a main public type,
trait, function, module, or closely related API when navigation helps the
reader. Use code spans for later mentions and for private implementation
details.

Use reference-style link definitions when they improve sentence readability,
rename the displayed text, or disambiguate a method. For example:

```rust
/// Reports an error produced by [`from_bytes`].
///
/// [`from_bytes`]: Packet::from_bytes
```

Links `MUST` resolve to the intended item. Documentation `SHOULD NOT` link
every identifier, repeat the same link in one short section, or replace an
explanation with a linked API inventory.

## Applying the `std` and `core` Model

Use these standard-library patterns as writing references:

| Reference | Pattern to apply in X-Kernel |
|-----------|------------------------------|
| [`std::thread`](https://doc.rust-lang.org/std/thread/) and [`core::convert`](https://doc.rust-lang.org/core/convert/) | Start with purpose, then explain the abstraction, main entry points, usage, and important constraints. |
| [`alloc::string::FromUtf16Error`](https://doc.rust-lang.org/alloc/string/struct.FromUtf16Error.html) | For an error type, give a concise meaning, connect it to the producing API, and show a representative failure when useful. |
| [`alloc::string::String::new`](https://doc.rust-lang.org/alloc/string/struct.String.html#method.new) | For a constructor, state what it creates, add important allocation or state detail, and show the minimal normal use. |
| [`alloc::string::String::push_str`](https://doc.rust-lang.org/alloc/string/struct.String.html#method.push_str) | For a method, state the effect, document applicable panic conditions, and demonstrate the changed value. |
| [`core::ptr`](https://doc.rust-lang.org/core/ptr/) and [`core::mem::MaybeUninit`](https://doc.rust-lang.org/core/mem/union.MaybeUninit.html) | Make unsafe invariants, invalid operations, and the boundary between caller and implementation responsibilities explicit. |
| [`core::slice::index` source](https://github.com/rust-lang/rust/blob/main/library/core/src/slice/index.rs) | A narrow internal module can use one precise module-summary sentence. |
| [`alloc::collections::btree::node` source](https://github.com/rust-lang/rust/blob/main/library/alloc/src/collections/btree/node.rs) | Keep detailed representation and algorithm invariants in ordinary comments near the internal implementation. |

Match the level of precision and reader orientation, not the exact length or
section layout. A two-line wrapper may need only a precise summary and error
condition; a foundational unsafe abstraction may require extensive safety and
usage guidance.

The standard library does not give every private implementation module full
reader-facing rustdoc. X-Kernel's requirement that every hand-written module
has at least module docs is an additional local coverage rule. Apply the
standard library's proportionality: use a short summary for a simple private
module, rich documentation for a public or foundational abstraction, and
ordinary comments for local implementation reasoning.

## Good and Bad Comparisons

These comparisons calibrate documentation depth and specificity. The symbols
are illustrative. Reuse the pattern only after verifying equivalent facts in
the target code and its callers.

### Module introduction

Bad: this repeats the filename and replaces an explanation with a symbol list.

```rust
//! Buffer module.
//!
//! Contains [`Cache`], [`Slot`], [`read`], and [`write`].
```

Good for a non-trivial module: this states responsibility, ownership, the
normal entry path, and a caller-visible constraint.

```rust
//! Coordinates cached metadata blocks with block-device I/O.
//!
//! [`Cache`] owns shared cache entries but not the backing device. Callers
//! obtain a pinned slot through `Cache::get` before accessing block contents.
//! A cache miss may wait for I/O, so that path requires sleepable task context.
```

The longer form would be bad for a small pure helper. Use proportional depth:

```rust
//! Encodes and decodes little-endian on-disk block identifiers.
```

### Public API failure contract

Bad: neither the error trigger nor the failure effect is actionable.

```rust
/// Opens a child entry.
///
/// # Errors
///
/// Returns an error if the operation fails.
pub fn open_child(name: &str) -> VfsResult<File>;
```

Good: the summary defines the operation, and `# Errors` describes observable
conditions and state effects.

```rust
/// Opens the existing child named `name` relative to this directory.
///
/// The lookup does not follow a symbolic link in the final component.
///
/// # Errors
///
/// Returns `VfsError::InvalidInput` if `name` is not one path component, or
/// `VfsError::NotFound` if the directory has no matching child. Neither
/// failure modifies the directory.
pub fn open_child(name: &str) -> VfsResult<File>;
```

### Unsafe API contract

Bad: this leaves the meaning of safe entirely undefined.

```rust
/// # Safety
///
/// The caller must ensure this is safe.
pub unsafe fn header_from_ptr(ptr: *const Header) -> &'static Header;
```

Good: this names the validity, lifetime, aliasing, and concurrent-access
obligations that make the operation sound.

```rust
/// Returns the header stored at `ptr`.
///
/// # Safety
///
/// `ptr` must be non-null, aligned for `Header`, and have valid provenance for
/// reading `size_of::<Header>()` fully initialized bytes. Those bytes must
/// remain mapped and allocated for the returned lifetime. No CPU or device may
/// mutate, unmap, or reuse them while the returned reference is live.
pub unsafe fn header_from_ptr(ptr: *const Header) -> &'static Header;
```

## Reference Basis

- [The Rustdoc Book: How to write documentation](https://doc.rust-lang.org/rustdoc/how-to-write-documentation.html)
  defines crate/module and item documentation conventions and recommends
  documenting public APIs.
- [The Rust Reference: Comments](https://doc.rust-lang.org/reference/comments.html)
  defines ordinary comments, outer item docs, and inner crate/module docs.
- [The Rust Style Guide](https://doc.rust-lang.org/style-guide/)
  prefers outer docs for items, reserves inner docs for crates/modules, and
  places doc comments before attributes.
- [The Rust API Guidelines documentation checklist](https://rust-lang.github.io/api-guidelines/documentation.html)
  motivates crate docs, useful examples, failure sections, safety contracts,
  and hiding unhelpful implementation detail. Apply its recommendations within
  reason, as the guideline itself requires.
- [The `missing_docs` lint](https://doc.rust-lang.org/rustc/lints/listing/allowed-by-default.html#missing-docs)
  checks public-item coverage; it does not establish a requirement to document
  every private helper.
