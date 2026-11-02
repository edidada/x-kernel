# Capability catalogue

The catalogue lists reusable capabilities of X-Kernel and its locked
dependencies: containers, locks and wait queues, allocators, user-memory
access, MMIO, parsers, per-CPU data, and so on. Each capability is one card
that names the symbols to use, the operations they offer, the contract
(no_std, allocation, blocking, IRQ / NMI, ownership, errors and bounds,
concurrency) and when not to use them. Every claim cites a file, the item
that holds the evidence and a verbatim quote, so it can be checked
mechanically. Cards never carry line numbers: code that moves, or changes
elsewhere in the same file, leaves a card untouched and fresh.

It serves three readers from one source:

- developers and coding agents look up an existing capability before writing
  a new helper, container or protocol;
- xreview uses the cards to point out re-implementations in review;
- maintainers keep the cards current with a deterministic check.

## Files

| Path | Written by | Content |
| --- | --- | --- |
| `helpers.md` | `xkmake` (generated) | The human-readable helper catalogue, grouped by domain. Start here. |
| `cards/<capability>.yaml` | people (and agents), reviewed in PRs | One card per capability. |
| `catalogue.yaml` | people | Schema version, baseline commit, known families, families with no implementation, maintenance settings. |
| `stamps.json` | `xkmake` only | Per card: the token fingerprint of every cited item at the last review (file, path inside the file, hashes). No file contents, no line numbers. |

## Using the catalogue

Before writing a helper, data structure, lock, queue, parser or allocator,
search `helpers.md` (by domain, or for the words in the card aliases) and
open the card of a candidate. Then check the card's exclusions against your
case: a capability with the same shape is often not interchangeable (SPSC vs
MPMC, fixed vs growable, IRQ context, allocation on the hot path, ID reuse,
address stability).

Contract values are what the cited source states. `unknown` means no
evidence was found, not that the property is absent: do not rely on an
`unknown` property in IRQ, NMI or allocation-free code without reading the
source.

A card's state (below) tells how much to trust it, and its `status` whether it
is in use at all: a `needs-owner` card is kept as a record but is not
suggested until its open questions are answered, and `helpers.md` lists the
questions. `helpers.md` shows suspended cards with a warning and omits
retired ones; run `make check_capabilities` to see the current state of
every card.

## Card format (schema 2)

The schema version is `schema` in `catalogue.yaml`. The checker and xreview
both ignore fields they do not know, so optional fields can be added without
breaking the other reader; changing the meaning of an existing field needs a
new schema version. Schema 2 replaced the line-anchored evidence and the
review record of schema 1; both readers refuse schema 1 catalogues and cards
(any `at`, `line`, `review` or `lifecycle` field) with a message pointing
here.

| Field | Required | Meaning |
| --- | --- | --- |
| `capability` | yes | Identifier; equals the file name. |
| `title`, `summary` | yes | One-line title; short description. |
| `family` | yes | One of `families` in `catalogue.yaml` (e.g. `container.ring`, `sync.wait_queue`). |
| `source` | yes | `{kind: workspace, crate, path}` for a repository crate, or `{kind: dependency, crate, version}` for a crates.io package; the version must be in `Cargo.lock`. |
| `symbols` | yes | `{fqn, kind, path, role}`. `kind`: `struct`, `enum`, `union`, `trait`, `fn`, `macro`, `type`, `mod`, `const`, `static`. `role`: `primary`, `constructor`, `operation`, `helper`, `reexport`. `path` is the file that declares the item: a repository path, or a path inside the dependency crate. `fqn` is the public path; it names the item as described under "Items" below. |
| `operations` | no | `{name, symbols, effect, evidence}`; `symbols` must appear in `symbols`. |
| `usage_protocol` | no | Ordered `{step, evidence}`. |
| `contract` | yes | Each of `no_std`, `allocation`, `blocking`, `irq_nmi`, `ownership_lifetime`, `errors_bounds`, `concurrency` as `{value, evidence}`. A value other than `unknown` needs evidence. |
| `layers` | no | `{depends_on, used_by, allowed_layers, evidence}`; `allowed_layers` stays `unknown` unless a document states a layering rule. |
| `aliases` | yes | `{en: [...], zh: [...]}`, both non-empty. |
| `identifiers` | no | Words common in code that re-implements the capability. |
| `exclusions` | no | When the capability must not replace new code. |
| `counter_examples` | no | `{path, item, why, evidence}`: same-shaped code that is not interchangeable; `item` is optional. |
| `notes` | no | Free text. Do not cite lines here either; name the file and item. |
| `status` | no | `active` (default), `needs-owner`, `suspended` or `retired`; see "Card states". |
| `reason` | for `suspended`, `retired` | Why the card is out of use. |
| `open_questions` | for `needs-owner` | `{field, question, evidence}`: what an owner must decide before the card is used, phrased as a question; `evidence` is optional and checked like any other. Only a `needs-owner` card may carry open questions. |

