// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common clock framework core for x-kernel.
//!
//! This crate is the in-kernel analogue of the Linux `drivers/clk/clk.c` core
//! plus the basic reusable clock types. It provides:
//!
//! - [`ClkHwOps`], the provider-side hardware operation trait (the `clk_ops`
//!   equivalent), including the two-phase `prepare`/`enable` gate.
//! - [`ClkCore`], a clock-tree node that caches rate/accuracy and serializes
//!   reference-counted gating and rate/parent changes that propagate to the
//!   selected parent.
//! - [`ClkLookup`] / [`register_provider`] / [`ClkTable`], the global provider
//!   registry that consumers query via [`clk_get_by_name`] /
//!   [`clk_get_by_index`] (by `(provider_id, index)`).
//! - [`ClkHandle`], the consumer-side reference returned to drivers.
//! - Basic composable types in [`basic`]: [`FixedRate`], [`FixedFactor`],
//!   [`Gate`], [`Mux`], [`Divider`], [`Composite`].
//!
//! Concrete clock-controller drivers (SoC PLL/mux/divider chips) register their
//! outputs here, mirroring how the [`device_res`] capability providers are
//! wired into the kernel.
//!
//! [`device_res`]: device_res
#![cfg_attr(not(test), no_std)]
#![warn(missing_docs)]

extern crate alloc;

pub mod basic;

mod core;
mod error;
mod event;
mod flags;
mod handle;
mod lookup;
mod ops;
mod provider;
mod rate;
mod reg;

#[cfg(unittest)]
mod tests;

pub use core::{ClkBuilder, ClkCore, clk_disable_unused};

pub use basic::{Composite, Divider, DividerFlags, FixedFactor, FixedRate, Gate, GateFlags, Mux};
pub use error::{ClkError, ClkResult};
pub use event::{ClkEvent, ClkNotifier, ClkNotifierId, NotifierResult};
pub use flags::ClkFlags;
pub use handle::ClkHandle;
pub use lookup::ClkTable;
pub use ops::ClkHwOps;
#[cfg(unittest)]
pub use provider::reset_providers;
pub use provider::{
    ClkLookup, ProviderId, clk_get_by_index, clk_get_by_name, provider_installed,
    register_provider, unregister_provider,
};
pub use rate::{ClkRateRequest, Rate, RoundResult};
pub use reg::{ClkRegAccess, MmioClkReg};

/// Index into a provider's clock table.
///
/// Corresponds to the cell value in a DT `clocks = <&phandle N>` specifier.
pub type ClkIndex = u32;

/// Host-only verification of rate propagation (standard `#[test]`, collected by
/// `cargo test`). Temporary — confirms the SET_RATE_PARENT fix without QEMU.
#[cfg(test)]
mod host_verify {
    extern crate alloc;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicU32 as A32, Ordering};

    use crate::{
        ClkBuilder, ClkFlags, ClkRegAccess, Divider, DividerFlags, FixedFactor, FixedRate, Mux,
    };

    struct MockReg {
        mem: [A32; 8],
    }
    impl MockReg {
        fn new() -> Self {
            Self {
                mem: [const { A32::new(0) }; 8],
            }
        }
    }
    impl ClkRegAccess for MockReg {
        fn reg_read(&self, o: usize) -> u32 {
            self.mem[o / 4].load(Ordering::Relaxed)
        }

        fn reg_write(&self, o: usize, v: u32) {
            self.mem[o / 4].store(v, Ordering::Relaxed)
        }
    }

    #[test]
    fn fixed_factor_propagates_via_flag() {
        let reg = Arc::new(MockReg::new());
        let root = ClkBuilder::new("root", FixedRate::new(1_000_000_000).into_ops())
            .build()
            .unwrap();
        let div = ClkBuilder::new(
            "div",
            Divider::into_ops(reg, 0, 0, 4, None, DividerFlags::ONE_BASED),
        )
        .parent(root)
        .build()
        .unwrap();
        let ff = ClkBuilder::new("ff", FixedFactor::new(1, 2).unwrap().into_ops())
            .flags(ClkFlags::SET_RATE_PARENT)
            .parent(div.clone())
            .build()
            .unwrap();
        let actual = ff.set_rate(100_000_000).unwrap();
        assert_eq!(actual, 100_000_000);
        assert_eq!(ff.get_rate(), 100_000_000);
        assert_eq!(div.get_rate(), 200_000_000);
    }

    #[test]
    fn mux_passthrough_propagates_via_flag() {
        let reg = Arc::new(MockReg::new());
        let root = ClkBuilder::new("root", FixedRate::new(1_000_000_000).into_ops())
            .build()
            .unwrap();
        let div = ClkBuilder::new(
            "div",
            Divider::into_ops(reg, 0, 0, 4, None, DividerFlags::ONE_BASED),
        )
        .parent(root)
        .build()
        .unwrap();
        let mux = Mux::into_ops(Arc::new(MockReg::new()), 0, 0, 0x1, None);
        let passthrough = ClkBuilder::new("pass", mux)
            .flags(ClkFlags::SET_RATE_PARENT)
            .parent(div.clone())
            .build()
            .unwrap();
        let actual = passthrough.set_rate(200_000_000).unwrap();
        assert_eq!(actual, 200_000_000);
        assert_eq!(passthrough.get_rate(), 200_000_000);
    }

    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
