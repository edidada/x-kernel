// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel logging utilities and macros.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate log;

use core::{
    fmt::{self, Write},
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
};

use kspin::{SpinNoIrq, static_lock};
use log::{Level, LevelFilter, Log, Metadata, Record};
pub use log::{debug, error, info, trace, warn};

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {
        $crate::print_fmt(format_args!($($arg)*)).unwrap();
    }
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {
        $crate::print_fmt(format_args!("{}\n", format_args!($($arg)*))).unwrap();
    }
}

macro_rules! color_fmt {
    ($color_code:expr, $($arg:tt)*) => {
        format_args!("\u{1B}[{}m{}\u{1B}[m", $color_code as u8, format_args!($($arg)*))
    };
}

#[repr(u8)]
enum AnsiColor {
    Red    = 31,
    Green  = 32,
    Yellow = 33,
    Cyan   = 36,
    White  = 37,
}

#[repr(u8)]
enum AnsiBrightColor {
    BrightBlack = 90,
}

// ---------------------------------------------------------------------------
// Runtime filter: a default level plus per-target overrides.
//
// One global level is not enough on real hardware: bringing the mmc stack up
// needs `debug` there, while `debug` everywhere drowns the console in IPI and
// syscall lines. A spec is a default level plus `target=level` overrides
// matched by longest path prefix, e.g.
//   `warn ksyscall=debug drv_uart=debug`
// ---------------------------------------------------------------------------

/// Maximum number of `target=level` overrides in a spec.
pub const MAX_FILTER_RULES: usize = 12;
/// Maximum length of one override's target prefix.
pub const MAX_FILTER_TARGET: usize = 24;
/// Longest level name (`error`, `debug`, `trace`, ...).
const MAX_LEVEL_NAME: usize = 5;
/// Longest canonical spec text, i.e. what [`filter_spec`] can produce for any
/// spec [`set_filter_spec`] accepts.
///
/// This must cover the whole accepted domain: a spec that can be installed but
/// not reported back would make `/proc/sys/kernel/printk` unreadable (the node
/// returns `InvalidInput` when the text does not fit) and hide the active
/// overrides from the boot banner. Layout:
///
/// ```text
/// <default> ( [" " <target> "=" <level>]{0,MAX_FILTER_RULES} )
/// ```
pub const MAX_FILTER_SPEC: usize =
    MAX_LEVEL_NAME + MAX_FILTER_RULES * (1 + MAX_FILTER_TARGET + 1 + MAX_LEVEL_NAME);

#[derive(Clone, Copy)]
struct FilterRule {
    target: [u8; MAX_FILTER_TARGET],
    target_len: u8,
    level: LevelFilter,
}

const EMPTY_RULE: FilterRule = FilterRule {
    target: [0; MAX_FILTER_TARGET],
    target_len: 0,
    level: LevelFilter::Off,
};

struct Filter {
    /// Build-time maximum: nothing above it is recorded or shown.
    cap: LevelFilter,
    /// Level used when no rule matches.
    default: LevelFilter,
    rules: [FilterRule; MAX_FILTER_RULES],
    count: usize,
}

impl Filter {
    const fn new() -> Self {
        Self {
            cap: LevelFilter::Error,
            default: LevelFilter::Warn,
            rules: [EMPTY_RULE; MAX_FILTER_RULES],
            count: 0,
        }
    }

    /// Level for `target`, longest matching prefix wins.
    fn level_for(&self, target: &str) -> LevelFilter {
        let mut best: Option<(usize, LevelFilter)> = None;
        for rule in &self.rules[..self.count] {
            let Ok(name) = core::str::from_utf8(&rule.target[..rule.target_len as usize]) else {
                continue;
            };
            if target.starts_with(name) && best.is_none_or(|(len, _)| name.len() >= len) {
                best = Some((name.len(), rule.level));
            }
        }
        best.map_or(self.default, |(_, level)| level)
    }

