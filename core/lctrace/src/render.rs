// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Snapshot rendering.
//!
//! Everything here runs off the hot path with recording suppressed, so it may
//! be as slow and as wordy as it likes. It must still avoid allocating: the
//! panic path renders through [`ConsoleWriter`] at a point where the allocator
//! may be unusable.

use core::{fmt, sync::atomic::Ordering};

use linux_sysno::Sysno;

use crate::{ARGS_PER_RECORD, ENTRIES_PER_CPU, ENTRY_MASK, LcTraceEvent, RINGS, event};

/// One decoded record, detached from the ring it came from.
struct DecodedRecord {
    seq: u64,
    timestamp_ticks: u64,
    event_id: u32,
    args: [u64; ARGS_PER_RECORD],
}

/// Reads the record reserved for `seq`, or `None` if it is not readable.
///
/// A slot is skipped when it was never written, when it has already been
/// recycled for a later reservation, or when a writer recycled it midway
/// through this read. The `seq` field is written last with `Release`, so
/// seeing the expected value means every other field of that record is
/// visible; re-reading it afterwards catches a concurrent overwrite.
fn decode(ring: &crate::CpuRing, seq: u64) -> Option<DecodedRecord> {
    let slot = &ring.records[(seq & ENTRY_MASK) as usize];
    let expected = seq + 1;

    if slot.seq.load(Ordering::Acquire) != expected {
        return None;
    }

    let timestamp_ticks = slot.timestamp_ticks.load(Ordering::Relaxed);
    let event_id = slot.event_id.load(Ordering::Relaxed);
    let mut args = [0u64; ARGS_PER_RECORD];
    for (slot_arg, arg) in slot.args.iter().zip(args.iter_mut()) {
        *arg = slot_arg.load(Ordering::Relaxed);
    }

    if slot.seq.load(Ordering::Relaxed) != expected {
        return None;
    }

    Some(DecodedRecord {
        seq,
        timestamp_ticks,
        event_id,
        args,
    })
}

/// Renders the syscall name for the events that carry a syscall number.
///
/// Returns an empty string for every other event so the column simply
/// disappears rather than printing a misleading name.
fn syscall_suffix(event_id: u32, args: &[u64; ARGS_PER_RECORD]) -> &'static str {
    let is_syscall_event = event_id == LcTraceEvent::SyscallEntry as u32
        || event_id == LcTraceEvent::SyscallExit as u32;
    if !is_syscall_event {
        return "";
    }
    match usize::try_from(args[0]).ok().and_then(Sysno::new) {
        Some(sysno) => sysno.name(),
        None => "<unknown syscall>",
    }
}

/// Converts a tick delta to whole microseconds.
///
/// `counter_hz` of zero means the caller could not supply a frequency, in
/// which case only the raw tick delta is meaningful.
fn ticks_to_micros(delta_ticks: u64, counter_hz: u64) -> u64 {
    if counter_hz == 0 {
        return 0;
    }
    let micros = u128::from(delta_ticks) * 1_000_000 / u128::from(counter_hz);
    u64::try_from(micros).unwrap_or(u64::MAX)
}

/// Renders every CPU's ring, one block per CPU, oldest record first.
pub(crate) fn render_all(out: &mut dyn fmt::Write, counter_hz: u64) -> fmt::Result {
    writeln!(
        out,
        "LCTrace snapshot: {} records/cpu, {} cpus, counter {} Hz",
        ENTRIES_PER_CPU,
        kbuild_config::NR_CPUS,
        counter_hz
    )?;

    for (cpu_id, ring) in RINGS.iter().enumerate() {
        let head = ring.head.load(Ordering::Acquire);
        if head == 0 {
            writeln!(out, "\ncpu{cpu_id}: no records")?;
            continue;
        }

        let oldest = head.saturating_sub(ENTRIES_PER_CPU as u64);
        writeln!(
            out,
            "\ncpu{}: {} records emitted, showing seq {}..{}",
            cpu_id,
            head,
            oldest,
            head - 1
        )?;
        writeln!(
            out,
            "{:>10} {:>16} {:>12} {:>10} {:<14} {:>16} {:>16} {:>16} {:>16}",
            "seq", "ticks", "+ticks", "+us", "event", "arg0", "arg1", "arg2", "arg3"
        )?;

        let mut base_ticks = None;
        for seq in oldest..head {
            let Some(record) = decode(ring, seq) else {
                continue;
            };
            let base = *base_ticks.get_or_insert(record.timestamp_ticks);
            let delta_ticks = record.timestamp_ticks.wrapping_sub(base);

            let name = event::event_name_of(record.event_id).unwrap_or("<unknown event>");
            let suffix = syscall_suffix(record.event_id, &record.args);

            writeln!(
                out,
                "{:>10} {:016x} {:>+12} {:>10} {:<14} {:016x} {:016x} {:016x} {:016x}{}{}",
                record.seq,
                record.timestamp_ticks,
                delta_ticks,
                ticks_to_micros(delta_ticks, counter_hz),
                name,
                record.args[0],
                record.args[1],
                record.args[2],
                record.args[3],
                if suffix.is_empty() { "" } else { " : " },
                suffix
            )?;
        }
    }

    Ok(())
}

/// Maximum rendered line length before [`ConsoleWriter`] forces a flush.
const CONSOLE_LINE_CAPACITY: usize = 256;

/// A [`fmt::Write`] sink that emits whole lines to the kernel console.
///
/// `klogger::print_fmt` takes the console lock on every call, and rendering a
/// single record issues many small writes. Buffering to the newline keeps a
/// rendered line atomic with respect to other console writers and keeps the
/// lock traffic proportional to lines rather than to format fragments.
pub(crate) struct ConsoleWriter {
    line: [u8; CONSOLE_LINE_CAPACITY],
    len: usize,
    /// Set once a line had to be split, so the dump says so exactly once.
    has_truncated: bool,
}

impl ConsoleWriter {
    pub(crate) const fn new() -> Self {
        Self {
            line: [0; CONSOLE_LINE_CAPACITY],
            len: 0,
            has_truncated: false,
        }
    }

    /// Emits whatever is buffered, even without a trailing newline.
    pub(crate) fn flush(&mut self) {
        if self.len != 0 {
            self.emit_line();
        }
        if self.has_truncated {
            let _ = klogger::print_fmt(format_args!(
                "LCTrace: one or more lines exceeded {CONSOLE_LINE_CAPACITY} bytes and were \
                 split\n"
            ));
        }
    }

    fn emit_line(&mut self) {
        let line =
            core::str::from_utf8(&self.line[..self.len]).unwrap_or("<lctrace: invalid line>");
        // `print_fmt` may fail if the console is not up; the panic path has no
        // better place to report that, so the result is deliberately dropped.
        let _ = klogger::print_fmt(format_args!("{line}\n"));
        self.len = 0;
    }
}

impl fmt::Write for ConsoleWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.as_bytes() {
            if *byte == b'\n' {
                self.emit_line();
                continue;
            }
            if self.len == CONSOLE_LINE_CAPACITY {
                self.has_truncated = true;
                self.emit_line();
            }
            self.line[self.len] = *byte;
            self.len += 1;
        }
        Ok(())
    }
}
