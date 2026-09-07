// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lightweight architecture-specific low-level operations.
//!
//! This crate provides a uniform API across all supported architectures for
//! TLB flush, cache maintenance, CPU halt, interrupt management, thread pointer
//! access, and FP/SIMD enable operations.
#![doc = include_str!("../README.md")]
#![no_std]

/// `kiface` for cross-PE pipeline synchronisation.
///
/// SMP builds use the provider implemented by `kipi`; uniprocessor builds use
/// the local no-op provider below because there are no remote PEs to flush.
#[kiface::interface]
pub trait IcacheFlushIf {
    fn flush_others();
}

#[cfg(not(feature = "smp"))]
#[kiface::provide]
impl IcacheFlushIf {
    fn flush_others() {}
}

/// Sends IPIs to all other PEs requesting they flush their instruction pipeline.
#[inline]
#[allow(dead_code)]
pub(crate) fn flush_icache_others() {
    crate::IcacheFlushIf::flush_others();
}

/// Masks local interrupt delivery before the final CPU stop protocol.
///
/// This is a one-way operation: callers must remain pinned and enter a power
/// terminal or [`stop_cpu`] without restoring interrupts. It only accesses
/// CPU-local registers and never waits for another CPU or an IRQ-control lock.
/// AArch64 masks DAIF and, when active, hardware-NMI ALLINT. Architectures
/// without a CPU-level NMI mask still require the owner to stop local NMI
/// sources before acknowledging that this CPU has parked.
#[inline]
pub fn prepare_cpu_stop() {
    #[cfg(target_arch = "aarch64")]
    aarch64::disable_local_exceptions();
    #[cfg(not(target_arch = "aarch64"))]
    disable_local_irq();
    // Keep subsequent terminal state publication after the CPU mask change.
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        mod x86_64;
        pub use self::x86_64::*;
    } else if #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))] {
        mod riscv;
        pub use self::riscv::*;
    } else if #[cfg(target_arch = "aarch64")] {
        mod aarch64;
        pub use self::aarch64::*;
    } else if #[cfg(target_arch = "loongarch64")] {
        mod loongarch64;
        pub use self::loongarch64::*;
    } else if #[cfg(target_arch = "arm")] {
        mod arm;
        pub use self::arm::*;
    }
}
