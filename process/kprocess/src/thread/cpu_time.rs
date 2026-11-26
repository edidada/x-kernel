// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Thread CPU-time accounting: raw tick counters and a monotonic adjustment.
//!
//! The owning CPU adds one `TICK_NSEC` to the raw user or system counter on
//! each periodic tick, classified by the interrupted context. A remote reader
//! never settles another thread: it loads the committed counters and scales
//! them against scheduler runtime. There is no wall-clock interval, published
//! mode or final-freeze state.

use core::sync::atomic::{AtomicU64, Ordering};

use kspin::SpinNoPreempt;
/// The shared raw sample carrier, accumulated across dead totals and live
/// members under the group's consistency domain; CPU-timer expiry consumes it
/// raw, [`PrevCputime::adjust`] scales it for query-time views.
pub(crate) use ktime_types::TaskCputime;

/// Raw per-thread user/system counters.
///
/// Single writer: the owning CPU's periodic tick. Many readers: a query on
/// another CPU loads the committed values without settling. The two counters
/// are independent, so they are atomic rather than sharing a lock; a consistent
/// `(utime, stime, runtime)` snapshot is assembled by the sampler, not here.
pub(crate) struct ThreadCpuTime {
    utime_ns: AtomicU64,
    stime_ns: AtomicU64,
}

impl ThreadCpuTime {
    /// Creates zeroed counters; the first periodic tick establishes them.
    pub(crate) const fn new() -> Self {
        Self {
            utime_ns: AtomicU64::new(0),
            stime_ns: AtomicU64::new(0),
        }
    }

    /// Charges one accounting tick to the interrupted context's counter.
    ///
    /// Owning-CPU only, from the periodic tick with IRQs masked. `user_tick` is
    /// the interrupted frame's `user_mode()`; `tick_ns` is `TICK_NSEC`. The
    /// relaxed add is published to a user-return consumer by the tick's later
    /// `Release` store on `cpu_timer_work_pending`.
    pub(crate) fn account_tick(&self, user_tick: bool, tick_ns: u64) {
        let counter = if user_tick {
            &self.utime_ns
        } else {
            &self.stime_ns
        };
        counter.fetch_add(tick_ns, Ordering::Relaxed);
    }

    /// Reads the committed raw counters.
    ///
    /// Lock-free and settlement-free: safe to sample another thread from any
    /// context that may read it.
    pub(crate) fn raw(&self) -> (u64, u64) {
        (
            self.utime_ns.load(Ordering::Relaxed),
            self.stime_ns.load(Ordering::Relaxed),
        )
    }
}

/// Lock-free running group CPU-timer accumulator.
///
/// Matches Linux `thread_group_cputimer.cputime_atomic`: a group-wide
/// `{utime, stime, sum_exec_runtime}` triple advanced by every member's tick
/// (user/system) and scheduler runtime commit (exec), gated on the armed flag
/// the way `get_running_cputimer()` returns `NULL` when `timers_active` is
/// false. The periodic tick cannot afford a membership walk (`Process::
/// thread_group_cputime` holds the membership lock and is task-context only),
/// so it samples this accumulator instead to decide whether a CPU timer may
/// have expired; the authoritative user-return poll still walks the group and
/// lifts this value back up with [`Self::seed_from`].
///
/// The three counters are independent atomics advanced with relaxed adds — a
/// non-uniform snapshot is acceptable because the fastpath is a heuristic that
/// can only defer expiry to a later tick, never fabricate one. It is seeded on
/// every `false -> true` armed transition (absolute `TIMER_ABSTIME` deadlines
/// need a clock synced to the walked group total at start) and the seed uses
/// [`Self::seed_from`] so a concurrent feed cannot move it backwards.
pub(crate) struct ThreadGroupCputimer {
    utime_ns: AtomicU64,
    stime_ns: AtomicU64,
    sum_exec_runtime_ns: AtomicU64,
}

impl ThreadGroupCputimer {
    /// Creates a zeroed accumulator; the armed gate decides when it is fed.
    pub(crate) const fn new() -> Self {
        Self {
            utime_ns: AtomicU64::new(0),
            stime_ns: AtomicU64::new(0),
            sum_exec_runtime_ns: AtomicU64::new(0),
        }
    }

    /// Charges one user tick to the group accumulator (owner-CPU tick, IRQs off).
    pub(crate) fn add_user_time(&self, delta_ns: u64) {
        self.utime_ns.fetch_add(delta_ns, Ordering::Relaxed);
    }

