# Naming And Structure

Apply these rules whenever code introduces or renames
types, functions, fields, locals, modules, or files.

## Names

Apply `docs/ai/code-guidelines/naming-cn.md` (`CG-NAME-*`).

## File And Module Structure

- keep one major concept per file when practical;
- organize files and impl blocks for top-down reading;
- place public entry points before private helpers where possible;
- split files when multiple distinct concepts start competing for attention;
- for visibility, apply
  `docs/ai/code-guidelines/modules-cn.md` (`CG-MOD-001`);
- prefer existing workspace abstractions and dependencies
  over ad hoc local reinvention.

## When Reviewing

Check specifically for:

- violations of `CG-NAME-*`;
- modules that are too broad to audit comfortably;
- helpers exposed more widely than required.
