// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real terminal storage and klazy::Once with synthetic CPU/frame providers.
//! This tests publication and nested admission, not stack unwinding or NMIs.

extern crate self as kbuild_config;
extern crate self as kcpu_id_map;
extern crate self as khal;
extern crate self as klazy;
extern crate self as unittest;

use std::{
    cell::{Cell, RefCell},
    sync::{Arc, Barrier, Mutex},
    thread,
};

#[path = "../../../../../util/klazy/src/once.rs"]
pub mod real_once;
pub use real_once::Once;
#[path = "../../../../../task/ktask/src/snapshot/terminal.rs"]
mod terminal;
use terminal::TerminalSnapshot;

pub const NR_CPUS: usize = 4;

thread_local! {
    static CPU: Cell<usize> = const { Cell::new(0) };
    static IP: Cell<usize> = const { Cell::new(0) };
    static CAPTURE_HOOK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

static TEST_SERIAL: Mutex<()> = Mutex::new(());
static OUTPUT: Mutex<Vec<String>> = Mutex::new(Vec::new());
static DUMPS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

pub fn for_each_present_logical_cpu(mut callback: impl FnMut(usize, percpu::CpuId, usize)) {
    // Deliberately sparse membership.
    callback(0, percpu::CpuId(0), 2);
    callback(1, percpu::CpuId(3), 2);
}

pub fn cpu_bit(cpu: usize) -> Option<usize> {
    (cpu < NR_CPUS).then(|| 1 << cpu)
}

pub mod percpu {
    #[derive(Clone, Copy)]
    pub struct CpuId(pub usize);
    impl CpuId {
        pub fn as_usize(&self) -> usize {
            self.0
        }
    }
    pub fn this_cpu_id() -> CpuId {
        CpuId(super::CPU.get())
    }
}

pub mod context {
    #[derive(Clone, Copy)]
    pub struct TrapFrame {
        pub ip: usize,
    }
    pub fn active_exception_context() -> Option<TrapFrame> {
        let hook = super::CAPTURE_HOOK.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
        Some(TrapFrame {
            ip: super::IP.get(),
        })
    }
}

pub mod dump {
    pub fn dump_terminal_backtrace(cpu: super::percpu::CpuId, frame: &super::context::TrapFrame) {
        super::DUMPS
            .lock()
            .unwrap()
            .push((cpu.as_usize(), frame.ip));
    }
}

#[macro_export]
macro_rules! kprint_atomic {
    ($($tokens:tt)*) => { $crate::OUTPUT.lock().unwrap().push(format!($($tokens)*)) };
}
#[macro_export]
macro_rules! path_mod_test {
    ($($tokens:tt)*) => {};
}

fn dump(snapshot: &TerminalSnapshot, mask: usize) {
    // SAFETY: the synthetic backtrace provider reads only copied integers;
    // no real stack address is followed and test threads retain the storage.
    unsafe { snapshot.dump(mask) };
}

fn reset_output() {
    OUTPUT.lock().unwrap().clear();
    DUMPS.lock().unwrap().clear();
}

#[test]
fn duplicate_capture_retains_the_first_frame_and_masks_out_missing_cpus() {
    let _serial = TEST_SERIAL.lock().unwrap();
    reset_output();
    let snapshot = TerminalSnapshot::new();
    CPU.set(0);
    IP.set(0x1000);
    snapshot.collect_local();
    IP.set(0x2000);
    snapshot.collect_local();
    dump(&snapshot, 1);
    assert_eq!(*DUMPS.lock().unwrap(), [(0, 0x1000)]);
    assert!(
        OUTPUT
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("cpu=3 NOT COLLECTED"))
    );
}

#[test]
fn reader_does_not_wait_for_a_late_publisher_or_read_its_uninitialized_slot() {
    let _serial = TEST_SERIAL.lock().unwrap();
    reset_output();
    let snapshot = Arc::new(TerminalSnapshot::new());
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    thread::scope(|scope| {
        let producer = snapshot.clone();
        let entered_producer = entered.clone();
        let release_producer = release.clone();
        scope.spawn(move || {
            CPU.set(3);
            IP.set(0x3000);
            CAPTURE_HOOK.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    entered_producer.wait();
                    release_producer.wait();
                }));
            });
            producer.collect_local();
        });
        entered.wait();
        dump(&snapshot, 1 << 3);
        assert!(DUMPS.lock().unwrap().is_empty());
        assert!(
            OUTPUT
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("frame not published"))
        );
        release.wait();
    });
    reset_output();
    dump(&snapshot, 1 << 3);
    assert_eq!(*DUMPS.lock().unwrap(), [(3, 0x3000)]);
}

#[test]
fn nested_capture_returns_without_waiting_on_the_interrupted_once_initializer() {
    let _serial = TEST_SERIAL.lock().unwrap();
    reset_output();
    let snapshot = Arc::new(TerminalSnapshot::new());
    let nested = snapshot.clone();
    CPU.set(0);
    IP.set(0x4000);
    CAPTURE_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || nested.collect_local()));
    });
    snapshot.collect_local();
    dump(&snapshot, 1);
    assert_eq!(*DUMPS.lock().unwrap(), [(0, 0x4000)]);
}
