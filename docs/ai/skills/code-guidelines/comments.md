# Comments

Use this file when the change touches implementation comments
or local design rationale.

Rustdoc, crate-local `docs/design.md`, and crate-local `docs/security.md`
requirements are owned by `docs/ai/skills/module-docs/SKILL.md`.

## Comments

- comments should explain why, not restate what the code does;
- if a comment is compensating for confusing code,
  simplify the code first;
- document non-obvious design decisions and tradeoffs;
- cite external specs, ABIs, man pages, or algorithm sources
  when behavior depends on them;
- prefer semantic line breaks in prose-heavy comments and Markdown.

## When Reviewing

Check specifically for:

- comments that just paraphrase the code;
- missing rationale on non-obvious choices;
- stale implementation comments after behavior changes.