    /// Highest level this spec asks for, if it exceeds the build cap.
    ///
    /// Only production builds report this: the unit tests use specs above their
    /// build cap on purpose, and a notice per test would only add noise.
    #[cfg(not(unittest))]
    fn above_cap(&self) -> Option<LevelFilter> {
        let highest = self.rules[..self.count]
            .iter()
            .map(|rule| rule.level)
            .fold(self.default, |acc, level| acc.max(level));
        (highest > self.cap).then_some(highest)
    }
}

static_lock! {
    static FILTER: SpinNoIrq<Filter> = SpinNoIrq::new(Filter::new());
}

/// Why a filter spec was rejected. The spec is applied atomically, so a
/// rejected one leaves the previous filter in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterSpecError;

/// Applies a filter spec: `[<level>] [<target>=<level>]...`, levels being
/// `0..=5` or `off/error/warn/info/debug/trace` and targets matched by prefix
/// (a trailing `*` is accepted and redundant). Tokens may be separated by
/// spaces or commas.
///
/// A bare level sets the default level for every target; without one the
/// **current default is kept** and only the rule set is replaced, so
/// `ksyscall=trace` narrows one target instead of silently resetting the rest
/// of the kernel to a hard-coded level.
///
/// The spec is validated before it is applied, so a bad token leaves the
/// current filter untouched.
pub fn set_filter_spec(spec: &str) -> Result<(), FilterSpecError> {
    let mut next = Filter::new();
    let mut default: Option<LevelFilter> = None;
    for token in spec.split(|c: char| c.is_ascii_whitespace() || c == ',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        match token.split_once('=') {
            Some((target, level)) => {
                let target = target.trim().trim_end_matches('*');
                let level = parse_log_level(level).ok_or(FilterSpecError)?;
                if target.is_empty() || target.len() > MAX_FILTER_TARGET {
                    return Err(FilterSpecError);
                }
                if next.count == MAX_FILTER_RULES {
                    return Err(FilterSpecError);
                }
                let mut rule = EMPTY_RULE;
                rule.target[..target.len()].copy_from_slice(target.as_bytes());
                rule.target_len = target.len() as u8;
                rule.level = level;
                next.rules[next.count] = rule;
                next.count += 1;
            }
            None => {
                default = Some(parse_log_level(token).ok_or(FilterSpecError)?);
            }
        }
    }
    if default.is_none() && next.count == 0 {
        return Err(FilterSpecError);
    }
    // Publish the rule set and the inherited default in one critical section.
    let mut filter = FILTER.lock();
    filter.rules = next.rules;
    filter.count = next.count;
    filter.default = default.unwrap_or(filter.default);
    // A spec may ask for a level this image cannot produce: the build cap is
    // fixed at compile time, so the request is accepted (it stays readable) but
    // silently ineffective. Say so — otherwise a bring-up session wonders why
    // its `trace` request produced nothing.
    // Unit tests exercise specs above the cap on purpose, so they stay silent.
    #[cfg(not(unittest))]
    if let Some(highest) = filter.above_cap() {
        crate::kprintln!(
            "log filter asks for {} but this build's cap is {}; rebuild with LOG_LEVEL_{} to \
             enable it",
            level_name(highest),
            level_name(filter.cap),
            level_name(highest).to_ascii_uppercase(),
        );
    }
    Ok(())
}

/// Writes the canonical spec (`warn ksyscall=debug`) into `buf` and returns its
/// length, or `None` when `buf` is too small.
pub fn filter_spec(buf: &mut [u8]) -> Option<usize> {
    let filter = FILTER.lock();
    let mut out = FilterSpecWriter { buf, len: 0 };
    if out.put_str(level_name(filter.default)).is_err() {
        return None;
    }
    for rule in &filter.rules[..filter.count] {
        let name = core::str::from_utf8(&rule.target[..rule.target_len as usize]).ok()?;
        out.put_str(" ").ok()?;
        out.put_str(name).ok()?;
        out.put_str("=").ok()?;
        out.put_str(level_name(rule.level)).ok()?;
    }
    Some(out.len)
}

