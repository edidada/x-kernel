# klogger — Design

## Purpose

`klogger` is the X-Kernel logging core. It implements the `log` crate's `Log`
trait and routes every `error!`/`warn!`/`info!`/`debug!`/`trace!` record to two
consumers:

- **the console**, filtered by a runtime per-target filter;
- **the kernel log ring buffer**, captured according to the `log` ceiling and
  read back by `syslog(2)` / `dmesg`.

Console output goes through the `kiface_logging_api` providers: `LogSinkIf`
writes characters and `LogContextIf` supplies the time, CPU and task id.
`kruntime` links both providers before `init_klogger`. Subsystems depend on
`klogger`'s macros and filter API only, never on a concrete output channel.

## Background

A single global level cannot serve real bring-up work: `debug` is needed for one
stack (a rising driver, a filesystem probe) while a global `debug` drowns the
console in IPI and syscall lines. Lowering the level afterwards then leaves no
way to read back what happened, because `syslog(2)` was a stub that returned 0.

This module separates the two questions:

- **what is shown**: the runtime filter — a default level plus per-target
  overrides matched by path prefix;
- **what happened**: a 64 KiB ring that captures every record that clears the
  `log` ceiling, regardless of the console decision.

Both share one ceiling: `log::max_level` is the filter's ceiling and only serves
as the cheap pre-check in the `log` macros; the real per-target decision happens
inside `Log::log`. That is what lets an override sit *above* the default level.

## Scope

```
util/klogger/
├── src/
│   └── lib.rs        (filter, ring buffer, macros, level parsing)
├── ut-tests/
│   └── lib.rs        (guest cases for the spec grammar and the ring semantics)
├── docs/
│   ├── design.md
│   └── security.md
└── Cargo.toml
```

## Architecture

```
error!/warn!/info!/debug!/trace!
        │  (log crate: first checks the log::max_level ceiling)
        ▼
KernelLogger::log
        ├── 1. LineBuf assembles "<N>[time cpu:tid target:line] args\n"
        │       └──► LOGBUF (64 KiB ring, overwrite oldest) ──► syslog(2)/dmesg
        └── 2. filter_enabled(target, level)
                ├── yes ──► print_fmt ──► LogSinkIf::write_str
                └── no  ──► drop (the record is already in the ring)
```

| Component | Responsibility |
|-----------|----------------|
| `Filter` | default level plus up to `MAX_FILTER_RULES` `target=level` rules; longest-prefix match |
| `FILTER` | the global filter, guarded by `SpinNoIrq` |
| `LogBuf` | fixed 64 KiB byte ring; `push` overwrites the oldest bytes, `read` copies the oldest unread window, `release` drops exactly that window |
| `LOGBUF` | the global ring, guarded by `SpinNoIrq` |
| `LineBuf` | fixed 256-byte line writer; the last byte is reserved for the terminator and truncation never splits a character |
| `print_fmt` | serializes console output |

## Calling constraints / execution context

- Any logging macro may be called from any context: early boot (before
  `init_klogger` the `log` crate has no logger and the macro is a no-op),
  interrupt context, softirq, and ordinary task context.
- `LOGBUF`, `FILTER` and the console lock are all `SpinNoIrq`. Their critical
  sections perform no user-memory access, no sleeping and no allocation;
  `LineBuf` and the ring are fixed-size, so the record path never allocates.
- User copies for `syslog(2)` happen **outside** `klogger`: `ksyscall` allocates
  a kernel buffer, calls `logbuf_read`, releases the lock and only then writes to
  user space. `logbuf_release` runs after that copy succeeded and drops exactly
  the window it was given; a window the ring overwrote in the meantime is stale
  and drops nothing.
- `set_filter_spec` is callable at runtime, including from the
  `/proc/sys/kernel/printk` write path. It replaces the rule set, the default
  level (only when the spec carries a bare level) and the ceiling in one
  critical section.
- `init_klogger` runs once on the primary CPU during boot.

## State machine

A spec is validated as a whole and installed as a whole; there are no partial
updates. Its rule set always replaces the previous one; its default level
replaces the previous default only when the spec has a bare level token, so
`ksyscall=trace` retargets one module instead of silently resetting the rest of
the kernel to a hard-coded level:

