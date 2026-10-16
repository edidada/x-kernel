// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicUsize, Ordering};

use subsys_char::{KError, KResult};
use serial_core::test_support::SerialPortFixture;
use unittest::def_test;

use super::*;

struct RuntimeIo {
    calls: Arc<AtomicUsize>,
    fails: bool,
}


impl CharDevice for RuntimeIo {
    fn read(&self, buf: &mut [u8]) -> KResult<usize> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fails {
            return Err(KError::BadState);
        }
        buf.fill(b'r');
        Ok(buf.len())
    }

    fn write(&self, buf: &[u8]) -> KResult<usize> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fails {
            Err(KError::BadState)
        } else {
            Ok(buf.len())
        }
    }
}

fn connection(
    _parent: &device_class::ClassParent,
    fails: bool,
    boot_port: Option<Arc<SerialPort>>,
    offline: bool,
) -> (Arc<ConsoleConnection>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    // Reuse live identity metadata without publishing or changing global state.
    // Only this test connection can access the synthetic runtime implementation.
    let runtime: CharDeviceImpl = Box::new(RuntimeIo {
        calls: calls.clone(),
        fails,
    });
    // A fresh detached parent per test device: sharing the stdout parent's
    // snapshot would alias its offline flag between test and live device.
    // `offline` expresses the removed-device state (the main-side test used
    // a bound-but-inactive device object for the same effect).
    let parent = device_class::ClassParent::detached(
        0x7e57,
        "tty-test",
        alloc::string::String::from("platform/static0x7e57"),
    );
    if offline {
        parent.mark_offline();
    }
    let handle =
        ClassDevice::try_new_with_info(parent, runtime, "tty-test".into())
            .unwrap();
    let connection = ConsoleConnection {
        boot_port,
        runtime: SpinNoPreempt::new(Some(handle)),
    };
    (Arc::new(connection), calls)
}

#[def_test]
fn runtime_connection_is_shared_by_read_write_and_repeated_handoff() {
    // Boot discovery precedes all UTs; this immutable parent stays active for
    // the run. No test publishes, removes or mutates the shared device.
    let Some(stdout) = stdout_console() else {
        unittest::ktest_println!("SKIP: no adopted runtime stdout in this configuration");
        return unittest::TestResult::Ignored;
    };
    let (mut connection, calls) =
        connection(stdout.parent_ref(), false, serial_core::stdout_port(), false);
    connection.try_handoff();
    let mut bytes = [0; 3];
    assert_eq!(TtyRead::read(&mut connection, &mut bytes), 3);
    assert_eq!(bytes, [b'r'; 3]);
    assert_eq!(TtyWrite::write(&connection, b"test"), Ok(4));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[def_test]
fn runtime_errors_return_both_directions_to_the_boot_port() {
    // The boot-owned parent is read-only and persistent throughout this run.
    let Some(stdout) = stdout_console() else {
        unittest::ktest_println!("SKIP: no adopted runtime stdout in this configuration");
        return unittest::TestResult::Ignored;
    };
    for write_first in [false, true] {
        let fixture = SerialPortFixture::new(b"boot-input");
        let (mut connection, calls) =
            connection(stdout.parent_ref(), true, Some(fixture.port()), false);
        let mut prefix = [0; 4];
        if write_first {
            assert_eq!(TtyWrite::write(&connection, b"first"), Ok(5));
            assert_eq!(TtyRead::read(&mut connection, &mut prefix), prefix.len());
        } else {
            assert_eq!(TtyRead::read(&mut connection, &mut prefix), prefix.len());
            assert_eq!(TtyWrite::write(&connection, b"first"), Ok(5));
        }
        assert_eq!(&prefix, b"boot");
        assert!(connection.runtime_handle().is_none());
        let mut suffix = [0; 16];
        let count = TtyRead::read(&mut connection, &mut suffix);
        assert_eq!(&suffix[..count], b"-input");
        assert_eq!(TtyRead::read(&mut connection, &mut suffix), 0);
        assert_eq!(TtyWrite::write(&connection, b"-second"), Ok(7));
        assert_eq!(fixture.output(), b"first-second");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}

#[def_test]
fn unavailable_runtime_clears_handle_and_uses_boot_port() {
    // A detached class parent never touches any registry: the test
    // connection exercises availability without global device lifecycle.
    let parent = device_class::ClassParent::detached(
        1,
        "tty-test",
        alloc::string::String::from("platform/static1"),
    );
    // The main-side equivalent used a bound-but-inactive device object;
    // the class model expresses unavailability as an offline parent
    // (passed as the `offline` flag to `connection`).
    let fixture = SerialPortFixture::new(b"boot");
    let (mut connection, calls) = connection(&parent, false, Some(fixture.port()), true);
    assert!(connection.runtime_handle().is_none());
    assert!(connection.runtime.lock().is_none());
    let mut bytes = [0; 4];
    assert_eq!(TtyRead::read(&mut connection, &mut bytes), 4);
    assert_eq!(&bytes, b"boot");
    assert_eq!(TtyWrite::write(&connection, b"out"), Ok(3));
    assert_eq!(fixture.output(), b"out");
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[def_test]
fn console_output_keeps_backend_owned_newline_processing() {
    let fixture = SerialPortFixture::new(&[]);
    let connection = Arc::new(ConsoleConnection {
        boot_port: Some(fixture.port()),
        runtime: SpinNoPreempt::new(None),
    });
    let tty = Tty::new(
        Arc::default(),
        TtyKind::Terminal,
        TtyConfig {
            reader: connection.clone(),
            writer: connection,
            process_mode: ProcessMode::Manual,
        },
    );
    assert_eq!(tty.output.lock().write(b"line\n"), Ok(5));
    // The test serial backend is raw; the TTY must pass LF through unchanged.
    assert_eq!(fixture.output(), b"line\n");
}

#[def_test]
fn direct_boot_connection_and_manual_input_remain_usable() {
    let connection = Arc::new(ConsoleConnection::new());
    assert!(connection.runtime_handle().is_none());
    assert_eq!(ConsoleConnection::read(&connection, &mut []), 0);
    assert_eq!(TtyWrite::write(&connection, &[]), Ok(0));
    let tty = Tty::new(
        Arc::default(),
        TtyKind::Terminal,
        TtyConfig {
            reader: connection.clone(),
            writer: connection,
            process_mode: ProcessMode::Manual,
        },
    );
    assert!(tty.input.lock().input_waker().is_none());
}