struct FilterSpecWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl FilterSpecWriter<'_> {
    fn put_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len + text.len();
        if end > self.buf.len() {
            return Err(fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Kernel log ring buffer ("dmesg"): every record that passes the `log`
// ceiling is stored here, whether or not the console prints it. That is what
// makes a quiet console and a complete boot log compatible: the console filter
// decides what to *show*, this buffer keeps what happened, and userland reads
// it back through `syslog(2)` / `dmesg`.
// ---------------------------------------------------------------------------

/// Fixed size of the kernel log buffer.
pub const LOGBUF_SIZE: usize = 64 * 1024;
/// Longest single stored line; longer records are truncated.
const LOGBUF_LINE: usize = 256;

struct LogBuf {
    buf: [u8; LOGBUF_SIZE],
    /// Index of the oldest unread byte.
    start: usize,
    /// Bytes currently unread (saturates at `LOGBUF_SIZE`).
    len: usize,
    /// Sequence position of the oldest unread byte: advanced whenever bytes
    /// leave the buffer, whether a reader released them or the ring overwrote
    /// them.
    unread_from: u64,
    /// Bumped whenever the contents shift under a reader (an overwrite or a
    /// clear). A reader that copies in chunks compares it between chunks and
    /// restarts when it changed, so a window is never a mix of two ring states.
    generation: u64,
}

impl LogBuf {
    const fn new() -> Self {
        Self {
            buf: [0; LOGBUF_SIZE],
            start: 0,
            len: 0,
            unread_from: 0,
            generation: 0,
        }
    }

    /// Appends `line`, overwriting the oldest bytes when the ring is full.
    ///
    /// Both segments are copied with `copy_from_slice`, so the time spent under
    /// the caller's IRQ-off guard depends on the byte count, not on a per-byte
    /// loop with a modulo each iteration.
    fn push(&mut self, line: &[u8]) {
        // A record larger than the ring keeps its newest byte range.
        let line = if line.len() > LOGBUF_SIZE {
            &line[line.len() - LOGBUF_SIZE..]
        } else {
            line
        };
        let free = LOGBUF_SIZE - self.len;
        if line.len() > free {
            // The oldest bytes make room, and leaving the unread window makes
            // any reader cursor over them stale.
            let overwrite = line.len() - free;
            self.start = (self.start + overwrite) % LOGBUF_SIZE;
            self.len -= overwrite;
            self.unread_from += overwrite as u64;
            self.generation = self.generation.wrapping_add(1);
        }
        let index = (self.start + self.len) % LOGBUF_SIZE;
        let first = line.len().min(LOGBUF_SIZE - index);
        self.buf[index..index + first].copy_from_slice(&line[..first]);
        if first < line.len() {
            let rest = line.len() - first;
            self.buf[..rest].copy_from_slice(&line[first..]);
        }
        self.len += line.len();
    }

    /// Byte at `offset` from the oldest buffered byte.
    fn at(&self, offset: usize) -> u8 {
        self.buf[(self.start + offset) % LOGBUF_SIZE]
    }

    /// Whether the byte at `offset` starts one of our records (`<N>`).
    fn starts_record_at(&self, offset: usize) -> bool {
        if self.len - offset < 3 {
            // Too little left to tell; treat it as a start rather than dropping it.
            return true;
        }
        self.at(offset) == b'<'
            && self.at(offset + 1).is_ascii_digit()
            && self.at(offset + 2) == b'>'
    }

    /// The window a read of `size` bytes returns: `(skip, take, generation)`.
    fn newest_window(&self, size: usize) -> (usize, usize, u64) {
        let mut take = size.min(self.len);
        if take == 0 {
            return (0, 0, self.generation);
        }
        let mut skip = self.len - take;

        if !self.starts_record_at(skip) {
            // Records are at most `LOGBUF_LINE` bytes, so this scan is short.
            if let Some(trimmed) = (0..take)
                .find(|offset| self.at(skip + offset) == b'\n')
                .map(|offset| offset + 1)
                && trimmed < take
            {
                skip += trimmed;
                take -= trimmed;
            }
        }
        (skip, take, self.generation)
    }

    /// Copies up to `limit` bytes of a window into `out`, starting at `done`.
    ///
    /// Keeps each pass over the ring short so the caller's IRQ-off guard is held
    /// for a bounded time even when `dmesg` asks for the whole 64 KiB ring.
    fn copy_window_chunk(
        &self,
        skip: usize,
        take: usize,
        done: usize,
        out: &mut [u8],
        limit: usize,
    ) -> usize {
        let remaining = take - done;
        let chunk = remaining.min(limit).min(out.len() - done);
        if chunk == 0 {
            return 0;
        }
        let begin = (self.start + skip + done) % LOGBUF_SIZE;
        let head = chunk.min(LOGBUF_SIZE - begin);
        out[done..done + head].copy_from_slice(&self.buf[begin..begin + head]);
        if head < chunk {
            out[done + head..done + chunk].copy_from_slice(&self.buf[..chunk - head]);
        }
        chunk
    }

    fn clear(&mut self) {
        self.unread_from += self.len as u64;
        self.start = 0;
        self.len = 0;
        self.generation = self.generation.wrapping_add(1);
    }
}

static_lock! {
    static LOGBUF: SpinNoIrq<LogBuf> = SpinNoIrq::new(LogBuf::new());
}

/// Bytes currently held in the kernel log buffer.
pub fn logbuf_len() -> usize {
    LOGBUF.lock().len
}

/// Largest slice copied while `LOGBUF` is held.
///
/// A `dmesg` read asks for the whole 64 KiB ring; copying that in one pass would
/// keep interrupts and preemption off for several microseconds and make every
/// other CPU's `log()` spin on the ring. Chunking bounds it to a few hundred
/// nanoseconds per pass.
const READ_CHUNK: usize = 4 * 1024;
/// How often a read restarts when the ring overwrites its window mid-copy.
const READ_RETRIES: usize = 3;

/// Copies the newest bytes of the kernel log buffer that fit into `out`, oldest
/// first, and returns how many bytes were written.
///
/// The window starts at a record boundary, so callers get whole lines even when
/// `out` is smaller than the ring. Reading never consumes: only
/// [`logbuf_clear`] removes records.
///
/// The copy runs in [`READ_CHUNK`]-sized passes, each in its own critical
/// section. If the ring wraps onto the window between passes the read restarts
/// (up to [`READ_RETRIES`] times) rather than returning a mixture of two ring
/// states; if the writer keeps winning, the bytes copied so far are trimmed to
/// the last complete record.
pub fn logbuf_read_newest(out: &mut [u8]) -> usize {
    for _ in 0..READ_RETRIES {
        let (skip, take, generation) = LOGBUF.lock().newest_window(out.len());
        if take == 0 {
            return 0;
        }
        let mut done = 0;
        let mut consistent = true;
        while done < take {
            let guard = LOGBUF.lock();
            if guard.generation != generation {
                consistent = false;
                break;
            }
            let copied = guard.copy_window_chunk(skip, take, done, out, READ_CHUNK);
            done += copied;
            if copied == 0 {
                break;
            }
        }
        if consistent && done == take {
            return done;
        }
        if !consistent {
            continue;
        }
        // No progress (the buffer shrank underneath): keep what is a whole record.
        return trim_to_record_end(out, done);
    }
    // The ring kept wrapping: return the consistent prefix we managed to copy.
    trim_to_record_end(out, out.len())
}

/// Shrinks `len` to just after the last newline, so a truncated read still ends
/// on a record boundary.
fn trim_to_record_end(out: &[u8], len: usize) -> usize {
    let len = len.min(out.len());
    out[..len]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1)
}

/// Drops everything in the kernel log buffer.
pub fn logbuf_clear() {
    LOGBUF.lock().clear();
}

/// Fixed-capacity line writer: logging must not allocate, and a runaway
/// message must not lose the interesting prefix.
struct LineBuf {
    buf: [u8; LOGBUF_LINE],
    len: usize,
}

impl LineBuf {
    const fn new() -> Self {
        Self {
            buf: [0; LOGBUF_LINE],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Appends the record's terminating newline.
    ///
    /// Kept separate from the formatting path so the terminator cannot be
    /// truncated away: `write_str` reserves the byte, this writes it.
    fn terminate(&mut self) {
        self.buf[self.len] = b'\n';
        self.len += 1;
    }
}

impl fmt::Write for LineBuf {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        // One byte is reserved for the record's terminating newline: the caller
        // formats the body with `write!` and then appends the terminator through
        // `LineBuf::terminate`, so a record that filled the buffer would
        // otherwise lose it and run into the next record.
        let room = self.buf.len() - 1 - self.len;
        let mut take = text.len().min(room);
        // Copy whole characters only; a split code point would put invalid
        // UTF-8 into the ring and into every `dmesg` reader.
        while take > 0 && !text.is_char_boundary(take) {
            take -= 1;
        }
        self.buf[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

/// Maximum number of CPUs the recursion guard tracks.
///
/// The guard only needs to catch a CPU re-entering the logger, which happens
/// with a console sink that logs or with a fault taken inside the logger. A CPU
/// id beyond this bound simply skips the guard rather than growing state.
const MAX_GUARD_CPUS: usize = 64;

/// Per-CPU "this CPU is inside the logger" flags.
static LOGGING: [AtomicBool; MAX_GUARD_CPUS] = [const { AtomicBool::new(false) }; MAX_GUARD_CPUS];

/// Marks the current CPU as inside the logger for as long as it lives.
struct RecursionGuard {
    slot: Option<&'static AtomicBool>,
}

impl RecursionGuard {
    /// Returns `None` when this CPU is already inside the logger.
    fn enter() -> Option<Self> {
        let slot = kiface_logging_api::LogContextIf::cpu_id()
            .and_then(|cpu| LOGGING.get(cpu.min(MAX_GUARD_CPUS - 1)));
        match slot {
            Some(slot) => slot
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .ok()
                .map(|_| Self { slot: Some(slot) }),
            // No CPU identity yet (very early boot): skip the guard.
            None => Some(Self { slot: None }),
        }
    }
}

impl Drop for RecursionGuard {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            slot.store(false, Ordering::Release);
        }
    }
}

struct KernelLogger;

impl Write for KernelLogger {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        cfg_if::cfg_if! {
            if #[cfg(feature = "std")] {
                std::print!("{s}");
            } else {
                kiface_logging_api::LogSinkIf::write_str(s);
            }
        }
        Ok(())
    }
}

#[inline]
fn get_level_color(level: Level) -> u8 {
    match level {
        Level::Error => AnsiColor::Red as u8,
        Level::Warn => AnsiColor::Yellow as u8,
        Level::Info => AnsiColor::Green as u8,
        Level::Debug => AnsiColor::Cyan as u8,
        Level::Trace => AnsiBrightColor::BrightBlack as u8,
    }
}

impl Log for KernelLogger {
    #[inline]
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn flush(&self) {}

    fn log(&self, record: &Record) {
        // `enabled` is true for every level (the level is compiled in); the
        // real decision is here, where the per-target filter is known.
        if !self.enabled(record.metadata()) {
            return;
        }

        // Two contexts must never reach the locks: an NMI, which ordinary
        // interrupt masking cannot exclude, and a nested record from the same
        // CPU (a console sink that logs, or a fault inside the logger). Both
        // would deadlock on `SpinNoIrq` instead of reporting anything, so they
        // are dropped. NMI/panic paths that need output use the platform's
        // atomic console path instead.
        if kiface_logging_api::LogContextIf::in_nmi() {
            return;
        }
        let Some(_guard) = RecursionGuard::enter() else {
            return;
        };

        // Capture first, print second: the buffer is the record of what
        // happened, the console filter only decides what is shown. The macros
        // already dropped everything above the build cap, so every record that
        // reaches this point is stored — the ring is never narrowed by the
        // console filter. The `<level>` prefix is X-Kernel's own numbering
        // (`1..=5`, error..trace), so `dmesg | grep '^<1>'` selects one level.
        let mut line = LineBuf::new();
        let level = level_filter_of(record.level());
        // The trailing newline is part of the record: the console path adds its
        // own, and without it every buffered record runs into the next one, so
        // `dmesg` prints the whole log as a single line. It is appended after
        // formatting because a record that fills the line buffer would otherwise
        // lose it to truncation.
        let _ = write!(line, "<{}>{}", level_number(level), LineText(record));
        line.terminate();
        LOGBUF.lock().push(line.as_bytes());

        if !filter_enabled(record.target(), record.level()) {
            return;
        }
        print_record(record);
    }
}

/// Prints one record to the console, with the per-level colour and the
/// `[time cpu:tid target:line]` prefix.
///
/// Kept out of [`Log::log`] so the record path (guards, ring capture, filter)
/// stays readable on its own.
fn print_record(record: &Record) {
    let line = record.line().unwrap_or(0);
    let path = record.target();
    let compact = path == "unittest";
    let color = get_level_color(record.level());

    cfg_if::cfg_if! {
        if #[cfg(feature = "std")] {
            let _ = if compact {
                print_fmt(color_fmt!(
                    AnsiColor::White,
                    "[{time}] {args}\n",
                    time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.6f"),
                    args = color_fmt!(color, "{}", record.args()),
                ))
            } else {
                print_fmt(color_fmt!(
                    AnsiColor::White,
                    "[{time} {path}:{line}] {args}\n",
                    time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.6f"),
                    path = path,
                    line = line,
                    args = color_fmt!(color, "{}", record.args()),
                ))
            };
        } else {
            let cpu_id = kiface_logging_api::LogContextIf::cpu_id();
            let tid = kiface_logging_api::LogContextIf::task_id();
            let now = kiface_logging_api::LogContextIf::now();

            // Logging is best effort; reporting its format error via the logger would recurse.
            let _ = match (compact, cpu_id, tid) {
                (true, Some(c), Some(t)) => print_fmt(color_fmt!(
                    AnsiColor::White,
                    "[{:>3}.{:06} {c}:{t}] {args}\n",
                    now.as_secs(),
                    now.subsec_micros(),
                    c = c,
                    t = t,
                    args = color_fmt!(color, "{}", record.args()),
                )),
                (true, Some(c), None) => print_fmt(color_fmt!(
                    AnsiColor::White,
                    "[{:>3}.{:06} {c}] {args}\n",
                    now.as_secs(),
                    now.subsec_micros(),
                    c = c,
                    args = color_fmt!(color, "{}", record.args()),
                )),
                (true, _, _) => print_fmt(color_fmt!(
                    AnsiColor::White,
                    "[{:>3}.{:06}] {args}\n",
                    now.as_secs(),
                    now.subsec_micros(),
                    args = color_fmt!(color, "{}", record.args()),
                )),
                (false, Some(c), Some(t)) => print_fmt(color_fmt!(
                    AnsiColor::White,
                    "[{:>3}.{:06} {c}:{t} {path}:{line}] {args}\n",
                    now.as_secs(),
                    now.subsec_micros(),
                    c = c,
                    t = t,
                    path = path,
                    line = line,
                    args = color_fmt!(color, "{}", record.args()),
                )),
                (false, Some(c), None) => print_fmt(color_fmt!(
                    AnsiColor::White,
                    "[{:>3}.{:06} {c} {path}:{line}] {args}\n",
                    now.as_secs(),
                    now.subsec_micros(),
                    c = c,
                    path = path,
                    line = line,
                    args = color_fmt!(color, "{}", record.args()),
                )),
                (false, _, _) => print_fmt(color_fmt!(
                    AnsiColor::White,
                    "[{:>3}.{:06} {path}:{line}] {args}\n",
                    now.as_secs(),
                    now.subsec_micros(),
                    path = path,
                    line = line,
                    args = color_fmt!(color, "{}", record.args()),
                )),
            };
        }
    }
}

