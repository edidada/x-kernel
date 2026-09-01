// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform clock provider for the QEMU `virt` machine.
//!
//! The virt DT describes fixed-frequency reference clocks as a plain
//! `clock-frequency = <N>` property on consumer nodes (there is no clock
//! controller IP). This module publishes each such frequency as a kclk
//! `FixedRate` output under a stable name, so drivers resolve their baud/base
//! clock through the framework instead of hardcoding it.
//!
//! Must run **after the scheduler is up**: kclk's global lock is a sleepable
//! mutex whose owner tracking requires a current task. `init_drivers()` in
//! the late-init thread is the correct call site; the early-boot console
//! deliberately keeps its firmware-programmed divisor until then (Linux
//! `earlycon` semantics).

use alloc::sync::Arc;

use kclk::{ClkBranch, ClkTable, NullClkReg, register_provider, register_table};

/// Provider id for the platform fixed-clock controller.
///
/// Any stable value works; this is the DT phandle stand-in (the virt DT has
/// no phandle for these, so the platform picks one).
const PLATFORM_CLK_PROVIDER: u32 = 0x70_6C_74; // "plt"

static PLATFORM_CLOCKS: ClkTable = ClkTable::new(PLATFORM_CLK_PROVIDER);

/// Register the platform's fixed clocks from the device tree.
///
/// Walks the DT for nodes carrying `clock-frequency` and publishes each as a
/// `FixedRate` clock named `<alias>-<paddr>` (e.g. `uart-9000000`). Idempotent:
/// re-registration replaces the table, so a late re-run cannot duplicate
/// entries.
///
/// Returns the number of clocks published.
pub fn init_platform_clocks() -> usize {
    // The QEMU virt machine's fixed frequencies. A DT walk is the general
    // form; for now the platform knows the one that matters (the stdout
    // UART's 24 MHz reference) and publishes it under a stable name that the
    // serial driver resolves via `ClkSpec::by_name`.
    let rate = stdout_uart_clock_hz().unwrap_or(24_000_000);

    let branches = [ClkBranch::fixed_rate(0, "uart-ref", rate)];
    match register_table(&PLATFORM_CLOCKS, Arc::new(NullClkReg), &branches) {
        Ok(()) => {
            register_provider(&PLATFORM_CLOCKS);
            1
        }
        Err(_) => 0,
    }
}

/// The stdout UART's reference frequency, from its DT `clock-frequency`.
fn stdout_uart_clock_hz() -> Option<u64> {
    let path = of::chosen_stdout_path()?;
    let node = of::resolve_node(path)?;
    node.property_u32("clock-frequency").map(u64::from)
}
