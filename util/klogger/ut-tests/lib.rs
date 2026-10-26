// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::string::ToString;

use log::LevelFilter;
use unittest::{assert_eq, def_test};

use super::{
    current_log_level, filter_spec, level_name, level_number, logbuf_clear, logbuf_len,
    logbuf_snapshot, parse_log_level, set_filter_spec, set_log_level, set_log_level_filter,
};

#[def_test]
fn parse_accepts_level_names_case_insensitively() {
    assert_eq!(parse_log_level("off"), Some(LevelFilter::Off));
    assert_eq!(parse_log_level("ERROR"), Some(LevelFilter::Error));
    assert_eq!(parse_log_level("Warn"), Some(LevelFilter::Warn));
    assert_eq!(parse_log_level("info"), Some(LevelFilter::Info));
    assert_eq!(parse_log_level("Debug"), Some(LevelFilter::Debug));
    assert_eq!(parse_log_level("trace"), Some(LevelFilter::Trace));
    assert_eq!(parse_log_level("  trace\n"), Some(LevelFilter::Trace));
}

#[def_test]
fn parse_maps_our_own_level_numbers() {
    assert_eq!(parse_log_level("0"), Some(LevelFilter::Off));
    assert_eq!(parse_log_level("1"), Some(LevelFilter::Error));
    assert_eq!(parse_log_level("2"), Some(LevelFilter::Warn));
    assert_eq!(parse_log_level("3"), Some(LevelFilter::Info));
    assert_eq!(parse_log_level("4"), Some(LevelFilter::Debug));
    assert_eq!(parse_log_level("5"), Some(LevelFilter::Trace));
    assert_eq!(parse_log_level("4\n"), Some(LevelFilter::Debug));
}

#[def_test]
fn parse_rejects_unknown_and_out_of_range_input() {
    assert_eq!(parse_log_level(""), None);
    assert_eq!(parse_log_level("6"), None);
    assert_eq!(parse_log_level("9"), None);
    assert_eq!(parse_log_level("banana"), None);
}

#[def_test]
fn level_number_round_trips_canonical_values() {
    for (level, number) in [
        (LevelFilter::Off, 0),
        (LevelFilter::Error, 1),
        (LevelFilter::Warn, 2),
        (LevelFilter::Info, 3),
        (LevelFilter::Debug, 4),
        (LevelFilter::Trace, 5),
    ] {
        assert_eq!(level_number(level), number);
        assert_eq!(parse_log_level(&number.to_string()), Some(level));
    }
}

#[def_test]
fn level_name_matches_acceptable_input() {
    for (level, name) in [
        (LevelFilter::Off, "off"),
        (LevelFilter::Error, "error"),
        (LevelFilter::Warn, "warn"),
        (LevelFilter::Info, "info"),
        (LevelFilter::Debug, "debug"),
        (LevelFilter::Trace, "trace"),
    ] {
        assert_eq!(level_name(level), name);
        assert_eq!(parse_log_level(name), Some(level));
    }
}

/// `serial`: the level filter and the `log` ceiling are process-global, so a
/// test that asserts on them must not share them with a parallel one.
#[def_test(serial)]
fn set_log_level_takes_effect_immediately() {
    let previous = current_log_level();
    set_log_level("3");
    assert_eq!(current_log_level(), LevelFilter::Info);
    set_log_level_filter(LevelFilter::Debug);
    assert_eq!(current_log_level(), LevelFilter::Debug);
    // Invalid input must not silence the whole kernel by accident.
    set_log_level("invalid");
    assert_eq!(current_log_level(), LevelFilter::Debug);
    set_log_level_filter(previous);
}

#[def_test(serial)]
fn filter_spec_reports_targets_and_longest_prefix_wins() {
    let previous = current_log_level();
    assert_eq!(set_filter_spec("warn drv_uart=debug"), Ok(()));
    // The default stays visible to anything that reports "the level".
    assert_eq!(current_log_level(), LevelFilter::Warn);

    let mut buf = [0u8; super::MAX_FILTER_SPEC];
    let len = filter_spec(&mut buf).expect("spec fits");
    assert_eq!(core::str::from_utf8(&buf[..len]), Ok("warn drv_uart=debug"));

    assert_eq!(set_filter_spec("warn drv_uart=trace drv_uart::dw_uart=error"), Ok(()));
    let mut buf = [0u8; super::MAX_FILTER_SPEC];
    let len = filter_spec(&mut buf).expect("spec fits");
    assert_eq!(
        core::str::from_utf8(&buf[..len]),
        Ok("warn drv_uart=trace drv_uart::dw_uart=error")
    );

    set_log_level_filter(previous);
}