/// The `[time cpu:tid target:line]` prefix, in the same shape the console
/// uses (without the per-level colour).
struct LineText<'a>(&'a Record<'a>);

impl fmt::Display for LineText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let record = self.0;
        let path = record.target();
        let line = record.line().unwrap_or(0);
        cfg_if::cfg_if! {
            if #[cfg(feature = "std")] {
                write!(f, "[{} {}:{}] {}", chrono::Local::now().format("%H:%M:%S%.6f"), path, line, record.args())
            } else {
                let now = kiface_logging_api::LogContextIf::now();
                match (
                    kiface_logging_api::LogContextIf::cpu_id(),
                    kiface_logging_api::LogContextIf::task_id(),
                ) {
                    (Some(c), Some(t)) => write!(
                        f,
                        "[{:>3}.{:06} {c}:{t} {path}:{line}] {}",
                        now.as_secs(),
                        now.subsec_micros(),
                        record.args()
                    ),
                    (Some(c), None) => write!(
                        f,
                        "[{:>3}.{:06} {c} {path}:{line}] {}",
                        now.as_secs(),
                        now.subsec_micros(),
                        record.args()
                    ),
                    _ => write!(
                        f,
                        "[{:>3}.{:06} {path}:{line}] {}",
                        now.as_secs(),
                        now.subsec_micros(),
                        record.args()
                    ),
                }
            }
        }
    }
}

