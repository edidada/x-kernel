# klogger — Security and Reliability Analysis

## Trust model

```
kernel modules (trusted) ──logging macros──────► klogger ──► LogSinkIf (console)
                                                       └──► LOGBUF ──► ksyscall::sys_syslog
boot metadata (boot chain, semi-trusted) ──filter spec──► set_filter_spec
privileged user ──/proc/sys/kernel/printk──► set_filter_spec
privileged user ──syslog(2)──► logbuf_read / logbuf_release
```

`klogger` does not check privileges itself: the console-filter write path is
gated by `procfs` (`cred().is_privileged()`) and the read-back path by `ksyscall`
(`current_cred()`). The module's own security goal is therefore that no input it
is fed can corrupt its state or its memory.

## External boundary / attack surface

| Boundary | Source | Entry point |
|----------|--------|-------------|
| Boot metadata | `/chosen/x-kernel,log-level`, `loglevel=`/`quiet`/`debug`, Kconfig `LOG_FILTER` | `set_filter_spec` (via `kruntime`) |
| Privileged user input | `echo '...' > /proc/sys/kernel/printk` | `set_filter_spec` (via `procfs`) |
| Privileged read-back | `syslog(2)` / `dmesg` | `logbuf_len`, `logbuf_read`, `logbuf_release`, `logbuf_clear`; **the user copy itself happens in `ksyscall`** |
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
   indices with `% LOGBUF_SIZE`; `read` copies at most `len` bytes starting at
   `start`; `release` clamps `read.copied` to `len`, so it can neither underflow
   nor walk past the buffer, and it drops nothing when `read.from` no longer
   matches the ring's sequence position.
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
- `set_filter_spec` installs the filter and updates `log::set_max_level` inside
  one `FILTER` critical section. Two privileged writers racing on
  `/proc/sys/kernel/printk` therefore cannot leave the filter and the ceiling
  describing different specs.
- The `syslog(2)` read path reads under `LOGBUF`, releases the lock, copies to
  user space, and only then releases the returned window. The window carries its
  sequence position, so the release drops exactly the bytes that were delivered
  even though the lock was dropped in between, and a window that wrapped in the
  meantime drops nothing; a faulting destination neither holds the lock nor
  destroys records.
- There is no reentrancy protection: if a `LogSinkIf` provider logs from inside
  `write_str`, the console lock is taken recursively. The provider side
  (`kruntime`) must keep the console write free of logging.

## Threat analysis

| ID | Threat | Severity | Trigger | Mitigation |
|----|--------|----------|---------|------------|
| T-01 | An invalid filter spec destroys logging availability (silent kernel) | Medium | Malformed spec from the device tree, the command line, or a privileged write | The spec is validated as a whole before it is installed; a rejected spec only warns and keeps the previous filter; empty specs are rejected as well |
| T-02 | An override widens the default ceiling and spills hot-path `trace` lines | Low | A `target=trace` rule | The ceiling is the maximum of all rules, so only that target is widened; hot paths such as `kipi` log at `trace` |
| T-03 | A log flood overwrites boot evidence | Medium | More than 64 KiB of records | The ring overwrites the oldest bytes; the read side exposes only the newest tail; no rate limit (see Known limitations) |
| T-04 | Unauthorized reads leak kernel addresses and paths | Medium | An unprivileged `syslog(2)` call | `ksyscall::sys_syslog` checks `is_privileged()` first and returns `OperationNotPermitted` |
| T-05 | A privileged write is not the only path to the level | Medium | Non-privileged writes to `/proc/sys/kernel/printk` | `procfs` requires `cred().is_privileged()` on write; the node is owner-write, world-read |
| T-06 | An over-long record loses its prefix or corrupts line boundaries | Low | A module prints a very long message | Truncation keeps the `<N>` prefix, reserves one byte for the terminator (so the record always ends with `\n`), and stops at a character boundary |
| T-07 | A read starts inside a multi-byte character, so `dmesg` prints invalid UTF-8 | Low | A release boundary lands mid-character | Records themselves are character-aligned, so this needs a reader that releases an arbitrary byte count (a short destination); `dmesg`'s chunked drain is the case, and user-space tools handle it because the kernel never parses the body |
| T-08 | A record body forges an `<n>` prefix and confuses `dmesg` filtering | Low | A module prints a message starting with `<4>` | The prefix is generated by the logger; `dmesg` only parses it at line start |
| T-10 | A forged read window releases records the caller never received | Low | In-kernel code that inflates the byte count of a `LogRead` | `LogRead` has private fields, so a dependent crate can only release the window `logbuf_read` handed it; `release` also no-ops when the window is stale, and the byte count is clamped to the buffered length |
| T-09 | `READ_CLEAR` destroys records the caller never received | Medium | A short or zero `len`, or records appended during the copy | Reads take the oldest unread window and `logbuf_release` drops exactly that window after a successful copy; a window the ring overwrote is stale and drops nothing, records appended during the copy stay buffered, and a copy failure keeps everything |

