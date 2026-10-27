// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::string::ToString;

use log::{Level, LevelFilter};
use unittest::{assert_eq, def_test};

use super::{
    build_level_cap, current_log_level, filter_enabled, filter_spec, level_name, level_number,
    logbuf_clear, logbuf_read_newest, parse_log_level, set_filter_spec, set_log_level,
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
/// "quiet console, complete log" contract `dmesg` depends on, and it holds for
/// every build cap because capture is no longer derived from the console filter.
///
/// `serial`: the ring and the filter are process-global resources. The assertions
/// below are scoped to this test's own unique markers because other kernel
/// threads keep logging concurrently.
#[def_test(serial)]
fn logbuf_captures_records_the_console_filter_hides() {
    let mut previous_buf = [0u8; super::MAX_FILTER_SPEC];
    let previous_len = filter_spec(&mut previous_buf).unwrap_or(0);
    // Console filter `off`: even `error!` is not printed. `error` is at or below
    // every build cap, so the ring still records it in any configuration.
    assert_eq!(set_filter_spec("off"), Ok(()));
    // Drop whatever earlier contexts buffered: the window below must be this
    // test's records.
    logbuf_clear();

    super::error!("klogger logbuf captured marker");

    let mut buf = [0u8; 4096];
    let len = logbuf_read_newest(&mut buf);
    let text = core::str::from_utf8(&buf[..len]).expect("log text is utf-8");
    let line = text
        .lines()
        .find(|line| line.contains("klogger logbuf captured marker"))
        .expect("the record the console dropped is missing from the ring");
    // Each line carries X-Kernel's own level number, which is what makes
    // `dmesg | grep '^<1>'` a level filter.
    assert!(
        line.starts_with("<1>"),
        "the captured record lost its level: {text}"
    );
    // Records are newline-terminated: without it `dmesg` prints the whole
    // buffer as one run-on line.
    assert!(text.ends_with('\n'), "record without newline: {text:?}");

    // Clearing drops this test's records; unrelated records may follow.
    logbuf_clear();
    let len = logbuf_read_newest(&mut buf);
    let text = core::str::from_utf8(&buf[..len]).expect("log text is utf-8");
    assert!(
        !text.contains("klogger logbuf"),
        "clear left stale records: {text}"
    );

    if let Ok(spec) = core::str::from_utf8(&previous_buf[..previous_len]) {
        set_log_level(spec);
    }
}

/// A read returns the newest whole records that fit, oldest first, starting at a
/// record boundary — Linux's `syslog_print_all` shape — and never consumes.
///
/// `serial` because the ring is process-global; the level used is `error`, which
/// is at or below every build cap, and the records are emitted while the console
/// filter is `off` so nothing goes through the UART.
#[def_test(serial)]
fn logbuf_read_returns_the_newest_whole_records() {
    let mut previous_buf = [0u8; super::MAX_FILTER_SPEC];
    let previous_len = filter_spec(&mut previous_buf).unwrap_or(0);
    assert_eq!(set_filter_spec("off"), Ok(()));
    logbuf_clear();

    super::error!("klogger order marker one");
    super::error!("klogger order marker two");

    let mut buf = [0u8; 4096];
    let len = logbuf_read_newest(&mut buf);
    let text = core::str::from_utf8(&buf[..len]).expect("log text is utf-8");
    let first = text.find("klogger order marker one").expect("first record");
    let second = text.find("klogger order marker two").expect("second record");
    assert!(first < second, "records are not in log order: {text}");
    assert!(text.ends_with('\n'), "window does not end a record: {text:?}");

    // Reading does not consume: the same records come back.
    let again = logbuf_read_newest(&mut buf);
    assert!(again >= len, "the second read lost records");

    // A destination smaller than the ring gets the *newest* records, aligned to
    // a record boundary, never a prefix of the whole buffer.
    let mut small = [0u8; 96];
    let small_len = logbuf_read_newest(&mut small);
    assert!(small_len > 0 && small_len <= small.len());
    let tail = core::str::from_utf8(&small[..small_len]).expect("log text is utf-8");
    assert!(
        tail.starts_with('<'),
        "the window does not start at a record boundary: {tail:?}"
    );
    assert!(
        tail.contains("klogger order marker two"),
        "a short destination did not receive the newest record: {tail:?}"
    );

    logbuf_clear();
    if let Ok(spec) = core::str::from_utf8(&previous_buf[..previous_len]) {
        set_log_level(spec);
    }
}

/// The build cap is what the `log` crate gates on, so it must never be `off` and
/// must match a level the image was built for.
#[def_test]
fn build_cap_is_a_level_the_image_can_produce() {
    let cap = build_level_cap();
    assert!(
        cap != log::LevelFilter::Off,
        "an unset LOG_LEVEL_* must fall back to a level, not silence the kernel"
    );
}

/// A record longer than the line buffer keeps its terminating newline and never
/// ends in the middle of a code point, so `dmesg` still sees one line per
/// record.
#[def_test(serial)]
fn logbuf_terminates_truncated_records() {
    use alloc::string::String;

    let mut previous_buf = [0u8; super::MAX_FILTER_SPEC];
    let previous_len = filter_spec(&mut previous_buf).unwrap_or(0);
    // `off` keeps the console quiet; `error` is at or below every build cap, so
    // the ring records these in any configuration.
    assert_eq!(set_filter_spec("off"), Ok(()));
    logbuf_clear();

    // Four times the 256-byte line buffer, so both records are truncated.
    let long = String::from("0123456789abcdef").repeat(16);
    super::error!("marker-one {long}");
    super::error!("marker-two {long}");
    // Truncation must not split a three-byte code point.
    super::error!("{}", String::from("\u{5b57}").repeat(120));

    let mut buf = [0u8; 4096];
    let len = logbuf_read_newest(&mut buf);
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