/// Writes already formatted text to the console output path.
///
/// This is the primitive behind [`kprint!`] and [`kprintln!`]: it serializes
/// console writes with an IRQ-safe lock, but bypasses the per-target filter and
/// the kernel log buffer (`kprint!` output is not visible to `dmesg`).
///
/// May be called in any execution context.
pub fn print_fmt(args: fmt::Arguments) -> fmt::Result {
    static_lock! {
        static LOCK: SpinNoIrq<()> = SpinNoIrq::new(());
    }

    let _guard = LOCK.lock();
    KernelLogger.write_fmt(args)
}

/// Installs the global `log` logger and fixes the build-time maximum level.
///
/// `build_level` is the compiled-in `LOG_LEVEL_*` choice, parsed here because
/// this crate owns the level vocabulary. It becomes the **cap**: the `log`
/// crate's own filter, so a record above it is dropped before the logger runs
/// and costs a comparison, and no runtime spec can raise it — widening verbosity
/// means rebuilding.
///
/// A configuration that selects no level (`configured_log_level()` reports
/// `off`) is mapped to `error` rather than silencing the kernel: the Kconfig
/// choice has no `off` option, so an empty selection is a broken configuration,
/// and losing only the warnings is a safer fallback than losing everything.
///
/// Call once from primary-CPU boot after the `kiface_logging_api` providers are
/// linked; `kruntime` then applies the resolved boot filter spec through
/// [`set_log_level`]. Records emitted before this call are dropped by the `log`
/// crate, which is why early boot uses [`kprintln!`] instead of the macros.
pub fn init_klogger(build_level: &str) {
    let cap = parse_log_level(build_level)
        .filter(|level| *level != LevelFilter::Off)
        .unwrap_or(LevelFilter::Error);
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: failure to install the global logger \
                  during primary boot is unrecoverable; this initialization entry has no error \
                  return or recovery caller"
    )]
    log::set_logger(&KernelLogger).unwrap();
    FILTER.lock().cap = cap;
    log::set_max_level(cap);
}