```
boot default (LOG_LEVEL_* / LOG_FILTER, warn)
    │ set_filter_spec(dt / loglevel= / quiet / debug / LOG_FILTER)
    ▼
{ default, rules[] } ──set_filter_spec(spec), validated──► rules := spec rules
                     │                                     default := spec level, or kept
                     └─rejected──► unchanged (no rule keeps a new value)

set_log_level_filter(level) ≡ set the default, drop every override
```

The ring has two states: appending while it has room, and overwriting the oldest
byte once it is full. Every byte that leaves the buffer — either because a reader
released it or because the ring overwrote it — advances the buffer's sequence
position (`unread_from`), which is how a `syslog(2)` reader acknowledges exactly
the bytes it delivered. `logbuf_clear` drops everything and invalidates every
outstanding reader position.

### Reading and releasing

`logbuf_read(out)` copies the **oldest unread** bytes, in log order, and returns
a `LogRead` window whose private `from` field is the sequence position of the
first copied byte (the byte count is readable through `LogRead::copied()`).
Reading does not consume: `logbuf_release(read)` drops that window and nothing
else, and only if the window's sequence position still matches the ring, i.e. it
was neither already released nor overwritten while the caller was copying the
bytes to user space. A stale window drops nothing: its bytes no longer exist, and
dropping a same-sized range instead would discard records the reader never
received.

Reads therefore start where Linux's `syslog_print_all` starts — at the oldest
unread record — so a reader that drains in chunks makes progress instead of
re-reading the newest tail, and the released window is exactly the returned one
even though the copy happens outside the lock.

## Algorithms

### Handling one record

1. The `log` crate applies `log::max_level()` (the spec's ceiling: the maximum
   of the default level and every override). Records below it never reach the
   logger.
2. `Log::log` formats `<N>[time cpu:tid target:line] args` into a `LineBuf`.
   `<N>` is X-Kernel's own level number (`1..=5`, error..trace).
3. The terminating newline is appended by `LineBuf::terminate`, not by
   `writeln!`: the writer reserves one byte, so a record that fills the buffer is
   truncated to 255 bytes and still ends with `\n`. Truncation also stops at a
   character boundary, so the ring never holds a split code point.
4. The whole line is pushed into `LOGBUF` (overwriting the oldest bytes when
   full). Both the push and a ring read copy at most two contiguous segments
   with `copy_from_slice`, so the time spent inside the IRQ-off guard scales with
   the byte count rather than with a per-byte loop.
5. Only then does `filter_enabled(target, level)` decide whether the record also
   goes to the console. A dropped record still stays in the ring — that is what
   makes a quiet console and a complete boot log compatible.

### Parsing a filter spec

A spec is `[<level>] [<target>=<level>]...`, with tokens separated by whitespace
or commas:

1. a token containing `=` splits into `target` and `level`; `level` must be
   `0..=5` or `off/error/warn/info/debug/trace`; `target` is trimmed, its
   redundant trailing `*` removed, and its length must be `1..=MAX_FILTER_TARGET`;
   at most `MAX_FILTER_RULES` rules are accepted;
2. a token without `=` sets the default level, the last one winning; a spec with
   no such token keeps the level that is already in force;
3. a spec with neither a rule nor a bare level is invalid;
4. only after the whole spec validates is it installed, together with the
   ceiling, in one `FILTER` critical section. Any invalid token returns
   `FilterSpecError` and leaves the previous spec, and the previous ceiling,
   untouched.

`filter_spec(buf)` is the inverse: it writes the canonical text of the active
filter into the caller's buffer and returns the length, or `None` when the buffer
is too small. `/proc/sys/kernel/printk` and the boot banner share it, so the
buffer they pass is `MAX_FILTER_SPEC`, which is derived from the grammar
(`MAX_LEVEL_NAME + MAX_FILTER_RULES * (1 + MAX_FILTER_TARGET + 1 +
MAX_LEVEL_NAME)`) and therefore covers every spec `set_filter_spec` accepts — a
spec that could be installed but not reported back would make the world-readable
node fail with `InvalidInput` and hide the active overrides from the boot banner.

The public entry points and their directions:

