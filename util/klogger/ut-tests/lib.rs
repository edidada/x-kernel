// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::string::ToString;

use log::{Level, LevelFilter};
use unittest::{assert_eq, def_test};

use super::{
    current_log_level, filter_enabled, filter_spec, level_name, level_number, logbuf_clear,
    logbuf_len, logbuf_read, logbuf_release, parse_log_level, set_filter_spec, set_log_level,
    set_log_level_filter,
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
fn filter_spec_reports_the_canonical_text() {
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

/// The console decision itself: the longest matching prefix wins, and a target
/// without a rule falls back to the default level.
#[def_test(serial)]
fn filter_rules_match_the_longest_prefix() {
    let previous = current_log_level();
    assert_eq!(
        set_filter_spec("error drv_uart=debug drv_uart::dw_uart=trace"),
        Ok(())
    );

    // The most specific rule decides for the nested module.
    assert!(filter_enabled("drv_uart::dw_uart", Level::Trace));
    assert!(filter_enabled("drv_uart::dw_uart", Level::Debug));
    assert!(filter_enabled("drv_uart::dw_uart", Level::Error));

    // Its sibling inherits the shorter rule, not the nested one.
    assert!(filter_enabled("drv_uart::other", Level::Debug));
    assert!(!filter_enabled("drv_uart::other", Level::Trace));

    // Everything else uses the default (`error`).
    assert!(filter_enabled("ksyscall", Level::Error));
    assert!(!filter_enabled("ksyscall", Level::Warn));
    assert!(!filter_enabled("spi", Level::Warn));

    // Matching is textual prefix, not path-segment: a longer module path that
    // merely starts with the rule's target is covered by it.
    assert!(filter_enabled("drv_uart_extra", Level::Debug));
    assert!(!filter_enabled("drv_uart_extra", Level::Trace));

    set_log_level_filter(previous);
}

/// A spec without a bare level keeps the default in force and replaces only the
/// rule set: `ksyscall=trace` must not silently reset the rest of the kernel to
/// a hard-coded `warn`.
#[def_test(serial)]
fn filter_spec_without_a_level_keeps_the_current_default() {
    let previous = current_log_level();
    set_log_level_filter(LevelFilter::Debug);

    assert_eq!(set_filter_spec("ksyscall=trace"), Ok(()));
    assert_eq!(current_log_level(), LevelFilter::Debug);
    assert!(filter_enabled("drv_uart", Level::Debug));
    assert!(filter_enabled("ksyscall", Level::Trace));

    let mut buf = [0u8; super::MAX_FILTER_SPEC];
    let len = filter_spec(&mut buf).expect("spec fits");
    assert_eq!(
        core::str::from_utf8(&buf[..len]),
        Ok("debug ksyscall=trace")
    );

    // A bare level still replaces the default, and the rule set is replaced
    // wholesale rather than merged.
    assert_eq!(set_filter_spec("warn drv_uart=info"), Ok(()));
    assert_eq!(current_log_level(), LevelFilter::Warn);
    assert!(!filter_enabled("ksyscall", Level::Trace));

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
    // Reads start at the oldest unread byte, so drop whatever earlier contexts
    // buffered: the 4 KiB window below must be this test's records.
    logbuf_clear();

    // `info` sits below the default level, so the console drops it; the ring
    // keeps it anyway because the ceiling is debug.
    super::info!("klogger logbuf info marker");
    super::debug!("klogger logbuf debug marker");

    let mut buf = [0u8; 4096];
    assert!(
        logbuf_len() > 0,
        "capture did not grow the kernel log buffer"
    );
    let len = logbuf_read(&mut buf).copied();
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
    let len = logbuf_read(&mut buf).copied();
    let text = core::str::from_utf8(&buf[..len]).expect("log text is utf-8");
    assert!(
        !text.contains("klogger logbuf"),
        "clear left stale records: {text}"
    );

    if let Ok(spec) = core::str::from_utf8(&previous_buf[..previous_len]) {
        set_log_level(spec);
    }
}

/// Reads start at the oldest unread byte and advance with each release, so a
/// reader that drains in chunks makes progress through the log instead of
/// re-reading the newest tail.
///
/// Two properties make this reproducible on a shared ring: the level is pinned
/// (the ring only captures what clears the `log` ceiling, and the CI unittest
/// build resolves that ceiling to `off`), and the records are emitted below this
/// module's console level (ceiling `trace`, default `warn`) so they never go
/// through the UART.
#[def_test(serial)]
fn logbuf_read_starts_at_the_oldest_unread_byte() {
    let mut previous_buf = [0u8; super::MAX_FILTER_SPEC];
    let previous_len = filter_spec(&mut previous_buf).unwrap_or(0);
    assert_eq!(set_filter_spec("warn ksyscall=trace"), Ok(()));
    logbuf_clear();

    super::info!("klogger order marker one");
    super::info!("klogger order marker two");

    let mut buf = [0u8; 4096];
    let read = logbuf_read(&mut buf);
    let text = core::str::from_utf8(&buf[..read.copied()]).expect("log text is utf-8");
    let first = text.find("klogger order marker one").expect("first record");
    let second = text.find("klogger order marker two").expect("second record");
    assert!(
        first < second,
        "records are not in log order: {text}"
    );

    // A destination smaller than the buffer returns a full window, and the
    // sequence position advances by exactly what was released.
    let mut small = [0u8; 16];
    let window = logbuf_read(&mut small);
    assert_eq!(window.copied(), small.len());
    assert_eq!(logbuf_release(window), small.len());
    let next = logbuf_read(&mut small);
    assert_eq!(
        next.from,
        window.from + small.len() as u64,
        "the next read did not advance past the released window"
    );
    assert_eq!(logbuf_release(next), small.len());

    // Releasing the same window twice must not drop a second time: the bytes
    // are already gone.
    assert_eq!(logbuf_release(next), 0);

    // The records survive a partial release; only the released bytes are gone.
    let mut rest = [0u8; 4096];
    let rest_len = logbuf_read(&mut rest).copied();
    let text = core::str::from_utf8(&rest[..rest_len]).expect("log text is utf-8");
    assert!(
        text.contains("klogger order marker two"),
        "a short read destroyed an undelivered record: {text}"
    );

    if let Ok(spec) = core::str::from_utf8(&previous_buf[..previous_len]) {
        set_log_level(spec);
    }
}

/// A window that the ring overwrote while the reader was copying is stale:
/// releasing it drops nothing rather than discarding records the reader never
/// received.
#[def_test(serial)]
fn logbuf_release_ignores_an_overwritten_window() {
    let mut previous_buf = [0u8; super::MAX_FILTER_SPEC];
    let previous_len = filter_spec(&mut previous_buf).unwrap_or(0);
    assert_eq!(set_filter_spec("warn ksyscall=trace"), Ok(()));
    logbuf_clear();

    super::info!("klogger stale marker");
    let mut small = [0u8; 16];
    let window = logbuf_read(&mut small);
    assert_eq!(window.copied(), small.len());

    // `logbuf_clear` is the cheapest way to invalidate the reader's position,
    // and it reports the same staleness a ring wrap does.
    logbuf_clear();
    assert_eq!(logbuf_release(window), 0);
    super::info!("klogger after stale marker");
    let mut buf = [0u8; 4096];
    let len = logbuf_read(&mut buf).copied();
    let text = core::str::from_utf8(&buf[..len]).expect("log text is utf-8");
    assert!(
        text.contains("klogger after stale marker"),
        "releasing a stale window dropped current records: {text}"
    );

    if let Ok(spec) = core::str::from_utf8(&previous_buf[..previous_len]) {
        set_log_level(spec);
    }
}

/// A record longer than the line buffer keeps its terminating newline and never
/// ends in the middle of a code point, so `dmesg` still sees one line per
/// record.
#[def_test(serial)]
fn logbuf_terminates_truncated_records() {
    use alloc::string::String;

    let mut previous_buf = [0u8; super::MAX_FILTER_SPEC];
    let previous_len = filter_spec(&mut previous_buf).unwrap_or(0);
    assert_eq!(set_filter_spec("warn ksyscall=trace"), Ok(()));
    logbuf_clear();

    // Four times the 256-byte line buffer, so both records are truncated.
    let long = String::from("0123456789abcdef").repeat(16);
    super::info!("marker-one {long}");
    super::info!("marker-two {long}");
    // Truncation must not split a three-byte code point.
    super::info!("{}", String::from("\u{5b57}").repeat(120));

    let mut buf = [0u8; 4096];
    let len = logbuf_read(&mut buf).copied();
    let text = core::str::from_utf8(&buf[..len]).expect("log text is utf-8");
    let marker_lines: alloc::vec::Vec<&str> = text
        .lines()
        .filter(|line| line.contains("marker-one") || line.contains("marker-two"))
        .collect();
    assert_eq!(marker_lines.len(), 2, "records ran together: {text}");
    for line in marker_lines {
        assert!(line.len() < 256, "record over the line budget: {line:?}");
    }
    assert!(text.ends_with('\n'), "record without terminator: {text:?}");

    if let Ok(spec) = core::str::from_utf8(&previous_buf[..previous_len]) {
        set_log_level(spec);
    }
}