/// The build-time maximum level (`LOG_LEVEL_*`), fixed for this image.
///
/// A filter spec may ask for a higher level; it is accepted and reported, but no
/// record above the cap is ever produced, so the request has no effect until the
/// kernel is rebuilt.
pub fn build_level_cap() -> LevelFilter {
    FILTER.lock().cap
}

/// Whether a record at `level` from `target` should reach the console.
fn filter_enabled(target: &str, level: Level) -> bool {
    let filter = FILTER.lock();
    let allowed = filter.level_for(target);
    allowed >= level_filter_of(level)
}

const fn level_filter_of(level: Level) -> LevelFilter {
    match level {
        Level::Error => LevelFilter::Error,
        Level::Warn => LevelFilter::Warn,
        Level::Info => LevelFilter::Info,
        Level::Debug => LevelFilter::Debug,
        Level::Trace => LevelFilter::Trace,
    }
}

/// Returns the default level of the active filter (no per-target override
/// applied).
pub fn current_log_level() -> LevelFilter {
    FILTER.lock().default
}

/// Sets the console default level at runtime, keeping every per-target override.
///
/// This is the knob behind `/proc/sys/kernel/printk`, which only ever carried the
/// level: writing it must not silently drop the module filter that
/// `/proc/sys/kernel/log_filter` owns.
pub fn set_console_default(level: LevelFilter) {
    FILTER.lock().default = level;
}

