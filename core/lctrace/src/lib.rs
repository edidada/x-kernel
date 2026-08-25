// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LCTrace — low-overhead kernel tracing.
//!
//! # What it is
//!
//! Each CPU owns a fixed-size ring of fixed-size binary records. Emitting one
//! costs a relaxed load, a per-CPU pointer read, one uncontended atomic
//! increment, a counter read and a handful of relaxed stores. Nothing locks,
//! nothing allocates and nothing is formatted until a snapshot is rendered.
//! When the buffer wraps, the oldest records are simply overwritten, so the
//! kernel always carries the recent past with it into a crash.
//!
//! # Enabling
//!
//! LCTrace is a debug facility and is off by default. It is compiled in only
//! when `KFEAT_LCTRACE=y`; otherwise every probe site expands to nothing.
//!
//! # Consuming
//!
//! - `/proc/lctrace` renders a snapshot on read (read-only, no control knobs).
//! - The panic handler dumps every ring to the console after the backtrace.

#![no_std]

use core::{
    fmt,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

mod counter;
mod event;
mod render;

pub use event::LcTraceEvent;

/// Number of records retained per CPU.
///
/// Must be a power of two so the ring index is a mask rather than a division.
const ENTRIES_PER_CPU: usize = kbuild_config::LCTRACE_ENTRIES_PER_CPU;

const ENTRY_MASK: u64 = (ENTRIES_PER_CPU - 1) as u64;

const _: () = assert!(
    ENTRIES_PER_CPU.is_power_of_two(),
    "LCTRACE_ENTRIES_PER_CPU must be a power of two"
);

/// Number of `u64` arguments carried by every record.
pub const ARGS_PER_RECORD: usize = 4;

/// Cache line size the records are padded to.
const CACHE_LINE_BYTES: usize = 64;

/// One trace record.
///
/// Every field is an atomic so that a reader rendering a snapshot concurrently
/// with a writer is a well-defined race rather than undefined behaviour. All
/// accesses are relaxed except the `seq` publication, so the generated code is
/// the same as for plain loads and stores.
#[repr(C, align(64))]
struct LcTraceRecord {
    /// Reservation sequence plus one, published last. Zero means "never
    /// written", which is also the initial state of the whole ring.
    seq: AtomicU64,
    timestamp_ticks: AtomicU64,
    args: [AtomicU64; ARGS_PER_RECORD],
    event_id: AtomicU32,
}

impl LcTraceRecord {
    const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            timestamp_ticks: AtomicU64::new(0),
            args: [const { AtomicU64::new(0) }; ARGS_PER_RECORD],
            event_id: AtomicU32::new(0),
        }
    }
}

const _: () = assert!(
    core::mem::size_of::<LcTraceRecord>() == CACHE_LINE_BYTES,
    "a record must occupy exactly one cache line"
);

/// One CPU's ring.
///
/// `head` counts reservations and never wraps in practice; the ring index is
/// `head & ENTRY_MASK`. `repr(C)` plus the records' 64-byte alignment puts the
/// first record on the cache line after `head`, so a writer bumping `head`
/// never dirties a record line it is not about to write anyway.
#[repr(C, align(64))]
struct CpuRing {
    head: AtomicU64,
    records: [LcTraceRecord; ENTRIES_PER_CPU],
}

impl CpuRing {
    const fn new() -> Self {
        Self {
            head: AtomicU64::new(0),
            records: [const { LcTraceRecord::new() }; ENTRIES_PER_CPU],
        }
    }
}

/// The rings live in `.bss` rather than in the per-CPU section.
///
/// The `.percpu` output section is `AT(_percpu_start)` and is not `NOLOAD`
/// (see `linker.lds.S`), so a per-CPU array of this size would be replicated
/// `NR_CPUS` times inside the kernel image on disk. Keeping the rings in
/// `.bss` and storing only a pointer per CPU costs nothing in image size.
static RINGS: [CpuRing; kbuild_config::NR_CPUS] =
    [const { CpuRing::new() }; kbuild_config::NR_CPUS];

/// Pointer to the calling CPU's ring, or zero before [`init_this_cpu`].
#[percpu::def_percpu]
static CURRENT_RING_PTR: usize = 0;

/// Set while a snapshot is being rendered.
///
/// Rendering runs in ordinary kernel context and would otherwise trip the
/// scheduler, IRQ and syscall probes, overwriting the very history it is
/// copying out. Suppressing recording for the duration is what keeps a
/// snapshot a picture of the past rather than of itself.
static IS_SNAPSHOTTING: AtomicBool = AtomicBool::new(false);

/// Binds the calling CPU to its ring.
///
/// Must be called once per CPU, on that CPU, after the per-CPU areas are up
/// and before any probe on it can fire. Probes are inert until it runs.
///
/// `cpu_id` beyond `NR_CPUS` leaves this CPU untraced rather than aliasing
/// another CPU's ring.
pub fn init_this_cpu(cpu_id: usize) {
    let Some(ring) = RINGS.get(cpu_id) else {
        return;
    };
    let ring_addr = core::ptr::from_ref(ring) as usize;
    // SAFETY: this runs during bring-up of `cpu_id` itself, on that CPU, so
    // the current per-CPU area is the one being initialized and no other
    // context on this CPU can observe a half-written pointer.
    unsafe { CURRENT_RING_PTR.write_current_raw(ring_addr) };
}