Who reviewed a card, and when, is in the Git history of the card, not in the
card.

**Evidence** is `{path, item, quote}`. `path` is a repository path; paths in
a dependency are written `<crate>-<version>/<path in the crate>`. Absolute
paths and `..` are rejected.

- `item` names the item that holds the quote, such as
  `kirq::manager::register` or `kmutex::mutex::RawMutex::lock`. The quote
  must appear in that item (its attributes and doc comment included) exactly
  once; when it appears more often, extend the quote.
- Without `item`, the quote must appear exactly once in the whole file. This
  is for text outside any item: `Cargo.toml`, documents, crate attributes such
  as `#![no_std]`, module docs, `use` and `impl` headers. A quote without
  `item` that lies inside a named item of a Rust file is an error: name the
  item, so the card follows the item when code moves.

The comparison collapses whitespace and also tries with leading comment
markers (`//`, `///`, `//!`, `*`, `#`) removed, so a quote may span
doc-comment lines.

**Items** are resolved with `syn` in the file the card names: the item must
be written there literally (macro-generated items, `cfg` evaluation and name
resolution are out of scope). An item's path inside its file is the chain of
inline modules, the implementing type or trait of an associated item, and
its name (`Ring::push`, `inner::LIMIT`). A card path names the items whose
chain is a tail of it, preferring the longest match; the inline modules may
be left out, since public paths often re-export items out of private modules.
Everything before that tail (crate and module names) is for readers. A
`macro` symbol matches `macro_rules!` or a proc-macro function (by its
invocation name). Several declarations of one path in one file (`cfg`
variants, methods of one type in several impl blocks) are one item: its
fingerprint covers all of them, and a quote may appear once in each.

## Card states

The checker derives a state for every card that is not retired:

| State | Set by | Meaning | Consumers |
| --- | --- | --- | --- |
| fresh | checker | Every cited item is found (in its file, or in another file of the crate when it moved) and matches its stamp, and every quote is present exactly once. | Use when `status` is `active`. |
| stale | checker | A cited item's signature, visibility or body changed, an item is missing, renamed or now declared in several files, a quote is gone or no longer unique, a quote outside items now lies inside one, the locked dependency version changed, or the card was never stamped. | Reference only, marked unverified; never as a suggestion. |
| suspended | people (`status: suspended`), or checker after the grace period | Reported wrong, or stale for longer than `maintenance.stale_grace_days`, measured in commit time from the first commit that made it stale. | Do not use. |
| retired | people (`status: retired`) | The capability is gone or the card is withdrawn. Not checked or rendered. | Do not use. |
| unchecked | checker | The source could not be read here (dependency source not fetched, file does not parse). An environment problem, not a card problem. | Treat as stale. |

`status: needs-owner` does not change the state: the card is checked like an
active one, but it is not usable until an owner answers its open questions
and sets it back to `active` (removing `open_questions`).

Moving code is not a change. When an item moved to another file, the card
stays fresh and the report notes the new file; update the card's `path` the
next time the card is edited. There is nothing to rewrite mechanically, so
the checker never writes cards.

Item comparison uses token hashes with doc comments removed, so formatting,
comment and doc-comment changes are not substance; doc comments are checked
through the quotes that cite them. An edit outside the cited items, even in
the same file, does not affect a card.

