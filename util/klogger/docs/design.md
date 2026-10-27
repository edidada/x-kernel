# klogger — Design

## Purpose

`klogger` is the X-Kernel logging core. It implements the `log` crate's `Log`
trait and routes every record to two consumers:

- **the console**, filtered by a runtime filter (default level plus per-target
  overrides);
- **the kernel log ring buffer** (`LOGBUF`), which records everything that
  reaches the logger and is read back by `syslog(2)` / `dmesg`.

Console output goes through the `kiface_logging_api` providers: `LogSinkIf`
writes characters and `LogContextIf` supplies the time, CPU and task id.
`kruntime` links both providers before `init_klogger`. Subsystems depend on
`klogger`'s macros and filter API only, never on a concrete output channel.

## Background

A single global level cannot serve real work: `debug` is needed for one stack
while a global `debug` drowns the console in IPI and syscall lines, and lowering
the level afterwards used to leave no way to read back what happened (`syslog(2)`
was a stub returning 0).

Three decisions are therefore kept separate, the way Linux separates its console
level from its printk ring:

| Role | Whose | What it decides | Can it change at runtime? |
|------|-------|-----------------|---------------------------|
| **Build cap** | `LOG_LEVEL_*` (Kconfig) | the highest level this image can produce; the `log` crate's own filter | no — rebuild to widen |
| **Capture** | `LOGBUF` | which records are stored for `dmesg`: **everything up to the cap** | no |
| **Console** | `Filter` | which records are printed: a default level plus per-target overrides | yes, `/proc/sys/kernel/printk` |

The cap is what makes the feature affordable: a record above it is dropped by the
`log` macro for the price of a comparison, so a `warn`/`info` production image
pays nothing for the `debug!`/`trace!` sites compiled into it. In exchange, the
ring cannot be *wider* than the build — widening verbosity means rebuilding, which
is the usual bring-up loop anyway.

Because capture is independent of the console filter, a quiet console no longer
implies a quiet ring: `LOG_LEVEL_DEBUG` plus a console default of `warn` (for
example `quiet` or `loglevel=2` on the command line) prints warnings while
`dmesg` still holds every `debug` record.

## Scope

```
util/klogger/
├── src/
│   └── lib.rs        (build cap, filter, ring buffer, macros, level parsing)
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
        │  (log crate: `level <= log::max_level()`, and max_level == build cap)
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
| `Filter` | the build cap, the console default level, and up to `MAX_FILTER_RULES` `target=level` overrides; longest-prefix match |
| `FILTER` | the global filter (cap + console filter), guarded by `SpinNoIrq` |
| `LogBuf` | fixed 64 KiB byte ring; `push` overwrites the oldest bytes, `read_newest` copies the newest whole records, `clear` drops everything |
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
  a kernel buffer, calls `logbuf_read_newest`, and only then writes to user space.
- `set_filter_spec` is callable at runtime, from the
  `/proc/sys/kernel/log_filter` write path. It replaces the console rule set, and
  the default level when the spec carries one, in a single critical section; it
  never touches the build cap. `set_console_default` is the level-only knob
  behind `/proc/sys/kernel/printk` and leaves the rule set alone.
- `init_klogger(build_level)` runs once on the primary CPU during boot and fixes
  the cap.

## State machine

A spec is validated as a whole and installed as a whole; there are no partial
updates. Its rule set always replaces the previous one; its default level
replaces the previous default only when the spec has a bare level token, so
`ksyscall=trace` retargets one module instead of silently resetting the rest of
the kernel to a hard-coded level:

```
build cap (LOG_LEVEL_*), fixed by init_klogger
    │
    ├── LOGBUF: records everything up to the cap; only `logbuf_clear` removes
    │
    └── console filter { default, rules[] }
            set_filter_spec(spec) ── validated ──► rules := spec rules
                                                 default := spec level, or kept
                                 └─ rejected ──► unchanged
            set_log_level_filter(level) ≡ set the default, drop every override
