# Capability Catalogue

Use this skill when an AI agent is about to write a new helper, container,
lock or wait protocol, allocator, parser, ID pool, per-CPU structure or
similar reusable mechanism; when a change touches code that a capability
card cites; or when maintaining the cards themselves.

The catalogue lives in `docs/capabilities/`. Its `README.md` is the canonical
description of the card format, card states, commands, CI and the maintenance
flow; this skill only routes to it.

## Before writing a reusable mechanism

1. Search `docs/capabilities/helpers.md` by domain and by the words of the
   need (card aliases are English and Chinese).
2. Open the card of each candidate and compare its exclusions and contract
   with the use: SPSC vs MPMC, fixed vs growable, IRQ / NMI context,
   allocation on the hot path, blocking, ID reuse, address stability.
3. Treat a contract value of `unknown` as not established; read the cited
   source before relying on it.
4. Use a card only when it is fresh and its status is `active`
   (`make check_capabilities` prints every other card with its state). If an existing capability fits,
   use it; if it almost fits, say why it does not in the change description.

## When a change touches cited code

Run `make check_capabilities` after the change. Moving cited code needs
nothing from you: cards name items and quotes, not lines. If a card you made
stale is one you can fix within the change, update its evidence or contract,
run `make capabilities`, and stamp it with
`cargo xkmake capability stamp <capability>`; otherwise mention the stale card
in the PR description so the catalogue maintainer handles it.

## Maintaining cards

Follow `docs/capabilities/README.md` ("Adding or changing a card" and
"Maintenance proposals"). Rules that agents get wrong:

- every non-`unknown` claim cites `path` + owning `item` + verbatim `quote`
  (no line numbers; the quote must be unique within the item); never infer a
  contract from the absence of a call, lock or `unsafe`;
- a card drafted by a model is reviewed by a model of another family and then
  approved by a person; do not stamp an unreviewed card as reviewed;
- do not edit `stamps.json` or `helpers.md` by hand;
- model drafting of card updates stays disabled (`maintenance.model_draft`);
  enabling it needs the user's explicit cost approval.
