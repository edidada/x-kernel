// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The LCTrace event catalog.

/// Declares the event catalog once and derives both the discriminant enum and
/// the name table from it, so an event can never gain an id without also
/// gaining a name.
macro_rules! define_events {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {
        /// An LCTrace event kind.
        ///
        /// The discriminant is what actually lands in a record; the textual
        /// name is resolved only when a snapshot is rendered.
        #[repr(u32)]
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum LcTraceEvent {
            $($(#[$doc])* $name),*
        }

        const EVENT_NAMES: &[&str] = &[$(stringify!($name)),*];
    };
}

define_events! {
    /// A task was switched out in favour of another one.
    ///
    /// Arguments: `prev_tid`, `next_tid`, `prev_state`.
    SchedSwitch,
    /// A task was made runnable.
    ///
    /// Arguments: `tid`, `target_cpu`.
    SchedWakeup,
    /// A normal IRQ trap was entered.
    ///
    /// Arguments: `vector`.
    IrqEntry,
    /// A normal IRQ trap finished dispatching.
    ///
    /// Arguments: `vector`, `handled`.
    IrqExit,
    /// A syscall was dispatched from user space.
    ///
    /// Arguments: `sysno`, `arg0`, `arg1`, `arg2`.
    SyscallEntry,
    /// A syscall finished and its return value was published.
    ///
    /// Arguments: `sysno`, `retval`.
    SyscallExit,
    /// A user-space page fault was taken.
    ///
    /// Arguments: `fault_addr`, `flags`, `ip`.
    UserPageFault,
    /// User space was left through a trap.
    ///
    /// Arguments: `ip`, `sp`, `reason` (0 interrupt, 1 syscall, 2 page fault,
    /// 3 exception, 4 unknown).
    UserReturn,
    /// A user-space exception other than a syscall or page fault was taken.
    ///
    /// Arguments: `kind`, `ip`.
    UserException,
    /// A signal was about to run its user-space handler.
    ///
    /// Arguments: `signo`, `handler_ip`.
    SignalDeliver,
    /// A callback was queued on another CPU and an IPI raised for it.
    ///
    /// Arguments: `dest_cpu`, `src_cpu`.
    IpiSend,
    /// A queued IPI callback was about to run on the receiving CPU.
    ///
    /// Arguments: `src_cpu`.
    IpiHandle,
    /// The kernel entered its panic handler.
    Panic,
    /// Scratch event reserved for ad-hoc debugging.
    Debug0,
    /// Scratch event reserved for ad-hoc debugging.
    Debug1,
    /// Scratch event reserved for ad-hoc debugging.
    Debug2,
    /// Scratch event reserved for ad-hoc debugging.
    Debug3,
    /// Scratch event reserved for ad-hoc debugging.
    Debug4,
    /// Scratch event reserved for ad-hoc debugging.
    Debug5,
}

impl LcTraceEvent {
    /// Returns the event name as it appears in a rendered snapshot.
    pub const fn name(self) -> &'static str {
        EVENT_NAMES[self as usize]
    }
}

/// Resolves a raw discriminant read back from a record.
///
/// Returns `None` for a discriminant this build does not know, which can only
/// happen if a record is torn or the buffer is read by a mismatched tool.
pub(crate) fn event_name_of(event_id: u32) -> Option<&'static str> {
    EVENT_NAMES.get(event_id as usize).copied()
}
