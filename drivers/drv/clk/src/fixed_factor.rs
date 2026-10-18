// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `fixed-factor` clock driver — the transcription of Linux
//! `drivers/clk/clk-fixed-factor.c` onto the driver model.
//!
//! It is the second provider driver, and deliberately a *consumer and a
//! provider in one probe*:
//!
//! ```text
//! dev.res().clk("parent")            // -EPROBE_DEFER until apb-pclk publishes
//!   → rate = parent_rate * mult / div
//! dev.env().provide::<ClkProvider>(PROVIDER_ID, lookup)?   // publish LAST
//! ```
//!
//! # Why the node is board-static
//!
//! QEMU `virt` exposes exactly one clock node (`apb-pclk`, a `fixed-clock` at
//! phandle `0x8000`), already claimed by `fixed_clock`. A derived-clock DT node
//! would need a patched device tree, so this device is enumerated by the
//! composition root as a board-static device (alias `clk48m`) and the
//! multiplier/divisor come from the board table below — the same place Linux's
//! `fixed-factor-clock` gets `clock-mult`/`clock-div` from its node.
//!
//! The parent edge is a real firmware reference: the descriptor carries
//! `clocks = <&0x8000 0>`, and `dev.res().clk("parent")` resolves it through
//! the host clock registry.
//!
//! Known limit (documented, not hidden): `kclk`'s `FixedRate` does not track
//! parent rate changes, so the derived rate is sampled once at probe. Linux
//! `clk-fixed-factor` re-evaluates on parent change.

use alloc::{boxed::Box, sync::Arc};

use driver_base::{
    AliasMatch, Device, DriverOps, PlatformBus, PlatformId, ProbeError, RuntimeError,
};
use kclk::{ClkBuilder, ClkCore, ClkLookup, FixedRate, ProviderId};

/// Board alias of the derived-clock device (the composition root enumerates
/// it; board-static devices are claimed by alias, never by compatible).
static IDS: &[PlatformId] = &[PlatformId::Alias(AliasMatch::new("clk48m"))];

/// The synthetic phandle this driver publishes under. It is NOT in QEMU's DT:
/// consumers of the derived clock reference this value.
const PROVIDER_ID: ProviderId = 0xC1C0_0001;

/// Board configuration for the derived output (Linux reads the same two
/// values from `clock-mult` / `clock-div`). The parent edge itself is a
/// descriptor resource (`clocks`), not board config: `dev.res().clk("parent")`
/// resolves it.
struct FixedFactorConfig {
    mult: u64,
    div: u64,
    name: &'static str,
}

static CONFIG: FixedFactorConfig = FixedFactorConfig {
    mult: 2,
    div: 1,
    name: "clk48m",
};

struct FixedFactorLookup {
    provider_id: ProviderId,
    core: Arc<ClkCore>,
}

impl ClkLookup for FixedFactorLookup {
    fn provider_id(&self) -> ProviderId {
        self.provider_id
    }

    fn get_by_index(&self, index: kclk::ClkIndex) -> Option<Arc<ClkCore>> {
        (index == 0).then(|| self.core.clone())
    }

    fn get_by_name(&self, name: &str) -> Option<Arc<ClkCore>> {
        let _ = name;
        None
    }
}

/// Driver-private data (drvdata): the parent handle and the derived clock.
///
/// Field order is the release order: the parent reference (acquired first)
/// goes last, the clock tree it fed goes first.
pub struct FixedFactorData {
    _core: Arc<ClkCore>,
    _parent: driver_base::ClkHandle,
}

pub struct FixedFactorDriver;

impl DriverOps<PlatformBus> for FixedFactorDriver {
    type Data = FixedFactorData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<FixedFactorData, ProbeError> {
        // Consumer half: the parent is firmware-referenced (`clocks =
        // <&0x8000 0>` in the descriptor). While its provider is missing this
        // returns Deferred and the domain parks the device.
        let parent = dev.res().clk("parent")?;
        let parent_rate = parent.get_rate();
        if CONFIG.div == 0 {
            return Err(ProbeError::Fatal(RuntimeError::Invalid));
        }
        let rate = parent_rate / CONFIG.div * CONFIG.mult;

        let core = ClkBuilder::new(CONFIG.name, FixedRate::new(rate).into_ops())
            .build()
            .map_err(|_| ProbeError::Fatal(RuntimeError::Io))
            .inspect_err(|_| log::warn!("fixed-factor: builder failed for `{}`", CONFIG.name))?;

        // Provider half: the parent clock id and the derived output's id must
        // agree with what consumers name; `provide` cross-checks key vs lookup.
        let lookup: &'static FixedFactorLookup = Box::leak(Box::new(FixedFactorLookup {
            provider_id: PROVIDER_ID,
            core: core.clone(),
        }));
        // Publish LAST: registration wakes consumers parked on this phandle
        // and cannot be undone by a later `Err`.
        dev.env()
            .provide::<driver_host::ClkProvider>(PROVIDER_ID, lookup)?;

        log::info!(
            "fixed-factor: published `{}` @ {rate} Hz (parent {parent_rate} Hz, {}/{})",
            CONFIG.name,
            CONFIG.mult,
            CONFIG.div
        );
        Ok(FixedFactorData {
            _core: core,
            _parent: parent,
        })
    }

    fn remove(&self, _dev: &mut Device<'_, PlatformBus>, _data: &mut FixedFactorData) {
        // Clocks are boot-static (kclk model): the published lookup outlives
        // the device, exactly like `fixed_clock`.
    }
}

driver_base::declare_driver!(PlatformBus, FixedFactorDriver, "fixed-factor-clk", IDS);
