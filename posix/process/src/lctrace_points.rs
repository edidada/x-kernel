// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LCTrace probes on the user-thread trap loop.
//!
//! Each probe has a real body when the `lctrace` feature is on and an empty
//! one otherwise, so the trap loop stays free of `cfg` attributes.

#[cfg(feature = "lctrace")]
use khal::uspace::ReturnReason;

/// Numeric encodings of [`ReturnReason`] used in the `UserReturn` record.
///
/// The reason is an enum carrying payloads, so it cannot be cast directly.
/// The values are part of what a dump reader interprets, so they are named
/// here rather than written as bare literals at the probe site.
#[cfg(feature = "lctrace")]
mod return_reason_code {
    pub(super) const INTERRUPT: u64 = 0;
    pub(super) const SYSCALL: u64 = 1;
    pub(super) const PAGE_FAULT: u64 = 2;
    pub(super) const EXCEPTION: u64 = 3;
    pub(super) const UNKNOWN: u64 = 4;
}

#[cfg(feature = "lctrace")]
fn reason_code(reason: &ReturnReason) -> u64 {
    match reason {
        ReturnReason::Interrupt => return_reason_code::INTERRUPT,
        ReturnReason::Syscall => return_reason_code::SYSCALL,
        ReturnReason::PageFault(..) => return_reason_code::PAGE_FAULT,
        ReturnReason::Exception(..) => return_reason_code::EXCEPTION,
        ReturnReason::Unknown => return_reason_code::UNKNOWN,
    }
}

/// Records that user space was left, and why.
#[cfg(feature = "lctrace")]
#[inline]
pub(crate) fn user_return(ip: usize, sp: usize, reason: &ReturnReason) {
    lctrace::event!(UserReturn, ip, sp, reason_code(reason));
}

/// Records a user-space page fault before the address space resolves it.
#[cfg(feature = "lctrace")]
#[inline]
pub(crate) fn user_page_fault(fault_addr: usize, flags: u64, ip: usize) {
    lctrace::event!(UserPageFault, fault_addr, flags, ip);
}

/// Records a user-space exception that is neither a syscall nor a page fault.
#[cfg(feature = "lctrace")]
#[inline]
pub(crate) fn user_exception(kind: u64, ip: usize) {
    lctrace::event!(UserException, kind, ip);
}

/// Records a signal whose user-space handler is about to run.
#[cfg(feature = "lctrace")]
#[inline]
pub(crate) fn signal_deliver(signo: u32, handler_ip: usize) {
    lctrace::event!(SignalDeliver, signo, handler_ip);
}

#[cfg(not(feature = "lctrace"))]
#[inline]
pub(crate) fn user_return(_ip: usize, _sp: usize, _reason: &khal::uspace::ReturnReason) {}

#[cfg(not(feature = "lctrace"))]
#[inline]
pub(crate) fn user_page_fault(_fault_addr: usize, _flags: u64, _ip: usize) {}

#[cfg(not(feature = "lctrace"))]
#[inline]
pub(crate) fn user_exception(_kind: u64, _ip: usize) {}

#[cfg(not(feature = "lctrace"))]
#[inline]
pub(crate) fn signal_deliver(_signo: u32, _handler_ip: usize) {}