## Commands

```bash
make check_capabilities                 # schema + every card against the tree
make check_capabilities CAPABILITY_REPORT=out/capabilities   # also report.json + proposals
make capabilities                       # regenerate helpers.md
cargo xkmake capability stamp <capability>...   # record the reviewed state of cards
cargo xkmake capability stamp --discovery       # accept the current Cargo.lock packages
cargo xkmake capability render                  # same as make capabilities
```

Dependency cards read the crate source from the Cargo registry cache; run
`cargo fetch --locked` first on a fresh machine, otherwise those cards are
`unchecked`. `make check_capabilities` fails when a schema error, a stale or
unchecked card whose status is `active` or `needs-owner`, or an outdated
`helpers.md` is found; discovery hints and notes do not fail it.

`stamp` refuses a card that does not match the tree as written (an item not
in the file the card names, a quote missing or not unique): stamping records
what a reviewer verified, it never repairs a card. A `stamps.json` of another
schema is refused by `check`; `stamp --all` replaces its card stamps and keeps
its discovery baseline.

`helpers.md` is generated from the cards alone. Entry points link to their
files, not to lines, so the page never goes out of date when code moves; the
lines at the checked commit are in `report.json`.

## Adding or changing a card

1. Write `cards/<capability>.yaml` following an existing card; every claim
   cites `path` + `item` + `quote` (no line numbers), and what you cannot cite
   is `unknown`.
2. Run `make capabilities` (regenerates `helpers.md`) and
   `make check_capabilities`; fix every error.
3. Have the card reviewed against the source; cards drafted by a model are
   reviewed by a model of another family and then by a person. Questions the
   review cannot settle go into `open_questions` with `status: needs-owner`;
   name reviewers in the commit, not in the card.
4. Run `cargo xkmake capability stamp <capability>` and commit the card,
   `stamps.json` and `helpers.md` together.

## CI and nightly upkeep

**PR CI (advisory).** The Jenkins `Build Check` leg named in the Jenkinsfile
(`capabilityCatalogueCheckPlatform`) runs `cargo fetch --locked` and
`make check_capabilities` after its build. The outcome is printed in that
stage's log and never fails the build. It is advisory at first because:

- the false-stale rate on ordinary PRs is unknown: a refactor of cited code
  would otherwise block authors who do not own the catalogue;
- dependency cards need registry sources, and the fetch adds a network step
  to the leg.

Once the reviewed card set has landed and a few weeks of advisory runs show
the false-stale rate, make schema errors and an outdated `helpers.md`
blocking (they are local to the PR, like the other hygiene checks under
`make clippy`). Staleness stays advisory in PRs and is handled by the nightly
job, so a PR is never blocked by cards it did not touch.

**Nightly on `main` (to be set up).** A separate Jenkins job (or any
scheduler with a checkout of `main`) runs, for example at `H 3 * * *`:

```bash
cargo fetch --locked
make check_capabilities CAPABILITY_REPORT=artifacts/capabilities || true
```

and archives `artifacts/capabilities/` (`report.json`, `proposals/*.md`).
Moved code needs no PR: cards carry no lines. Stale cards become maintenance
proposals for a person. Posting proposals needs a CI bot account with Gitee
write access, which does not exist yet; until it does, a catalogue
maintainer runs the same commands weekly.

## Maintenance proposals

A proposal is `proposals/<capability>.md` in the report directory, written
for every stale or suspended card: the reasons, the changed cited items, and
a `git diff` since the stamp commit of each file involved (truncated). Hints
about new `Cargo.lock` packages or workspace crates without a card go to
`proposals/discovery.md`.

Resolving a proposal is an ordinary PR:

