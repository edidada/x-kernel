# klogger — Security and Reliability Analysis

## Trust model

```
kernel modules (trusted) ──logging macros──────► klogger ──► LogSinkIf (console)
                                                       └──► LOGBUF ──► ksyscall::sys_syslog
Kconfig LOG_LEVEL_* (build time) ──cap──► log::max_level (fixed at boot)
boot metadata (boot chain, semi-trusted) ──console spec──► set_filter_spec
privileged user ──/proc/sys/kernel/printk──► set_console_default
privileged user ──/proc/sys/kernel/log_filter──► set_filter_spec
any user ──syslog(2) READ_ALL / SIZE_BUFFER──► logbuf_read_newest
privileged user ──syslog(2) READ_CLEAR / CLEAR / SIZE_UNREAD──► logbuf_read_newest / logbuf_clear
```

`klogger` does not check privileges itself: the console-filter write path is
gated by `procfs` (`cred().is_privileged()`) and the read-back path by `ksyscall`,
which allows `READ_ALL`/`SIZE_BUFFER` for any caller and requires `euid == 0`
(Linux's `CAP_SYSLOG` stand-in) for the rest. The module's own security goal is
therefore that no input it is fed can corrupt its state or its memory.

## External boundary / attack surface

| Boundary | Source | Entry point |
|----------|--------|-------------|
| Boot metadata | `/chosen/x-kernel,log-level`, `loglevel=`/`quiet`/`debug`, Kconfig `LOG_FILTER` | `set_filter_spec` (via `kruntime`) |
| Build configuration | Kconfig `LOG_LEVEL_*` | `init_klogger`, fixes the console default and the logger floor |
| Privileged user input | `echo 4 > /proc/sys/kernel/printk` (level) | `set_console_default` (via `procfs`) |
| Privileged user input | `echo 'warn ksyscall=debug' > /proc/sys/kernel/log_filter` (spec) | `set_filter_spec` (via `procfs`) |
| Log read-back | `syslog(2)` / `dmesg` — `READ_ALL`/`SIZE_BUFFER` for any caller, the rest privileged | `logbuf_len`, `logbuf_read_newest`, `logbuf_clear`; **the user copy itself happens in `ksyscall`** |
| Record body | formatted arguments of any kernel module | `Log::log` |
| Output channel | `kiface_logging_api::LogSinkIf` / `LogContextIf` providers | `print_fmt` |

The module handles no user pointers, MMIO, DMA, FFI or file-system data, and
contains no `unsafe` block.

## Unsafe inventory

None. All state manipulation is safe Rust: fixed arrays, modular indexing and
`SpinNoIrq` guards.

## Memory-safety invariants

1. `LineBuf.len <= LOGBUF_LINE - 1` for content, plus the terminator byte written
   by `terminate`; `write_str` copies `min(remaining, room)` bytes and never
   splits a character, so `as_bytes()` is always valid UTF-8 that ends with `\n`.
2. `LogBuf.len <= LOGBUF_SIZE` and `start < LOGBUF_SIZE`. `push` normalizes
   indices with `% LOGBUF_SIZE` and keeps `records_len <= LOGBUF_RECORDS`; a
   record is either fully buffered or absent, and the byte count and the record
   index are updated together. `read_newest` copies at most `len` bytes from
   `start` with at most two bounded copies per pass and never past the valid byte
   range; its window comes from the record index, so it starts and ends on
   record boundaries.
3. `FilterRule.target_len <= MAX_FILTER_TARGET`, validated by `set_filter_spec`;
   `level_for` slices `&rule.target[..target_len]` and skips a rule whose bytes
   are not valid UTF-8 instead of panicking.
4. `FilterSpecWriter` copies only when `len + text.len() <= buf.len()` and
   returns `fmt::Error` otherwise, which `filter_spec` turns into `None`.

## Thread safety

- `FILTER`, `LOGBUF` and the `print_fmt` lock are all `SpinNoIrq`, so multi-CPU
  and interrupt-context access is safe; `Filter` and `LogBuf` are plain data.
- The locks are never nested: `Log::log` takes `LOGBUF` briefly, releases it,
  then takes `FILTER` for the console decision, and `print_fmt` takes the console
  lock last. The ordering therefore cannot form a cycle.
- The `log` crate's ceiling is `max(build floor, console ceiling)` and is
  republished inside the same `FILTER` critical section that changes the filter,
  so the installed filter and the ceiling cannot describe different states.
- The `syslog(2)` read path copies into a kernel buffer under `LOGBUF` and does
  the user copy after releasing it, so a faulting destination neither holds the
  lock nor drops records.
- A `LogSinkIf` provider that logs from inside `write_str` does not recurse into
  the logger: the per-CPU guard sees the nested record and drops it. The record
  is lost, which is the intended failure mode for a logging path that cannot make
  progress.
- NMI context is excluded from logging entirely (see T-10); it is documented
  policy, not a per-call check the caller can opt out of.

## Threat analysis

| ID | Threat | Severity | Trigger | Mitigation |
|----|--------|----------|---------|------------|
| T-01 | An invalid filter spec destroys logging availability (silent kernel) | Medium | Malformed spec from the device tree, the command line, a privileged write, or the compiled-in `LOG_FILTER` | The spec is validated as a whole before it is installed and a rejected one keeps the previous filter; every boot source reports its rejection through the unfiltered `kprintln!` path, `procfs` returns `VfsError::InvalidInput`, and empty specs are rejected as well |
| T-02 | A runtime spec raises the ceiling and makes hot-path sites pay | Low | `echo 'trace' > /proc/sys/kernel/log_filter`, or `loglevel=trace` on the command line | Raising is intentional — the sites are compiled in and the operator asked for them — but it is the only way the higher levels cost anything: with no such request the ceiling stays at the build level and those records are dropped by a comparison. The write is privilege-gated on `/proc/sys/kernel/log_filter` |
| T-03 | A log flood overwrites boot evidence | Medium | More than 64 KiB of records | The ring overwrites the oldest bytes and reads return the newest records, so a full ring keeps the recent past; no rate limit (see Known limitations) |
| T-04 | Unauthorized reads leak kernel addresses and paths | Medium | An unprivileged `syslog(2)` call | `READ_ALL`/`SIZE_BUFFER` are open to every caller (Linux's default with `dmesg_restrict` off) so `dmesg` works for users; clearing, read-and-clear and the unread count require `euid == 0`, which stands in for `CAP_SYSLOG` until capability sets exist. A `dmesg_restrict` equivalent is not implemented (known limitation) |
| T-05 | A privileged write is not the only path to the level | Medium | Non-privileged writes to `/proc/sys/kernel/printk` | `procfs` requires `cred().is_privileged()` on write; the node is owner-write, world-read |
| T-06 | An over-long record loses its prefix or corrupts line boundaries | Low | A module prints a very long message | Truncation keeps the `<N>` prefix, reserves one byte for the terminator (so the record always ends with `\n`), and stops at a character boundary |
| T-07 | A read starts inside a record or a multi-byte character, so `dmesg` prints invalid UTF-8 | Low | A ring wrap, a short destination, or a record body containing a newline | The window is chosen from the record index, so it always starts at a record boundary; records are character-aligned, and a destination smaller than the newest record returns nothing instead of a fragment |
| T-08 | A record body forges an `<n>` prefix and confuses `dmesg` filtering | Low | A module prints a message starting with `<4>` | The prefix is generated by the logger; `dmesg` only parses it at line start |
| T-10 | An NMI or a nested record deadlocks on the logger's locks | Medium | A log call from NMI context, or a console sink that logs | Records from NMI context are dropped (`LogContextIf::in_nmi`), and a per-CPU guard drops a nested record on the same CPU instead of taking `SpinNoIrq` twice; emergency output from those contexts uses the platform's atomic console path |
| T-11 | A read holds the ring with interrupts off while copying 64 KiB | Medium | A `dmesg` read with a full-size destination | The copy runs in 4 KiB passes, each in its own critical section, so the IRQ-off window is bounded regardless of the requested length; a wrap between passes restarts the read |
| T-09 | `READ_CLEAR` destroys records the caller never received | Low | A destination smaller than the ring | This matches Linux: `syslog_print_all` skips the older records that do not fit and advances its clear cursor past them, so `dmesg -c` clears what it drained. Callers that want the whole buffer size it from `SIZE_BUFFER`, as `dmesg` does |

## Failure mode and effects analysis (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity | Mitigation |
|----|--------------|-------|--------------|---------------|----------|------------|
| F-01 | The boot filter stays at the build default | DT, command line and Kconfig all invalid or missing | The console follows `LOG_LEVEL_*` | None | 4 | Every invalid source is reported through the unfiltered `kprintln!` path and the resolver falls through to the next one, never to silence |
| F-02 | The ring stays full and old records are lost | Boot log volume above 64 KiB | Only the newest records remain | Missing evidence | 3 | Reads return the newest records, so the recent past survives; a full-size destination (`SIZE_BUFFER`) takes all of it |
| F-03 | A `syslog(2)` copy to user space fails | Unwritable destination | `EFAULT`, records kept | `dmesg` retries | 3 | The read copies into a kernel buffer first and `READ_CLEAR` clears only after a successful copy |
| F-04 | The filter lock waits on the logging hot path | Logging from inside a `FILTER` critical section | Interrupts stay off while spinning | Potential hang | 1 | Critical sections only touch fixed-size data; the one console notice is printed with the lock released |
| F-05 | The console provider is not ready | `LogSinkIf` not linked, or before `init_klogger` | Records only reach the ring once logging is up | No serial output, `dmesg` still works | 4 | The `log` macros are no-ops without a logger; `kruntime` links the provider early |
| F-06 | A record exceeds the line budget | A very long formatted message | Truncated to 255 bytes plus `\n` | The tail of the message is lost | 4 | Truncation preserves the prefix, the level tag and the terminator; the console still prints the full text |
| F-07 | A configuration selects no log level | Kconfig choice left empty | `configured_log_level()` reports `error` | None | 4 | The choice has no `off` option, so an empty selection is a broken configuration: it resolves to `error` at the source, which makes the console default, the logger floor and the boot banner agree instead of leaving the console mute |

## Failure handling

- An invalid filter spec returns `FilterSpecError` and the previous filter keeps
  working. `kruntime`'s boot resolver reports the rejection with `kprintln!`
  (unfiltered, so an `error`/`off` console cannot hide the diagnostic) and falls
  through to the next source; `procfs`'s write path returns
  `VfsError::InvalidInput` to the writer.
- A spec that asks for a level above the build level raises the ceiling and takes
  effect immediately; the cost of the newly admitted sites is paid from then on.
- `init_klogger` panics if the global logger cannot be installed: that path has
  no error return and no recovery, so it is an explicit, documented exception
  (see the `#[expect]` reason in the code).
- Formatting failures are ignored (`let _ = write!(..)`): logging is best effort,
  and reporting a formatting error through the logger would recurse.

## Privacy analysis

Record bodies may contain kernel addresses, module paths, task names and device
state. They appear on the kernel console (a physically reachable UART or display)
and through `syslog(2)`: whole-log reads are open to any caller, like a stock
Linux kernel, and clearing stays privileged. The module collects no user personal
data and never writes the ring to persistent storage.

## Known limitations

- There is no log rate limit: a burst of records overwrites the whole older
  content.
- A single record is limited to 255 bytes plus its terminator; `dmesg` shows the
  truncated line while the console prints the full text.
- The ring keeps at most `LOGBUF_RECORDS` (1024) records. A flood of very short
  records therefore retains less than the full 64 KiB, while every retained
  record stays whole.
- `filter_spec` needs a destination of `MAX_FILTER_SPEC` bytes (5 + 12 x (1 + 24
  + 1 + 5) = 377, derived from the spec grammar) and returns `None` when the
  caller's buffer is smaller, which is why `/proc/sys/kernel/printk` and the boot
  banner allocate exactly that much.
- `SYSLOG_ACTION_CONSOLE_*` is not implemented (the level domain is inverted);
  console verbosity is changed only through `/proc/sys/kernel/printk`.
- The ring is a fixed 64 KiB static buffer with no size configuration.
- Raising verbosity above the build level is possible at runtime and costs what
  the higher-level sites cost; there is no way to *lower* the logger below the
  build level, which is what keeps the ring complete under `quiet`.
- There is no `dmesg_restrict` equivalent, and privilege is `euid == 0` rather
  than `CAP_SYSLOG`.
- `READ_ALL`/`READ_CLEAR` return the newest records that fit, so a destination
  smaller than the ring skips (and, for `READ_CLEAR`, discards) the oldest ones.
  This matches Linux; `SIZE_BUFFER` lets a reader size its buffer for everything.

## Audit checklist

- [ ] Does a new filter entry point reuse `set_filter_spec` instead of writing
      `FILTER` or `log::set_max_level` directly?
- [ ] Does the spec grammar still reject empty specs, over-long targets and too
      many rules, and install nothing on rejection?
- [ ] Does any new log consumer touch user memory or sleep while holding
      `LOGBUF`/`FILTER`?
- [ ] Is a new log site that can fire on a hot path using `trace!`?
- [ ] Is the read path still privilege-gated in `ksyscall`?
- [ ] Are the line-buffer and ring index computations still bounded
      (`target_len`, `len`, `start`, the boundary scan)?
- [ ] Does the record path still leave room for the terminating newline and stay
      on a character boundary?
- [ ] Does a read window still start at a record boundary, so it is whole lines?
- [ ] Does the record path still refuse NMI and same-CPU nested records before
      taking any lock?
- [ ] Does a large read still copy in bounded passes rather than one long
      critical section?