    /// Charges one system tick to the group accumulator (owner-CPU tick, IRQs off).
    pub(crate) fn add_system_time(&self, delta_ns: u64) {
        self.stime_ns.fetch_add(delta_ns, Ordering::Relaxed);
    }

    /// Charges committed scheduler runtime to the group accumulator.
    pub(crate) fn add_exec_runtime(&self, delta_ns: u64) {
        self.sum_exec_runtime_ns
            .fetch_add(delta_ns, Ordering::Relaxed);
    }

    /// Lock-free triple read for the tick fastpath and group queries.
    pub(crate) fn sample(&self) -> TaskCputime {
        TaskCputime {
            utime: self.utime_ns.load(Ordering::Relaxed),
            stime: self.stime_ns.load(Ordering::Relaxed),
            sum_exec_runtime: self.sum_exec_runtime_ns.load(Ordering::Relaxed),
        }
    }

    /// Raises each field to at least `total` (compare-exchange-max).
    ///
    /// Seeds the accumulator on the `false -> true` armed transition from an
    /// authoritative [`TaskCputime`] group walk, applied before the gate is
    /// published. Taking the max — never an add or a plain store — is what makes
    /// this safe against a concurrent tick: the seed is a lower bound, ticks only
    /// add, and a feed that races the seed's load is preserved by the retry.
    /// Mirrors Linux `update_gt_cputime`.
    ///
    /// [`Process::thread_group_cputime`]: crate::Process::thread_group_cputime
    pub(crate) fn seed_from(&self, total: TaskCputime) {
        raise(&self.utime_ns, total.utime);
        raise(&self.stime_ns, total.stime);
        raise(&self.sum_exec_runtime_ns, total.sum_exec_runtime);
    }
}

/// Raise `counter` to at least `target`, retrying while another writer advances it.
fn raise(counter: &AtomicU64, target: u64) {
    let mut current = counter.load(Ordering::Relaxed);
    while current < target {
        match counter.compare_exchange_weak(current, target, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

/// Monotonic adjustment state, owned by both a thread and a thread group.
///
/// Its lock is independent of the membership domain: `adjust` runs after the
/// raw sample is released and serializes concurrent samplers so the published
/// split never goes backwards. Only task-context queries take it — the tick
/// writes the raw atomics, never this state — so it disables preemption to
/// avoid a same-CPU spin deadlock but does not mask IRQs.
pub(crate) struct PrevCputime {
    inner: SpinNoPreempt<PrevCputimeState>,
}

#[derive(Default)]
struct PrevCputimeState {
    utime: u64,
    stime: u64,
}

impl PrevCputime {
    /// Creates an empty adjustment history.
    pub(crate) const fn new() -> Self {
        Self {
            inner: SpinNoPreempt::new(PrevCputimeState { utime: 0, stime: 0 }),
        }
    }

    /// Scales the raw split against `rtime`.
    ///
    /// Guarantees `utime + stime == rtime` and `stime_i+1 >= stime_i`,
    /// `utime_i+1 >= utime_i`, assuming `rtime` is monotonic. The raw tick split
    /// has only tick-level precision; scaling it against the scheduler's exact
    /// runtime recovers the total while the previous sample keeps it monotonic.
    pub(crate) fn adjust(&self, utime: u64, stime: u64, rtime: u64) -> (u64, u64) {
        let mut prev = self.inner.lock();
        // Either rtime is not monotonic (a bug) or this caller was reordered by
        // the lock. Filter both so the rest assumes a monotonic rtime: keep the
        // last published sample.
        if prev.stime.saturating_add(prev.utime) >= rtime {
            return (prev.utime, prev.stime);
        }

        // With no ticks in one mode, assume all runtime is the other. Once ticks
        // arrive the monotonic clamp below converges to the observed ratio.
        let mut stime = if stime == 0 {
            // No system ticks: all runtime is user time.
            0
        } else if utime == 0 {
            // No user ticks: all runtime is system time.
            rtime
        } else {
            // stime * rtime / (stime + utime), widened to avoid overflow.
            (stime as u128 * rtime as u128 / (stime as u128 + utime as u128)) as u64
        };

        // Keep stime monotonic; utime = rtime - stime then inherits monotonicity
        // from rtime.
        if stime < prev.stime {
            stime = prev.stime;
        }
        let mut utime = rtime - stime;
        // Keep utime monotonic too, re-deriving stime so the sum stays rtime.
        if utime < prev.utime {
            utime = prev.utime;
            stime = rtime - utime;
        }

        prev.stime = stime;
        prev.utime = utime;
        (prev.utime, prev.stime)
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/thread_cpu_time.rs");