## Failure mode and effects analysis (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity | Mitigation |
|----|--------------|-------|--------------|---------------|----------|------------|
| F-01 | The boot filter stays at the build default | DT, command line and Kconfig all invalid or missing | Logging follows `LOG_LEVEL_*` | None | 4 | Every invalid source is reported with `warn!` and the resolver falls through to the next one, never to silence |
| F-02 | The ring stays full and old records are lost | Boot log volume above 64 KiB | Only the newest records remain | Missing evidence | 3 | Capacity and capture level are independent knobs; raise the capture level for a full boot log or capture the serial console |
| F-03 | A `syslog(2)` copy to user space fails | Unwritable destination | `EFAULT`, records kept | `dmesg` retries | 3 | The read copies into a kernel buffer first and the window is released only after a successful copy |
| F-04 | The filter lock waits on the logging hot path | Logging from inside a `FILTER` critical section | Interrupts stay off while spinning | Potential hang | 1 | Critical sections only touch fixed-size data; warnings are emitted outside the lock |
| F-05 | The console provider is not ready | `LogSinkIf` not linked, or before `init_klogger` | Records only reach the ring | No serial output, `dmesg` still works | 4 | The `log` macros are no-ops without a logger; `kruntime` links the provider early |
| F-06 | A record exceeds the line budget | A very long formatted message | Truncated to 255 bytes plus `\n` | The tail of the message is lost | 4 | Truncation preserves the prefix, the level tag and the terminator |

## Failure handling

- An invalid filter spec returns `FilterSpecError`; the caller (`kruntime`,
  `procfs`) maps it to a `warn!` or `VfsError::InvalidInput`, and the previous
  filter keeps working.
- `init_klogger` panics if the global logger cannot be installed: that path has
  no error return and no recovery, so it is an explicit, documented exception
  (see the `#[expect]` reason in the code).
- Formatting failures are ignored (`let _ = write!(..)`): logging is best effort,
  and reporting a formatting error through the logger would recurse.

## Privacy analysis

Record bodies may contain kernel addresses, module paths, task names and device
state. They appear only on the kernel console (a physically reachable UART or
display) and through the privileged `syslog(2)` path. The module collects no user
personal data and never writes the ring to persistent storage.

## Known limitations

- There is no log rate limit: a burst of records overwrites the whole older
  content.
- A single record is limited to 255 bytes plus its terminator; `dmesg` shows the
  truncated line.
- `filter_spec` caps the canonical text at `MAX_FILTER_SPEC` (256 bytes) and
  returns `None` when the spec grows past it.
- `SYSLOG_ACTION_CONSOLE_*` is not implemented (the level domain is inverted);
  console verbosity is changed only through `/proc/sys/kernel/printk`.
- The ring is a fixed 64 KiB static buffer with no size configuration.
- Reads return the oldest unread bytes that fit, so a reader with a destination
  smaller than the ring drains it in chunks; `SIZE_BUFFER` lets a reader size its
  buffer to take the whole ring in one call.
- A reader that does not release its window (plain `READ_ALL`, as `dmesg`
  without `-c` uses) sees the same bytes until they are overwritten.

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
      (`target_len`, `len`, `start`, `read.copied`)?
- [ ] Does the record path still leave room for the terminating newline and stay
      on a character boundary?
- [ ] Does a reader release exactly the window it read (`LogRead`), rather than a
      byte count that a concurrent push or wrap could have moved?
- [ ] Are `LogRead`'s fields still private, so no other crate can construct or
      inflate a window?