/// Sets the console default level at runtime and drops every per-target
/// override.
///
/// Takes effect immediately for the console; the ring keeps recording everything
/// up to the build cap, and a level above that cap cannot be produced until the
/// kernel is rebuilt.
pub fn set_log_level_filter(level: LevelFilter) {
    let mut filter = FILTER.lock();
    filter.default = level;
    filter.count = 0;
}

/// Parses a log level from user input, accepting either a level name
/// (`off`/`error`/`warn`/`info`/`debug`/`trace`, ASCII case-insensitive) or
/// a number in X-Kernel's own level order (`0..=5`, higher = more verbose):
///
/// | number | resolved level                |
/// |--------|-------------------------------|
/// | `0`    | [`LevelFilter::Off`]          |
/// | `1`    | [`LevelFilter::Error`]        |
/// | `2`    | [`LevelFilter::Warn`]         |
/// | `3`    | [`LevelFilter::Info`]         |
/// | `4`    | [`LevelFilter::Debug`]        |
/// | `5`    | [`LevelFilter::Trace`]        |
///
/// Values outside `0..=5` are rejected. Whitespace around the input is
/// ignored. Returns `None` for anything else.
pub fn parse_log_level(input: &str) -> Option<LevelFilter> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Ok(number) = input.parse::<u8>() {
        return match number {
            0 => Some(LevelFilter::Off),
            1 => Some(LevelFilter::Error),
            2 => Some(LevelFilter::Warn),
            3 => Some(LevelFilter::Info),
            4 => Some(LevelFilter::Debug),
            5 => Some(LevelFilter::Trace),
            _ => None,
        };
    }
    LevelFilter::from_str(input).ok()
}