```

## Algorithms

### Handling one record

1. The `log` crate applies `log::max_level()`, which is the build cap. Records
   above it never reach the logger.
2. `Log::log` formats `<N>[time cpu:tid target:line] args` into a `LineBuf`.
   `<N>` is X-Kernel's own level number (`1..=5`, error..trace).
3. The terminating newline is appended by `LineBuf::terminate`, not by
   `writeln!`: the writer reserves one byte, so a record that fills the buffer is
   truncated to 255 bytes and still ends with `\n`. Truncation also stops at a
   character boundary, so the ring never holds a split code point.
4. The whole line is pushed into `LOGBUF` (overwriting the oldest bytes when
   full). Both the push and a ring read copy at most two contiguous segments with
   `copy_from_slice`, so the time spent inside the IRQ-off guard scales with the
   byte count rather than with a per-byte loop.
5. Only then does `filter_enabled(target, level)` decide whether the record also
   goes to the console. A dropped record still stays in the ring — that is what
   makes a quiet console and a complete log coexist.

### Reading and clearing the ring

`logbuf_read_newest(out)` copies the **newest** bytes that fit, oldest first, and
starts at a record boundary (a partial line left by a ring wrap or by the cut is
dropped). The copy runs in 4 KiB passes, each in its own critical section, so a
full 64 KiB `dmesg` read never holds the ring with interrupts off for the whole
copy; a ring wrap between passes restarts the read (bounded) instead of returning
a mixture of two ring states. That is the shape Linux's `syslog_print_all` produces: with a
destination smaller than the ring, older records are skipped rather than
returning a prefix of the log. Reading never consumes; `logbuf_clear` drops
everything, and `syslog(2)`'s `READ_CLEAR` calls it after a successful copy —
Linux's clear cursor likewise ends up past everything it did not return.

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
4. only after the whole spec validates is it installed in one `FILTER` critical
   section. Any invalid token returns `FilterSpecError` and leaves the previous
   spec untouched.

A level above the build cap is *accepted* (the active spec stays readable) but
cannot produce records; `set_filter_spec` prints a one-line console notice naming
the requested level and the cap, so a bring-up session is not left wondering why
its `trace` request stayed silent.

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
| `init_klogger(build_level)` | `kruntime` fixes the build cap from `LOG_LEVEL_*`, once |
| `build_level_cap()` | reports the cap (diagnostics and specs that ask for more) |
| `set_filter_spec(spec)` | `kruntime` (boot, from the cmdline/DT/Kconfig sources) and `procfs` (`/proc/sys/kernel/log_filter` write) install a spec's rules, plus its default level when it has one; a rejected spec leaves the previous filter untouched |
| `set_console_default(level)` | `procfs`'s `/proc/sys/kernel/printk` write: the console level only, keeping the rule set |
| `set_log_level(spec)` | convenience wrapper that applies a spec and ignores a rejection; used by callers that only have a level string |
| `set_log_level_filter(level)` | set the console default and drop every override (`procfs` tests and internal resets) |
| `filter_spec(buf)` | `procfs` read side and the boot banner report the active spec |
| `current_log_level()` | reports the console filter's **default** level |
| `logbuf_len` / `logbuf_read_newest` / `logbuf_clear` | `ksyscall`'s `syslog(2)` read path and the unit tests |

The two runtime nodes are deliberately separate, so neither has to carry the
other's syntax: `/proc/sys/kernel/printk` keeps its historical single-value shape
(a level number, X-Kernel's domain) and `/proc/sys/kernel/log_filter` carries the
spec text (`warn ksyscall=debug`). A level write does not disturb the module
filter, the way Linux's `console_loglevel` leaves dynamic debug alone.

### Where the boot filter comes from

Precedence is decided by `kruntime`; `klogger` only validates and applies:
`loglevel=` / `quiet` / `debug` on the command line → `/chosen/x-kernel,log-level`
in the device tree → Kconfig `LOG_FILTER` → Kconfig `LOG_LEVEL_*`. An explicit
command-line request therefore overrides the board's default, which is what an
operator changing an argument expects. Every one of them configures
the **console** filter; none can exceed the build cap. The `LOG_LEVEL_*` choice is
applied first, so a rules-only source inherits it as its default rather than the
logger's initial value. Every rejected value is reported and the next source is
tried, so a typo cannot mute the kernel. Those reports go out through `kprintln!`
(the unfiltered console path) instead of `warn!`: the resolver is installing the
very level that would filter the warning, so an `error`- or `off`-level console
would otherwise hide the diagnostic that explains why its configured filter was
ignored.

Whitespace separates command-line parameters, so a full spec must stay inside one
word there: `loglevel=warn,ksyscall=debug`. The comma form works in every source.

## Concurrency model

| Shared state | Synchronization | Notes |
|--------------|-----------------|-------|
| filter `FILTER` | `SpinNoIrq<Filter>` | read on the logging hot path; rule lookup is a bounded scan of a fixed array |
| ring `LOGBUF` | `SpinNoIrq<LogBuf>` | writes and reads are mutually exclusive; one record copies at most 256 bytes |
| console output | `SpinNoIrq<()>` inside `print_fmt` | keeps multi-CPU output from interleaving |
| `log` ceiling | set once by `init_klogger` | equal to the build cap, never written again, so filter and ceiling cannot disagree |

`SpinNoIrq` disables interrupts, so the hot path cannot deadlock against an
interrupt handler on the same lock, but it also means no critical section may
block, allocate, or log again. Two contexts are excluded from the logger
entirely, because `SpinNoIrq` cannot protect against them: records raised in NMI
context are dropped (`LogContextIf::in_nmi`), and a CPU that is already inside
`log()` drops nested records through a per-CPU guard instead of deadlocking on a
console sink that logs. `LineText` formatting happens before the ring lock
is taken, and `print_fmt`'s lock is never held together with `LOGBUF`.

## Design decisions

- **Capture is not derived from the console filter**: the console decides what is
  shown, the build cap decides what can be produced, and the ring stores
  everything the latter allows. This is what makes "quiet console, complete log"
  true instead of aspirational.
- **The cap is immutable at runtime**: widening verbosity is a rebuild. In
  exchange, a record above the cap costs a comparison, not a formatting pass, and
  no runtime write can make a production image pay for `trace` sites.
- **Spec instead of a single level**: `warn ksyscall=debug` retargets one stack
  and can be changed at runtime without rebuilding.
- **Reading returns the newest whole records** (Linux's `syslog_print_all`
  shape): a short destination gets the most recent records, not the oldest ones,
  and never a partial line.
- **Fixed ring and fixed line**: the record path must not allocate, and a runaway
  message must not lose its interesting prefix; a record is truncated at 255
  bytes plus its terminator.
- **X-Kernel level numbering** (`0..=5`, higher is more verbose): Linux's
  `SYSLOG_ACTION_CONSOLE_*` actions are deliberately not implemented because
  `console_loglevel` is the inverted domain and mixing the two would mislead
  callers.

## Drop / resource release

Nothing needs destructors: the filter and the ring are static storage and the
installed logger is `&'static`. The closest operation to a release is
`logbuf_clear`, which resets the indices without wiping the bytes; they disappear
as later records overwrite them.