// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network poller counters for budget adaptation and CPU occupancy.

use core::sync::atomic::{AtomicU64, Ordering};

use ktime_types::TimeSpan;

use super::budget::{BudgetKind, FeedDelta, elapsed_us};

#[percpu::def_percpu]
static CPU_ROUNDS: AtomicU64 = AtomicU64::new(0);

#[percpu::def_percpu]
static CPU_BUSY_US: AtomicU64 = AtomicU64::new(0);

static RX_GROWS: AtomicU64 = AtomicU64::new(0);
static TX_GROWS: AtomicU64 = AtomicU64::new(0);
static TIMER_GROWS: AtomicU64 = AtomicU64::new(0);
static RX_SHRINKS: AtomicU64 = AtomicU64::new(0);
static TX_SHRINKS: AtomicU64 = AtomicU64::new(0);
static TIMER_SHRINKS: AtomicU64 = AtomicU64::new(0);
static RX_OVERRUNS: AtomicU64 = AtomicU64::new(0);
static TX_OVERRUNS: AtomicU64 = AtomicU64::new(0);
static TIMER_OVERRUNS: AtomicU64 = AtomicU64::new(0);
static TOTAL_ROUNDS: AtomicU64 = AtomicU64::new(0);
static RX_PACKETS: AtomicU64 = AtomicU64::new(0);
static TX_PACKETS: AtomicU64 = AtomicU64::new(0);
static TIMER_EVENTS: AtomicU64 = AtomicU64::new(0);
/// Reschedules driven by remaining immediate work after a round.
static BACKLOG_RESCHEDULES: AtomicU64 = AtomicU64::new(0);
/// Reschedules driven by notifications observed while Running.
static NOTIFY_RESCHEDULES: AtomicU64 = AtomicU64::new(0);

/// Point-in-time poller statistics snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PollerStatsSnapshot {
    pub rx_grows: u64,
    pub tx_grows: u64,
    pub timer_grows: u64,
    pub rx_shrinks: u64,
    pub tx_shrinks: u64,
    pub timer_shrinks: u64,
    pub rx_overruns: u64,
    pub tx_overruns: u64,
    pub timer_overruns: u64,
    pub total_rounds: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub timer_events: u64,
    /// Follow-up rounds scheduled because `has_more` remained true.
    pub backlog_reschedules: u64,
    /// Follow-up rounds scheduled because work arrived during `Running`.
    pub notify_reschedules: u64,
    /// Completed poll rounds observed on each configured CPU slot.
    ///
    /// Counts may land on a neighboring CPU if the task migrates across the
    /// unguarded `with_current` window; treat as an occupancy approximation.
    pub per_cpu_rounds: [u64; kbuild_config::NR_CPUS],
    /// Accumulated busy time in microseconds on each configured CPU slot.
    pub per_cpu_busy_us: [u64; kbuild_config::NR_CPUS],
}