| Entry point | Direction |
|-------------|-----------|
| `set_filter_spec(spec)` | `kruntime` (boot, from the DT/cmdline/Kconfig sources) and `procfs` (`/proc/sys/kernel/printk` write) install a spec's rules, plus its default level when it has one; a rejected spec leaves the previous filter untouched |
| `set_log_level(spec)` | convenience wrapper that applies a spec and ignores a rejection; used by callers that only have a level string |
| `set_log_level_filter(level)` | set the default level and drop every override (`procfs` tests and internal resets) |
| `filter_spec(buf)` | `procfs` read side and the boot banner report the active spec |
| `current_log_level()` | reports the filter's **default** level (not the `log` ceiling, which also includes the overrides) |
| `logbuf_len` / `logbuf_read` / `logbuf_release` / `logbuf_clear` | `ksyscall`'s `syslog(2)` read path and the unit tests; `LogRead` has private fields, so only a window returned by `logbuf_read` can be released |

### Where the boot filter comes from

Precedence is decided by `kruntime`; `klogger` only validates and applies:
`/chosen/x-kernel,log-level` → `loglevel=` / `quiet` / `debug` on the command
line → Kconfig `LOG_FILTER` → Kconfig `LOG_LEVEL_*`. The `LOG_LEVEL_*` choice is
installed **first**, before any source is consulted: a source may carry rules
only, and such a spec keeps the level already in force, so it has to be the build
level rather than the logger's hard-coded default. Every rejected value is
reported and the next source is tried, so a typo cannot mute the kernel. Those
reports go out through `kprintln!` (the unfiltered console path) instead of
`warn!`: the resolver is installing the very level that would filter the warning,
so an `error`- or `off`-level build would otherwise hide the diagnostic that
explains why its configured filter was ignored.

Whitespace separates command-line parameters, so a full spec must stay inside one
word there: `loglevel=warn,ksyscall=debug`. The comma form works in every source.

## Concurrency model

| Shared state | Synchronization | Notes |
|--------------|-----------------|-------|
| filter `FILTER` | `SpinNoIrq<Filter>` | read on the logging hot path; rule lookup is a bounded scan of a fixed array |
| ring `LOGBUF` | `SpinNoIrq<LogBuf>` | writes, reads and releases are mutually exclusive; one record copies at most 256 bytes |
| console output | `SpinNoIrq<()>` inside `print_fmt` | keeps multi-CPU output from interleaving |
| `log` ceiling | atomic in the `log` crate | updated inside the same `FILTER` critical section that installs the spec |

`SpinNoIrq` disables interrupts, so the hot path cannot deadlock against an
interrupt handler on the same lock, but it also means no critical section may
block, allocate, or log again. `LineText` formatting happens before the ring lock
is taken, and `print_fmt`'s lock is never held together with `LOGBUF`.

## Design decisions

- **Separate display from capture**: the console filter and the capture point are
  independent, so post-mortem analysis does not require opening the console
  filter globally.
- **Spec instead of a single level**: `warn ksyscall=debug` retargets one stack
  and can be changed at runtime without rebuilding.
- **`log::max_level` as the ceiling**: the `log` macros cannot distinguish
  targets, so the ceiling is only the cheap pre-check and the real decision lives
  in the logger. The cost is that the ceiling must admit the widest override, so
  a `trace` rule lets every trace record enter `log()` before being dropped.
- **Fixed ring and fixed line**: the record path must not allocate, and a runaway
  message must not lose its interesting prefix; a record is truncated at 255
  bytes plus its terminator.
- **X-Kernel level numbering** (`0..=5`, higher is more verbose): Linux's
  `SYSLOG_ACTION_CONSOLE_*` actions are deliberately not implemented because
  `console_loglevel` is the inverted domain and mixing the two would mislead
  callers.
- **Consuming instead of clearing**: `READ_CLEAR` drops only the window it
  returned, so a short or zero-length destination keeps the rest readable.
- **Sequence positions instead of raw indices**: a reader's window is identified
  by its sequence position, so a release stays correct across the gap in which
  the bytes are copied to user space and the ring may wrap.

## Drop / resource release

Nothing needs destructors: the filter and the ring are static storage and the
installed logger is `&'static`. The closest operation to a release is
`logbuf_clear`, which resets the indices (and invalidates outstanding reader
positions) without wiping the bytes; they disappear as later records overwrite
them.