| Problem | Action |
| --- | --- |
| Code moved | Nothing; the card stays fresh. Update `path` when next editing a card whose item moved to another file (the report notes it). |
| Quote no longer unique | Extend the quote (or name its item); stamp. |
| Cited code changed | Re-read the code; update evidence and contract (unknown when unsure); stamp. |
| Symbol removed or renamed | Point the card at the new item (the report lists rename candidates with the same token shape) or retire the card. |
| Contract evidence contradicted (e.g. a function now allocates) | Reset the field to `unknown` until re-verified; stamp. |
| Card reported wrong (author, reviewer, maintainer) | Set `status: suspended` with a `reason` immediately; then fix or retire. |
| Dependency version changed | Move evidence to the new `<crate>-<version>/` paths after re-reading; stamp. |
| New capability (discovery hint, or a review comment "use existing X") | Draft a card, or accept the package with `stamp --discovery`. |
| Checker or nightly job failure | The catalogue keeps its last good state; fix the job. Consumers stop using cards whose last successful check is older than their own limit. |

**Approval.** A catalogue maintainer approves every card PR; a change to a
contract value also needs a maintainer of the cited crate. Nothing becomes a
card or a contract value without that approval.

## Model drafting (off)

An optional nightly step could draft updates for stale cards and discovery
hints with a model: re-read the cited code, refresh evidence and contract
(unknown stays unknown), have the draft reviewed by a model of another
family, and attach it to the proposal for human approval. It is not
implemented. `maintenance.model_draft` in `catalogue.yaml` must be
`disabled`, and the checker rejects any other value. Enabling it spends model
quota and needs an explicit cost approval (expected cost per night and
budget cap) before any code lands; even then a draft is only ever a proposal.

## Consumers (xreview)

xreview reads the catalogue from the x-kernel commit it reviews against (the
PR base), so cards and code always match:

- cards: `docs/capabilities/cards/*.yaml` and `catalogue.yaml`, schema 2 as
  above; refuse any other `schema`, and schema 1 card fields;
- states: run `cargo xkmake capability check --report <dir>` on that commit
  (or take the nightly `report.json` for the same commit) and read
  `report.json`. Suggest only cards whose `usable` is true (`status` is
  `active` and `state` is `fresh`); show other stale cards as unverified
  reference; never show `needs-owner`, `suspended` or `retired` cards, in any
  state;
- per card, `report.json` carries `capability`, `family`, `status`, `state`,
  `usable`, `reasons`, `notes`, `open_questions` (`{field, question}`),
  `changed_items` (`{item, change, stamped_path, path}`; `change` is
  `signature`, `body`, `missing` or `ambiguous`), `stale_since`
  (`{commit, age_days}` or null), and the resolved locations: `symbols`
  (`{fqn, path, line}`) and `evidence` (`{field, path, item, line,
  last_line}`). Lines are those of the checked commit; they are the only
  place line numbers appear;
- at the top level, `report.json` carries `schema`, `commit`, `dirty`,
  `page_current`, `counts`, `findings` and `hints`.

A consumer that checks cards itself (without `report.json`) must apply the
same rules: a card goes stale only when a cited item's fingerprint changes or
an item or quote is lost, never because code moved or its file changed
elsewhere.

## Production feedback

Review outcomes flow back as card changes, always through an x-kernel PR:

- xreview records, for every reuse suggestion, the capability and the
  catalogue commit, and the outcome (accepted, rejected or "不适用" with the
  author's reason);
- a rejected suggestion with a reason becomes a proposed `counter_examples`
  entry or exclusion for that card;
- per-card precision is shown on the xreview panel; when rejections in a
  window exceed xreview's threshold, xreview raises a proposal to suspend the
  card, and a maintainer sets `status: suspended` in a PR;
- a reviewer comment "use existing X" where X has no card becomes a
  discovery proposal.

The xreview side of this flow is tracked in the xreview tickets for the
capability catalogue.

## Limits

- The checker does not expand macros, evaluate `cfg` or resolve names; it
  verifies that the cited text and items exist and are unchanged, not that a
  quote supports the claim built on it. That judgement belongs to review.
- Discovery hints only cover new `Cargo.lock` packages and workspace crates;
  new public types with many callers need precise code facts and are not
  detected yet.
- Counter-examples carry no fingerprint; the checker only notes when their
  file or item is gone.
- An item path whose owner matches nothing falls back to a free item of the
  same name; the stamp then pins the exact chain, so a later mismatch shows
  up as a changed or missing item.