#[def_test(serial)]
fn filter_spec_rejects_bad_input_without_half_applying() {
    let previous = current_log_level();
    assert_eq!(set_filter_spec("info subsys_uart=debug"), Ok(()));
    // A bad token anywhere must leave the previous spec intact.
    assert_eq!(set_filter_spec("debug drv_uart=nonsense").is_err(), true);
    assert_eq!(set_filter_spec("debug =4").is_err(), true);
    assert_eq!(set_filter_spec("debug too_long_target_name_aaaaaaaaaaaaaaaa=4").is_err(), true);
    assert_eq!(set_filter_spec("   ").is_err(), true);
    assert_eq!(current_log_level(), LevelFilter::Info);
    let mut buf = [0u8; super::MAX_FILTER_SPEC];
    let len = filter_spec(&mut buf).expect("spec fits");
    assert_eq!(
        core::str::from_utf8(&buf[..len]),
        Ok("info subsys_uart=debug")
    );
    set_log_level_filter(previous);
}

/// A record the console filter drops still lands in the ring: that is the
/// "quiet console, complete log" contract `dmesg` depends on.
///
/// `serial`: the ring, the filter and the `log` ceiling are one process-global
/// resource. The assertions below are scoped to this test's own unique markers
/// because other kernel threads keep logging concurrently.
#[def_test(serial)]
fn logbuf_captures_records_the_console_filter_hides() {
    let mut previous_buf = [0u8; super::MAX_FILTER_SPEC];
    let previous_len = filter_spec(&mut previous_buf).unwrap_or(0);
    // Default `warn`, but the ceiling has to admit `debug` for the override.
    assert_eq!(set_filter_spec("warn drv_uart=debug"), Ok(()));

    // `info` sits below the default level, so the console drops it; the ring
    // keeps it anyway because the ceiling is debug.
    super::info!("klogger logbuf info marker");
    super::debug!("klogger logbuf debug marker");

    let mut buf = [0u8; 4096];
    assert!(
        logbuf_len() > 0,
        "capture did not grow the kernel log buffer"
    );
    let len = logbuf_snapshot(&mut buf);
    let text = core::str::from_utf8(&buf[..len]).expect("log text is utf-8");
    let line_with = |needle: &str| -> &str {
        text.lines()
            .find(|line| line.contains(needle))
            .expect("record missing from the kernel log buffer")
    };
    // Each line carries X-Kernel's own level number, which is what makes
    // `dmesg | grep '^<4>'` a level filter.
    assert!(
        line_with("klogger logbuf info marker").starts_with("<3>"),
        "info record lost its level: {text}"
    );
    assert!(
        line_with("klogger logbuf debug marker").starts_with("<4>"),
        "debug record lost its level: {text}"
    );
    // Records are newline-terminated: without it `dmesg` prints the whole
    // buffer as one run-on line.
    assert!(text.ends_with('\n'), "record without newline: {text:?}");

    // Clearing drops this test's records; unrelated records may follow.
    logbuf_clear();
    let len = logbuf_snapshot(&mut buf);
    let text = core::str::from_utf8(&buf[..len]).expect("log text is utf-8");
    assert!(
        !text.contains("klogger logbuf"),
        "clear left stale records: {text}"
    );

    if let Ok(spec) = core::str::from_utf8(&previous_buf[..previous_len]) {
        set_log_level(spec);
    }
}

/// A destination smaller than the ring returns the newest bytes, not the
/// oldest ones, so `dmesg` on a full buffer shows the tail of the boot log.
#[def_test(serial)]
fn logbuf_snapshot_returns_the_newest_bytes() {
    super::warn!("klogger newest-bytes marker");
    let mut small = [0u8; 16];
    let len = logbuf_snapshot(&mut small);
    assert_eq!(len, 16);
    assert!(core::str::from_utf8(&small).expect("utf-8").contains("marker"));
}
