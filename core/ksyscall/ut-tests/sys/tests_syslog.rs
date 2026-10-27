// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `syslog(2)`: per-action privilege policy and the read-side contract.

use unittest::{assert, assert_eq, def_test};
use unittest_support::TestUserBuffer;

use super::{
    SYSLOG_ACTION_CLEAR, SYSLOG_ACTION_READ_ALL, SYSLOG_ACTION_READ_CLEAR,
    SYSLOG_ACTION_SIZE_BUFFER, SYSLOG_ACTION_SIZE_UNREAD, action_needs_privilege, sys_syslog,
};
use klogger::LOGBUF_SIZE;

/// Reading the log is open to every caller, like Linux with `dmesg_restrict`
/// off; mutating it and the unread count stay privileged. Unknown actions are
/// restricted, which is how Linux reaches `EPERM` before its `EINVAL` default.
#[def_test]
fn action_privilege_policy_matches_linux_per_action() {
    assert!(!action_needs_privilege(SYSLOG_ACTION_READ_ALL));
    assert!(!action_needs_privilege(SYSLOG_ACTION_SIZE_BUFFER));

    assert!(action_needs_privilege(SYSLOG_ACTION_READ_CLEAR));
    assert!(action_needs_privilege(SYSLOG_ACTION_CLEAR));
    assert!(action_needs_privilege(SYSLOG_ACTION_SIZE_UNREAD));

    // Unknown actions are restricted as well.
    assert!(action_needs_privilege(0));
    assert!(action_needs_privilege(6));
    assert!(action_needs_privilege(i32::MAX));
}

/// `SIZE_BUFFER` reports the ring capacity and needs no destination buffer.
#[def_test]
fn size_buffer_reports_the_ring_capacity() {
    assert_eq!(
        sys_syslog(SYSLOG_ACTION_SIZE_BUFFER, core::ptr::null_mut(), 0),
        Ok(LOGBUF_SIZE as isize)
    );
}

/// `READ_ALL` returns the newest whole records and leaves them buffered, so a
/// second read returns the same window.
///
/// `user`: the destination must be a real user mapping, so the case runs in the
/// user-test context that `TestUserBuffer` needs.
#[def_test(user)]
fn read_all_returns_bytes_without_consuming_them() {
    let buffer = TestUserBuffer::new(4096).unwrap();
    let before = klogger::logbuf_len();
    let read = sys_syslog(
        SYSLOG_ACTION_READ_ALL,
        buffer.as_user_ptr(),
        buffer.len() as i32,
    )
    .expect("a privileged read succeeds");
    assert!(read >= 0);
    assert_eq!(
        klogger::logbuf_len(),
        before,
        "READ_ALL must not consume the ring"
    );
    assert!(read as usize <= buffer.len());

    let bytes = buffer.read_bytes(read as usize).unwrap();
    if !bytes.is_empty() {
        // The window starts at a record boundary and is character-aligned.
        let text = core::str::from_utf8(&bytes).expect("a whole-record window is valid UTF-8");
        assert!(text.starts_with('<'), "window does not start a record: {text:?}");
    }

    // The same window is returned again, because nothing was released.
    let again = sys_syslog(
        SYSLOG_ACTION_READ_ALL,
        buffer.as_user_ptr(),
        buffer.len() as i32,
    )
    .expect("a privileged read succeeds");
    assert_eq!(again, read);
}

/// `READ_CLEAR` consumes the whole unread range, including the records older
/// than the returned window — Linux advances its clear cursor past everything it
/// skipped as well, so `dmesg -c` does clear the log.
///
/// `user` for the same reason as the read case.
#[def_test(user)]
fn read_clear_drops_the_unread_range() {
    klogger::error!("syslog read-clear marker");
    assert!(klogger::logbuf_len() > 0);

    let buffer = TestUserBuffer::new(klogger::LOGBUF_SIZE).unwrap();
    let read = sys_syslog(
        SYSLOG_ACTION_READ_CLEAR,
        buffer.as_user_ptr(),
        buffer.len() as i32,
    )
    .expect("a privileged read succeeds");
    assert!(read > 0);
    let bytes = buffer.read_bytes(read as usize).unwrap();
    assert!(
        core::str::from_utf8(&bytes).is_ok(),
        "the cleared window is not whole records"
    );

    // The cleared range does not come back; other contexts may have logged since.
    let mut probe = [0u8; 4096];
    let len = klogger::logbuf_read_newest(&mut probe);
    let text = core::str::from_utf8(&probe[..len]).expect("log text is utf-8");
    assert!(
        !text.contains("syslog read-clear marker"),
        "READ_CLEAR left the record behind: {text}"
    );
}

/// `len` is a signed ABI argument: a negative value fails like Linux's
/// `len < 0` instead of being clamped to the ring size.
#[def_test(user)]
fn negative_length_is_rejected() {
    let buffer = TestUserBuffer::new(64).unwrap();
    for action in [SYSLOG_ACTION_READ_ALL, SYSLOG_ACTION_READ_CLEAR] {
        assert_eq!(
            sys_syslog(action, buffer.as_user_ptr(), -1),
            Err(kerrno::KError::InvalidInput)
        );
    }
}

/// An unknown action from a privileged caller reports `EINVAL`, like Linux's
/// `switch` default.
#[def_test(user)]
fn unknown_action_is_rejected() {
    let buffer = TestUserBuffer::new(64).unwrap();
    assert_eq!(
        sys_syslog(6, buffer.as_user_ptr(), 64),
        Err(kerrno::KError::InvalidInput)
    );
}