// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![no_main]
use core::{
    arch::{asm, global_asm},
    sync::atomic::{AtomicU64, Ordering},
};
const CPU_COUNT: usize = env!("S3_TEST_CPUS").as_bytes()[0] as usize - b'0' as usize;
global_asm!(
    ".global _start",
    "_start:",
    "and rsp, -16",
    "call e2e_main",
    "ud2"
);
/// Invokes the x86-64 Linux syscall ABI without a userspace runtime.
///
/// # Safety
/// Pointer arguments must cover the memory required by the selected syscall,
/// remain live and obey its read/write aliasing contract until it returns.
/// The process must have a valid userspace stack and Linux syscall entry.
unsafe fn call(n: u64, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> i64 {
    let result: i64;
    // SAFETY: the caller supplies syscall-specific memory and a live stack.
    // Linux uses rax/rdi/rsi/rdx/r10/r8/r9; rcx and r11 are declared clobbered,
    // and syscall returns on the same userspace stack without touching the red zone.
    unsafe {
        asm!("syscall",inlateout("rax") n=>result,in("rdi")a,in("rsi")b,in("rdx")c,
        in("r10")d,in("r8")e,in("r9")f,lateout("rcx")_,lateout("r11")_,options(nostack));
    }
    result
}
fn sys(n: u64, a: u64, b: u64, c: u64) -> i64 {
    // SAFETY: all calls in this standalone probe use audited Linux syscall
    // numbers and live buffers with matching access. No operation retains a
    // borrowed pointer after returning; unused ABI argument slots are zero.
    unsafe { call(n, a, b, c, 0, 0, 0) }
}
fn text(s: &str) {
    sys(1, 1, s.as_ptr() as u64, s.len() as u64);
}
fn number(mut n: u64) {
    let mut b = [0u8; 20];
    let mut i = 20;
    loop {
        i -= 1;
        b[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    sys(1, 1, b[i..].as_ptr() as u64, (20 - i) as u64);
}
fn exit(code: u64) -> ! {
    sys(60, code, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}
fn check(ok: bool, stage: &str) {
    if !ok {
        text("E2E_FAIL: ");
        text(stage);
        text("\n");
        exit(2);
    }
}
fn pin(cpu: u64) {
    let mask = 1u64 << cpu;
    check(
        sys(203, 0, 8, (&mask as *const u64) as u64) == 0,
        "CPU affinity",
    );
}
#[repr(C)]
struct Timespec {
    i: i64,
    n: i64,
}
fn clock(id: u64) -> u64 {
    let mut t = Timespec { i: 0, n: 0 };
    check(
        sys(228, id, (&mut t as *mut Timespec) as u64, 0) == 0,
        "clock_gettime",
    );
    check(t.i >= 0 && t.n >= 0, "clock range");
    t.i as u64 * 1_000_000_000 + t.n as u64
}
#[repr(C)]
struct TimerSpec {
    interval: Timespec,
    value: Timespec,
}
fn timer(id: u64, delay: u64) -> u64 {
    let fd = sys(283, id, 0x800, 0);
    check(fd >= 0, "timerfd_create");
    let deadline = clock(id) + delay;
    let value = TimerSpec {
        interval: Timespec { i: 0, n: 0 },
        value: Timespec {
            i: (deadline / 1_000_000_000) as i64,
            n: (deadline % 1_000_000_000) as i64,
        },
    };
    check(
        // SAFETY: value is an initialized, aligned TimerSpec that remains live
        // and immutably borrowed until timerfd_settime copies it; old_value is null.
        unsafe {
            call(
                286,
                fd as u64,
                1,
                (&value as *const TimerSpec) as u64,
                0,
                0,
                0,
            )
        } == 0,
        "timerfd_settime",
    );
    fd as u64
}
fn timer_count(fd: u64) -> i64 {
    let mut n = 0u64;
    let result = sys(0, fd, (&mut n as *mut u64) as u64, 8);
    if result == 8 {
        check(n > 0, "timer expiration count");
        n as i64
    } else {
        result
    }
}
#[repr(C)]
struct Shared {
    stop: AtomicU64,
    failed: AtomicU64,
    counts: [AtomicU64; 4],
}
fn child(cpu: usize, shared: &Shared) -> ! {
    pin(cpu as u64);
    let mut last = clock(1);
    // Observe the parent's Release-published stop request before terminating.
    while shared.stop.load(Ordering::Acquire) == 0 {
        let now = clock(1);
        if now < last {
            // Publish this failure to the parent's Acquire check in wait_counts.
            shared.failed.store(1, Ordering::Release);
        }
        last = now;
        // Publish progress to the parent's Acquire snapshot/check. Each child
        // owns its counter; reaching a later value proves resumed execution.
        shared.counts[cpu].fetch_add(1, Ordering::Release);
    }
    exit(0)
}
fn wait_counts(shared: &Shared, before: [u64; 4]) {
    let start = clock(1);
    loop {
        // Pairs with each child's Release failure publication.
        check(
            shared.failed.load(Ordering::Acquire) == 0,
            "remote clock went backwards",
        );
        let ready =
            (1..CPU_COUNT).all(|cpu| shared.counts[cpu].load(Ordering::Acquire) > before[cpu] + 16);
        if ready {
            return;
        }
        check(clock(1) - start < 5_000_000_000, "remote CPU progress");
        sys(24, 0, 0, 0);
    }
}
fn read_sector(fd: u64, buffer: &mut [u8; 512]) {
    check(sys(8, fd, 0, 0) == 0, "raw disk seek");
    check(
        sys(0, fd, buffer.as_mut_ptr() as u64, 512) == 512,
        "raw disk read",
    );
}
#[unsafe(no_mangle)]
extern "C" fn e2e_main() -> ! {
    pin(0);
    // SAFETY: mmap receives a null hint, one page, anonymous shared RW flags
    // and no source buffer. The result is checked before forming any reference.
    let shared_address = unsafe { call(9, 0, 4096, 3, 0x21, u64::MAX, 0) };
    check(shared_address > 0, "shared mmap");
    // SAFETY: successful anonymous mmap returns a page-aligned zeroed 4096-byte
    // region, larger than Shared. Zero represents each AtomicU64; all shared
    // access is atomic and the mapping remains live through every child join.
    let shared = unsafe { &*(shared_address as *const Shared) };
    let mut children = [0i64; 3];
    for cpu in 1..CPU_COUNT {
        let pid = sys(57, 0, 0, 0);
        check(pid >= 0, "fork");
        if pid == 0 {
            child(cpu, shared);
        }
        children[cpu - 1] = pid;
    }
    wait_counts(shared, [0; 4]);
    let raw = sys(257, (-100i64) as u64, b"/dev/vda\0".as_ptr() as u64, 0);
    check(raw >= 0, "open raw disk");
    let mut original = [0u8; 512];
    read_sector(raw as u64, &mut original);
    for cycle in 1..=env!("S3_TEST_ROUNDS").parse::<u64>().unwrap() {
        let mut counts = [0u64; 4];
        for cpu in 1..CPU_COUNT {
            counts[cpu] = shared.counts[cpu].load(Ordering::Acquire);
        }
        let mono_timer = timer(1, 2_000_000_000);
        let boot_timer = timer(7, 1_000_000_000);
        let mono = clock(1);
        let boot = clock(7);
        let real = clock(0);
        text("E2E_CYCLE ");
        number(cycle);
        text(" REQUEST\n");
        let result = sys(169, 0xfee1dead, 672274793, 0xd000fce2);
        if option_env!("S3_TEST_UNSUPPORTED") == Some("1") {
            check(
                result == -95,
                "unsupported configuration must return EOPNOTSUPP",
            );
            sys(3, mono_timer, 0, 0);
            sys(3, boot_timer, 0, 0);
            wait_counts(shared, counts);
            let mut after = [0u8; 512];
            read_sector(raw as u64, &mut after);
            check(after == original, "disk after refused suspend");
            text("E2E_REFUSAL ");
            number(cycle);
            text(" PASS\n");
            continue;
        }
        if result != 0 {
            text("E2E_REBOOT_ERROR ");
            number(result.unsigned_abs());
            text("\n");
            exit(2);
        }
        let dm = clock(1) - mono;
        let db = clock(7) - boot;
        let dr = clock(0) - real;
        // Emit independent deltas before assertions so a timing failure retains
        // its measurements instead of hiding them behind the final PASS marker.
        text("E2E_CLOCKS cycle=");
        number(cycle);
        text(" mono_ns=");
        number(dm);
        text(" boot_ns=");
        number(db);
        text(" real_ns=");
        number(dr);
        text("\n");
        check(dm < 2_000_000_000, "MONOTONIC included suspend or jumped");
        // The host independently bounds BOOTTIME/REALTIME against the observed
        // request-to-return interval. Firmware/AP restoration can extend the
        // three-second QMP sleep; a fixed five-second upper bound is invalid.
        check(
            db >= dm && db - dm >= 2_000_000_000,
            "BOOTTIME sleep compensation",
        );
        check(
            dr >= dm && dr - dm >= 2_000_000_000,
            "REALTIME sleep compensation",
        );
        check(
            timer_count(mono_timer) == -11,
            "MONOTONIC timer expired across sleep",
        );
        let timer_wait = clock(1);
        loop {
            let count = timer_count(boot_timer);
            if count > 0 {
                break;
            }
            check(count == -11, "BOOTTIME timer read");
            check(
                clock(1) - timer_wait < 1_000_000_000,
                "BOOTTIME timer not rescanned after wake",
            );
            sys(24, 0, 0, 0);
        }
        sys(3, mono_timer, 0, 0);
        sys(3, boot_timer, 0, 0);
        let delay = Timespec {
            i: 0,
            n: 100_000_000,
        };
        let sleep_start = clock(1);
        check(
            sys(35, (&delay as *const Timespec) as u64, 0, 0) == 0,
            "post-resume nanosleep",
        );
        check(clock(1) - sleep_start >= 100_000_000, "nanosleep deadline");
        let mut after = [0u8; 512];
        read_sector(raw as u64, &mut after);
        check(after == original, "raw disk changed");
        // SAFETY: the static NUL-terminated path remains valid during openat;
        // create flags supply the initialized numeric mode and no output pointer.
        let fd = unsafe {
            call(
                257,
                (-100i64) as u64,
                b"/pm-proof\0".as_ptr() as u64,
                0x242,
                0o600,
                0,
                0,
            )
        };
        check(fd >= 0, "post-resume file create");
        let data = [cycle as u8; 4096];
        check(
            sys(1, fd as u64, data.as_ptr() as u64, 4096) == 4096,
            "post-resume file write",
        );
        check(sys(74, fd as u64, 0, 0) == 0, "post-resume file fsync");
        check(sys(3, fd as u64, 0, 0) == 0, "file close");
        wait_counts(shared, counts);
        text("E2E_CYCLE ");
        number(cycle);
        text(" PASS mono_ns=");
        number(dm);
        text(" sleep_ns=");
        number(db - dm);
        text("\n");
    }
    // Publish termination to the children's Acquire stop loads before joining.
    shared.stop.store(1, Ordering::Release);
    for pid in children.into_iter().take(CPU_COUNT - 1) {
        let mut status = 0i32;
        check(
            sys(61, pid as u64, (&mut status as *mut i32) as u64, 0) == pid,
            "waitpid",
        );
        check(status == 0, "child status");
    }
    sys(3, raw as u64, 0, 0);
    if option_env!("S3_TEST_UNSUPPORTED") == Some("1") {
        text("E2E_REFUSAL_PASS\n");
    } else {
        text("E2E_SUSPEND_PASS\n");
    }
    exit(0)
}
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    text("E2E_FAIL: panic\n");
    exit(2)
}
/// # Safety
/// p must cover n writable bytes in one live allocation with exclusive access.
#[unsafe(no_mangle)]
unsafe extern "C" fn memset(p: *mut u8, value: i32, n: usize) -> *mut u8 {
    for i in 0..n {
        // SAFETY: i < n stays within the caller's exclusively writable region.
        unsafe { p.add(i).write_volatile(value as u8) }
    }
    p
}
/// # Safety
/// q covers n initialized readable bytes; p covers n exclusively writable bytes.
/// Both allocations remain live and their ranges must not overlap.
#[unsafe(no_mangle)]
unsafe extern "C" fn memcpy(p: *mut u8, q: *const u8, n: usize) -> *mut u8 {
    for i in 0..n {
        // SAFETY: i < n bounds both caller-provided, nonoverlapping regions.
        let byte = unsafe { q.add(i).read_volatile() };
        // SAFETY: the destination is exclusively writable for this in-range byte.
        unsafe { p.add(i).write_volatile(byte) }
    }
    p
}
/// # Safety
/// p and q each cover n initialized readable bytes and remain live for the call.
/// Neither range may be concurrently mutated except through synchronization.
#[unsafe(no_mangle)]
unsafe extern "C" fn memcmp(p: *const u8, q: *const u8, n: usize) -> i32 {
    for i in 0..n {
        // SAFETY: i < n bounds each initialized input region; neither is written.
        let a = unsafe { p.add(i).read_volatile() };
        // SAFETY: the second caller-provided input has the same readable extent.
        let b = unsafe { q.add(i).read_volatile() };
        if a != b {
            return a as i32 - b as i32;
        }
    }
    0
}