/// Maps a level filter back to its canonical number in X-Kernel's own level
/// order (the inverse of [`parse_log_level`]):
/// `off`→`0`, `error`→`1`, `warn`→`2`, `info`→`3`, `debug`→`4`, `trace`→`5`.
pub fn level_number(level: LevelFilter) -> u8 {
    match level {
        LevelFilter::Off => 0,
        LevelFilter::Error => 1,
        LevelFilter::Warn => 2,
        LevelFilter::Info => 3,
        LevelFilter::Debug => 4,
        LevelFilter::Trace => 5,
    }
}

/// Returns the lowercase display name of a level filter
/// (`off`/`error`/`warn`/`info`/`debug`/`trace`).
pub fn level_name(level: LevelFilter) -> &'static str {
    match level {
        LevelFilter::Off => "off",
        LevelFilter::Error => "error",
        LevelFilter::Warn => "warn",
        LevelFilter::Info => "info",
        LevelFilter::Debug => "debug",
        LevelFilter::Trace => "trace",
    }
}

/// Applies a filter spec (see [`set_filter_spec`]); a bare level name/number
/// behaves like [`set_log_level_filter`]. Invalid input leaves the current
/// filter unchanged.
pub fn set_log_level(spec: &str) {
    let _ = set_filter_spec(spec.trim());
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