pub(crate) fn record_round(
    rx_packets: usize,
    tx_packets: usize,
    timer_events: usize,
    elapsed: TimeSpan,
    overrun: Option<BudgetKind>,
    feed: FeedDelta,
) {
    TOTAL_ROUNDS.fetch_add(1, Ordering::Relaxed);
    RX_PACKETS.fetch_add(rx_packets as u64, Ordering::Relaxed);
    TX_PACKETS.fetch_add(tx_packets as u64, Ordering::Relaxed);
    TIMER_EVENTS.fetch_add(timer_events as u64, Ordering::Relaxed);

    if feed.rx_grows != 0 {
        RX_GROWS.fetch_add(feed.rx_grows, Ordering::Relaxed);
    }
    if feed.tx_grows != 0 {
        TX_GROWS.fetch_add(feed.tx_grows, Ordering::Relaxed);
    }
    if feed.timer_grows != 0 {
        TIMER_GROWS.fetch_add(feed.timer_grows, Ordering::Relaxed);
    }
    if feed.rx_shrinks != 0 {
        RX_SHRINKS.fetch_add(feed.rx_shrinks, Ordering::Relaxed);
    }
    if feed.tx_shrinks != 0 {
        TX_SHRINKS.fetch_add(feed.tx_shrinks, Ordering::Relaxed);
    }
    if feed.timer_shrinks != 0 {
        TIMER_SHRINKS.fetch_add(feed.timer_shrinks, Ordering::Relaxed);
    }

    match overrun {
        Some(BudgetKind::Rx) => {
            RX_OVERRUNS.fetch_add(1, Ordering::Relaxed);
        }
        Some(BudgetKind::Tx) => {
            TX_OVERRUNS.fetch_add(1, Ordering::Relaxed);
        }
        Some(BudgetKind::Timer) => {
            TIMER_OVERRUNS.fetch_add(1, Ordering::Relaxed);
        }
        None => {}
    }

    let busy_us = elapsed_us(elapsed);
    // Approximate occupancy: without percpu preempt guards, a migrating task
    // may credit the neighboring CPU across this short window.
    CPU_ROUNDS.with_current(|rounds| {
        rounds.fetch_add(1, Ordering::Relaxed);
    });
    CPU_BUSY_US.with_current(|busy| {
        busy.fetch_add(busy_us, Ordering::Relaxed);
    });
}

pub(crate) fn record_backlog_reschedule() {
    BACKLOG_RESCHEDULES.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_notify_reschedule() {
    NOTIFY_RESCHEDULES.fetch_add(1, Ordering::Relaxed);
}

/// Returns a relaxed snapshot of poller adaptation and occupancy counters.
pub fn poller_stats_snapshot() -> PollerStatsSnapshot {
    let mut per_cpu_rounds = [0u64; kbuild_config::NR_CPUS];
    let mut per_cpu_busy_us = [0u64; kbuild_config::NR_CPUS];
    for cpu in 0..kbuild_config::NR_CPUS {
        // SAFETY: `cpu` is within the compile-time NR_CPUS range used to size
        // the per-CPU section, and the values are plain atomics.
        let rounds = unsafe { CPU_ROUNDS.remote_ref_raw(cpu) };
        // SAFETY: same NR_CPUS bound as above; the slot holds an AtomicU64.
        let busy = unsafe { CPU_BUSY_US.remote_ref_raw(cpu) };
        per_cpu_rounds[cpu] = rounds.load(Ordering::Relaxed);
        per_cpu_busy_us[cpu] = busy.load(Ordering::Relaxed);
    }

    PollerStatsSnapshot {
        rx_grows: RX_GROWS.load(Ordering::Relaxed),
        tx_grows: TX_GROWS.load(Ordering::Relaxed),
        timer_grows: TIMER_GROWS.load(Ordering::Relaxed),
        rx_shrinks: RX_SHRINKS.load(Ordering::Relaxed),
        tx_shrinks: TX_SHRINKS.load(Ordering::Relaxed),
        timer_shrinks: TIMER_SHRINKS.load(Ordering::Relaxed),
        rx_overruns: RX_OVERRUNS.load(Ordering::Relaxed),
        tx_overruns: TX_OVERRUNS.load(Ordering::Relaxed),
        timer_overruns: TIMER_OVERRUNS.load(Ordering::Relaxed),
        total_rounds: TOTAL_ROUNDS.load(Ordering::Relaxed),
        rx_packets: RX_PACKETS.load(Ordering::Relaxed),
        tx_packets: TX_PACKETS.load(Ordering::Relaxed),
        timer_events: TIMER_EVENTS.load(Ordering::Relaxed),
        backlog_reschedules: BACKLOG_RESCHEDULES.load(Ordering::Relaxed),
        notify_reschedules: NOTIFY_RESCHEDULES.load(Ordering::Relaxed),
        per_cpu_rounds,
        per_cpu_busy_us,
    }
}
