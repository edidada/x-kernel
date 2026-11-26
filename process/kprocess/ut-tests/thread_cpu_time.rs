// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{PrevCputime, TaskCputime, ThreadCpuTime, ThreadGroupCputimer};

#[def_test]
fn test_tick_counters_split_by_interrupted_context() {
    let time = ThreadCpuTime::new();
    assert_eq!(time.raw(), (0, 0));

    time.account_tick(true, 1000);
    assert_eq!(time.raw(), (1000, 0));

    time.account_tick(false, 1000);
    time.account_tick(false, 1000);
    assert_eq!(time.raw(), (1000, 2000));
}

#[def_test]
fn test_raw_reads_never_settle() {
    let time = ThreadCpuTime::new();
    time.account_tick(true, 40);
    time.account_tick(false, 60);

    // Repeated remote reads observe the committed counters unchanged; only the
    // owning CPU's next tick moves them.
    assert_eq!(time.raw(), (40, 60));
    assert_eq!(time.raw(), (40, 60));
    time.account_tick(true, 40);
    assert_eq!(time.raw(), (80, 60));
}

#[def_test]
fn test_adjust_scales_split_to_runtime() {
    let prev = PrevCputime::new();
    // stime = 10 * 90 / (10 + 20) = 30, utime = 90 - 30 = 60.
    assert_eq!(prev.adjust(20, 10, 90), (60, 30));

    // With no ticks in one mode, all runtime is charged to the other.
    let prev = PrevCputime::new();
    assert_eq!(prev.adjust(30, 0, 100), (100, 0));
    let prev = PrevCputime::new();
    assert_eq!(prev.adjust(0, 30, 100), (0, 100));

    // No runtime and no ticks: the split is zero.
    let prev = PrevCputime::new();
    assert_eq!(prev.adjust(0, 0, 0), (0, 0));
}

#[def_test]
fn test_adjust_monotonic_under_ratio_shift() {
    let prev = PrevCputime::new();
    assert_eq!(prev.adjust(80, 20, 100), (80, 20));

    // Kernel-heavy shift: both counters advance with the runtime.
    assert_eq!(prev.adjust(80, 120, 200), (80, 120));

    // User-heavy shift: stime clamps to its previous value, utime absorbs the
    // rest of the runtime.
    assert_eq!(prev.adjust(190, 10, 210), (90, 120));

    // Back to a balanced split at a larger runtime.
    assert_eq!(prev.adjust(280, 120, 400), (280, 120));

    // stime scales up but utime would regress: utime clamps and stime is
    // re-derived so the sum still equals the runtime.
    assert_eq!(prev.adjust(280, 200, 410), (280, 130));

    // A non-advancing or reordered runtime keeps the last published sample.
    assert_eq!(prev.adjust(280, 200, 410), (280, 130));
    assert_eq!(prev.adjust(280, 200, 400), (280, 130));

    // A constant ratio scales linearly with the runtime.
    let prev = PrevCputime::new();
    assert_eq!(prev.adjust(10, 10, 100), (50, 50));
    assert_eq!(prev.adjust(10, 10, 200), (100, 100));
}

#[def_test]
fn test_adjust_sum_equals_runtime_without_overflow() {
    let prev = PrevCputime::new();
    let half = u64::MAX / 2;
    let (utime, stime) = prev.adjust(half, half, u64::MAX);
    assert_eq!(utime.saturating_add(stime), u64::MAX);
    assert!(utime > 0 && stime > 0);
}

#[def_test]
fn test_group_cputimer_starts_zero() {
    let timer = ThreadGroupCputimer::new();
    assert_eq!(timer.sample(), TaskCputime::default());
}

#[def_test]
fn test_group_cputimer_feeds_are_per_field() {
    let timer = ThreadGroupCputimer::new();
    // Each charge touches only its own clock, so a PROF fastpath check (which
    // sums user+system) and a SCHED check (exec runtime) stay independent.
    timer.add_user_time(1000);
    timer.add_system_time(500);
    timer.add_exec_runtime(750);
    timer.add_user_time(1000);

    let sample = timer.sample();
    assert_eq!(sample.utime, 2000);
    assert_eq!(sample.stime, 500);
    assert_eq!(sample.sum_exec_runtime, 750);
}

#[def_test]
fn test_group_cputimer_seed_raises_only() {
    // Seed from an authoritative walk, then advance one clock past it, then
    // re-seed from a walk that is lower on that clock but higher on another:
    // seed_from only ever raises each field to the max, never lowers a clock
    // a concurrent tick already advanced past the walked total.
    let timer = ThreadGroupCputimer::new();
    timer.seed_from(TaskCputime {
        utime: 100,
        stime: 50,
        sum_exec_runtime: 150,
    });
    assert_eq!(timer.sample().utime, 100);

    // A tick advances user past the seed.
    timer.add_user_time(80);
    assert_eq!(timer.sample().utime, 180);

    // Re-seed with a walk that trails the advanced user clock but leads on the
    // others: user stays at the higher 180, system/runtime rise.
    timer.seed_from(TaskCputime {
        utime: 120,
        stime: 90,
        sum_exec_runtime: 400,
    });
    let sample = timer.sample();
    assert_eq!(sample.utime, 180);
    assert_eq!(sample.stime, 90);
    assert_eq!(sample.sum_exec_runtime, 400);
}