/// Records one event on the calling CPU.
///
/// Prefer the [`event!`] macro, which names the event and pads the arguments.
///
/// Safe to call from any context, including IRQ and pseudo-NMI context: the
/// slot is reserved with a single atomic increment, so two contexts on the
/// same CPU can never write the same slot. Records emitted before the CPU has
/// run [`init_this_cpu`], or while a snapshot is being rendered, are dropped.
#[inline]
pub fn record(event: LcTraceEvent, args: [u64; ARGS_PER_RECORD]) {
    if IS_SNAPSHOTTING.load(Ordering::Relaxed) {
        return;
    }

    // SAFETY: the pointer is per-CPU and, once installed, constant for the
    // lifetime of the kernel. Reading it without disabling preemption may
    // observe either the previous or the new CPU's ring across a migration;
    // both are valid rings and both are written race-free, so no guard is
    // needed on this path.
    let ring_addr = unsafe { CURRENT_RING_PTR.read_current_raw() };
    if ring_addr == 0 {
        return;
    }
    // SAFETY: `init_this_cpu` only ever stores the address of an element of
    // the `'static` `RINGS` array, so a non-zero value is a valid, live,
    // permanently mapped `CpuRing`.
    let ring = unsafe { &*(ring_addr as *const CpuRing) };

    let seq = ring.head.fetch_add(1, Ordering::Relaxed);
    let slot = &ring.records[(seq & ENTRY_MASK) as usize];

    slot.timestamp_ticks
        .store(counter::read_counter(), Ordering::Relaxed);
    slot.event_id.store(event as u32, Ordering::Relaxed);
    for (cell, value) in slot.args.iter().zip(args) {
        cell.store(value, Ordering::Relaxed);
    }
    // Published last: a reader that sees `seq + 1` has seen every field above.
    slot.seq.store(seq + 1, Ordering::Release);
}

/// Records one event, naming it and padding the arguments to four.
///
/// ```ignore
/// lctrace::event!(SchedSwitch, prev_tid, next_tid, prev_state);
/// lctrace::event!(Debug0, some_flag, ptr as usize);
/// ```
///
/// Arguments may be any integer or `bool` (see [`TraceArg`]); pointers should
/// be cast to `usize` at the call site.
#[macro_export]
macro_rules! event {
    ($event:ident $(,)?) => {
        $crate::record($crate::LcTraceEvent::$event, [0, 0, 0, 0])
    };
    ($event:ident, $arg0:expr $(,)?) => {
        $crate::record(
            $crate::LcTraceEvent::$event,
            [$crate::TraceArg::into_trace_arg($arg0), 0, 0, 0],
        )
    };
    ($event:ident, $arg0:expr, $arg1:expr $(,)?) => {
        $crate::record(
            $crate::LcTraceEvent::$event,
            [
                $crate::TraceArg::into_trace_arg($arg0),
                $crate::TraceArg::into_trace_arg($arg1),
                0,
                0,
            ],
        )
    };
    ($event:ident, $arg0:expr, $arg1:expr, $arg2:expr $(,)?) => {
        $crate::record(
            $crate::LcTraceEvent::$event,
            [
                $crate::TraceArg::into_trace_arg($arg0),
                $crate::TraceArg::into_trace_arg($arg1),
                $crate::TraceArg::into_trace_arg($arg2),
                0,
            ],
        )
    };
    ($event:ident, $arg0:expr, $arg1:expr, $arg2:expr, $arg3:expr $(,)?) => {
        $crate::record(
            $crate::LcTraceEvent::$event,
            [
                $crate::TraceArg::into_trace_arg($arg0),
                $crate::TraceArg::into_trace_arg($arg1),
                $crate::TraceArg::into_trace_arg($arg2),
                $crate::TraceArg::into_trace_arg($arg3),
            ],
        )
    };
}

/// A value that can be stored in a record argument slot.
///
/// Implemented for the integer and boolean types probes actually carry, so
/// call sites need no `as u64` casts. Signed values are sign-extended, which
/// keeps small negative values (such as `-EINVAL`) recognisable in a dump.
pub trait TraceArg {
    /// Converts the value into its record representation.
    fn into_trace_arg(self) -> u64;
}

macro_rules! impl_trace_arg {
    ($($ty:ty),* $(,)?) => {
        $(
            impl TraceArg for $ty {
                #[inline(always)]
                fn into_trace_arg(self) -> u64 {
                    self as u64
                }
            }
        )*
    };
}

impl_trace_arg!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, bool);

/// Renders a snapshot of every CPU's ring into `out`.
///
/// Rings are rendered one CPU at a time, oldest record first, matching the
/// layout a reader scans backwards from a crash.
///
/// `counter_hz` is the frequency of the platform counter used for timestamps.
/// It is injected by the caller because LCTrace deliberately does not depend
/// on the timer subsystem; pass `khal::time::freq()`. Zero suppresses the
/// microsecond column.
///
/// Recording is suppressed on every CPU while this runs. Concurrent calls do
/// not interleave: the second one reports that a snapshot is already running
/// and returns.
pub fn write_snapshot(out: &mut dyn fmt::Write, counter_hz: u64) -> fmt::Result {
    if IS_SNAPSHOTTING.swap(true, Ordering::Acquire) {
        return writeln!(out, "LCTrace: a snapshot is already in progress");
    }
    let result = render::render_all(out, counter_hz);
    IS_SNAPSHOTTING.store(false, Ordering::Release);
    result
}

/// Renders a snapshot to the kernel console, one line at a time.
///
/// Intended for the panic path, where no allocator may be usable. Output is
/// line-buffered so that a line is emitted with a single console acquisition.
pub fn dump_to_console(counter_hz: u64) {
    let mut writer = render::ConsoleWriter::new();
    let _ = write_snapshot(&mut writer, counter_hz);
    writer.flush();
}
