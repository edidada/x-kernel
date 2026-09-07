// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RK3588 Clock and Reset Unit (CRU) driver — the first real SoC clock
//! controller on x-kernel, mechanically translated from the pinned Linux
//! reference (see `docs/ai/skills/linux-driver-port/examples/rk3588-cru/`).
//!
//! The controller is a normal platform device: the DT node
//! `compatible = "rockchip,rk3588-cru"` is enumerated by the platform bus,
//! probed before its consumers (provider-first ordering), and probe registers
//! the whole clock tree into `kclk` under the node's DT phandle so
//! `clocks = <&cru N>` specifiers resolve board-wide.
//!
//! Translation domains (see `data.rs`):
//! - register layout, bits, parents, sequences: Linux `clk-rk3588.c` +
//!   `clk-pll.c` + `clk-cpu.c` + `clk.c` (reference tree `linux`);
//! - DT binding indices: the OrangePi 5 Plus BSP header (reference tree
//!   `bsp-opi5plus`) — the numbering the board DT was compiled with.

use alloc::{boxed::Box, format, sync::Arc, vec::Vec};

use device_res::AssignedClock;
use driver_base::{DeviceKind, DriverError, DriverResult};
use kclk::{ClkBranch, ClkError, ClkIndex, ClkRegAccess, ClkTable, MmioClkReg, MuxFlags};
use kdevice::{
    BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject, FirmwareMatchSpec, MatchResult, priority,
};

mod data;
mod ops;

/// One register field (offset/shift/mask) of a custom clock node.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Field {
    pub offset: usize,
    pub shift: u8,
    pub mask: u32,
}

/// The exposed child mux of a fractional-divider branch (Linux
/// `rockchip_clk_register_frac_branch`'s `child`).
pub(crate) struct FracChild {
    /// DT binding index of the child mux.
    pub index: ClkIndex,
    /// Child mux clock name (e.g. `clk_uart2`).
    pub name: &'static str,
    /// The child mux's parent list (the reference PNAME array).
    pub parents: &'static [&'static str],
    /// Parent-select register field.
    pub mux_offset: usize,
    pub mux_shift: u8,
    pub mux_mask: u32,
    /// Position of the fractional clock in the child mux's parent list — the
    /// notch the remux dance temporarily selects while the fraction changes.
    pub frac_parent_index: usize,
}

pub(crate) enum CustomKind {
    /// Mux + half divider + gate composite (Linux `clk-half-divider.c`).
    HalfDiv,
    /// Fractional divider (+ gate) with an exposed child mux (Linux
    /// `rockchip_clk_register_frac_branch`).
    Frac,
    /// Gate whose linked interconnect clock must stay enabled (Linux
    /// `GATE_LINK` + `gate-link.c`; x-kernel has no runtime PM, so probe
    /// holds the link clock enabled for the controller's lifetime).
    GateLink,
}

/// One runtime-built node of the CRU clock tree.
pub(crate) struct CustomSpec {
    pub kind: CustomKind,
    pub name: &'static str,
    pub index: Option<ClkIndex>,
    pub parents: &'static [&'static str],
    pub mux: Option<Field>,
    pub div: Option<Field>,
    pub gate: Option<Field>,
    pub child: Option<FracChild>,
    pub link_clock: Option<&'static str>,
    pub flags: kclk::ClkFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PllKind {
    /// `pll_rk3588_core` — the mode mux is never switched by software (the
    /// boot CPU and other always-on consumers hang off these PLLs).
    Core,
    /// `pll_rk3588` — set-rate temporarily selects the slow parent.
    Normal,
}

pub(crate) struct PllSpec {
    /// DT binding index of the mode mux (the exported clock, e.g. `gpll`).
    pub index: ClkIndex,
    /// Exported clock name (the mode mux), e.g. `gpll`.
    pub name: &'static str,
    /// Internal PLL node name, e.g. `pll_gpll`.
    pub pll_name: &'static str,
    pub kind: PllKind,
    /// PLL parameter register block (CON0..CON6) byte offset.
    pub con_offset: usize,
    /// Mode (parent-select) register byte offset.
    pub mode_offset: usize,
    /// Mode field LSB.
    pub mode_shift: u8,
    /// Branch flags from the reference PLL table.
    pub flags: kclk::ClkFlags,
    /// Mode mux reference parents from the PLL table: `[slow, deep]`; the
    /// middle parent (the pll output) is inserted between them.
    pub parents: &'static [&'static str],
}

pub(crate) struct PllRate {
    /// VCO output rate in Hz.
    pub rate: u64,
    /// Post divider (p), feedback (m), output divide exponent (s), fractional
    /// numerator over 65535 (k) — the RK3588 PLL parameter set.
    pub p: u32,
    pub m: u32,
    pub s: u32,
    pub k: u32,
}

/// DT binding index range reserved for name-only nodes (the internal `pll_*`
/// nodes). Nothing in the board DT references them; they only have to be
/// unique within this provider.
const INTERNAL_INDEX_BASE: ClkIndex = 900;

/// Fixed provider id for the external orphan placeholders. 0 is not a valid
/// DT phandle, so it can never collide with a real controller node.
const ORPHAN_PROVIDER_ID: u32 = 0;

/// Match the RK3588 CRU node.
struct CruMatcher;

static CRU_MATCH: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "clk-rk3588",
    dt_compatibles: &["rockchip,rk3588-cru"],
    acpi_ids: &[],
};

impl DeviceMatcher for CruMatcher {
    fn matches(&self, desc: &kdevice::DeviceDesc) -> MatchResult {
        if let kdevice::DeviceIdentity::Platform(plat) = desc.identity()
            && let Some(id) = plat.firmware_id
            && CRU_MATCH.match_dt(id).is_some()
        {
            return MatchResult::Match {
                priority: priority::EXACT,
            };
        }
        MatchResult::NoMatch
    }

    fn firmware_spec(&self) -> Option<&FirmwareMatchSpec> {
        Some(&CRU_MATCH)
    }
}

static MATCHER: CruMatcher = CruMatcher;

/// RK3588 CRU platform driver.
pub struct Rk3588CruDriver;

impl DeviceDriver for Rk3588CruDriver {
    fn name(&self) -> &'static str {
        "clk-rk3588"
    }

    fn device_kind(&self) -> DeviceKind {
        // A clock controller is infrastructure, not a char/block/net device.
        DeviceKind::Bus
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &MATCHER
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        let (vaddr, _) = crate::iomap_first_mmio(device.as_ref(), "clk-rk3588")?;
        let vaddr = vaddr.as_ptr() as usize;

        let provider = device.clk_provider().ok_or_else(|| {
            log::error!("clk-rk3588: CRU node carries no provider id (phandle)");
            DriverError::InvalidInput
        })?;
        let phandle = provider.provider_id();

        // SAFETY: `iomap_first_mmio` returned a kernel virtual address mapping
        // the CRU's first (and only) MMIO region described by firmware and
        // installed in `memspace`. Clock controllers are boot-static: the
        // device is never removed and the registration below outlives it, so
        // the window is live for the lifetime of every registered node.
        let reg: Arc<dyn ClkRegAccess> = unsafe { Arc::new(MmioClkReg::new(vaddr)) };

        register_placeholders();
        register_tree(phandle, reg, &device)?;

        log::info!("clk-rk3588: provider 0x{phandle:x} ready; {}", summarize());
        Ok(())
    }
}

/// Publish zero-rate placeholders for external input clocks that no provider
/// in this kernel implements yet (PHY outputs, pad inputs, GMAC references).
/// Linux registers those branches as orphans; a rate-0 node is the same
/// honest statement ("nothing drives this yet") inside a framework that
/// refuses half-linked trees. Consumers of their subtrees see rate 0, never a
/// fabricated value, and a future provider simply takes the names over.
fn register_placeholders() {
    static DONE: kspin::SpinNoIrq<bool> = kspin::SpinNoIrq::new(false);
    {
        let mut done = DONE.lock();
        if *done {
            return;
        }
        *done = true;
    }

    let table: &'static ClkTable = Box::leak(Box::new(ClkTable::new(ORPHAN_PROVIDER_ID)));
    let reg: Arc<dyn ClkRegAccess> = Arc::new(kclk::NullClkReg);
    let branches: Vec<ClkBranch> = data::ORPHAN_PARENTS
        .iter()
        .enumerate()
        .map(|(i, name)| ClkBranch::fixed_rate(INTERNAL_INDEX_BASE + i as ClkIndex, name, 0))
        .collect();
    if let Err(err) = kclk::register_table(table, reg, &branches) {
        log::error!("clk-rk3588: orphan placeholder registration failed: {err:?}");
        return;
    }
    kclk::register_provider(table);
}

/// Register the whole CRU clock tree: PLLs (+ mode muxes), armclks, the
/// generated branch table, fractional branches, then apply the
/// firmware-directed initial rates.
fn register_tree(
    phandle: u32,
    reg: Arc<dyn ClkRegAccess>,
    device: &DeviceObject,
) -> DriverResult<()> {
    let table: &'static ClkTable = Box::leak(Box::new(ClkTable::new(phandle)));

    // Runtime-built branches: customs + PLL nodes + armclks (the static
    // BRANCHES table carries no `Custom` entries and stays const).
    let mut branches: Vec<ClkBranch> =
        Vec::with_capacity(data::BRANCHES.len() + data::CUSTOM_COUNT + 12);
    branches.extend_from_slice(&data::BRANCHES);

    let mut deferred_links: Vec<(&'static str, &'static str)> = Vec::new();
    // (frac node DT index, child mux descriptor) — the notifier hook needs
    // the FRACTION node's index; the child carries only its own (mux) index.
    let mut fracs: Vec<(ClkIndex, &'static FracChild)> = Vec::new();

    // -- PLLs: internal pll node + exported mode mux ------------------------
    let mut internal_index = INTERNAL_INDEX_BASE + data::ORPHAN_PARENTS.len() as ClkIndex;
    for spec in &data::PLLS {
        let pll = Arc::new(ops::Rk3588Pll::new(spec, reg.clone()));
        // Linux's pll node has exactly one parent: parents[0] (xin24m).
        let pll_parent: &'static [&'static str] = Box::leak(Box::new([spec.parents[0]]));
        // Linux's mode-mux parents are `[p0, pll_<name>, p1]`; the middle
        // parent is the internal PLL node registered in this same batch.
        let mux_parents: &'static [&'static str] =
            Box::leak(Box::new([spec.parents[0], spec.pll_name, spec.parents[1]]));
        let mode_table: &'static [u8] = &[0, 1, 2]; // slow=0, norm=1, deep=2
        branches.push(
            ClkBranch::custom(internal_index, spec.pll_name, pll, pll_parent, spec.flags)
                .with_flags(kclk::ClkFlags::IGNORE_UNUSED),
        );
        branches.push(
            ClkBranch::mux(
                spec.index,
                spec.name,
                spec.mode_offset,
                spec.mode_shift,
                0x3,
                Some(mode_table),
                MuxFlags::HIWORD_MASK,
                mux_parents,
            )
            .with_flags(kclk::ClkFlags::SET_RATE_PARENT | kclk::ClkFlags::NO_REPARENT),
        );
        internal_index += 1;
    }

    // -- customs: fractional branches + half dividers -----------------------
    for spec in &data::CUSTOMS {
        match spec.kind {
            CustomKind::HalfDiv => {
                let ops = ops::HalfDivComposite::new(spec, reg.clone());
                let index = spec.index.ok_or_else(bad_custom)?;
                branches.push(ClkBranch::custom(
                    index,
                    spec.name,
                    ops,
                    spec.parents,
                    spec.flags,
                ));
            }
            CustomKind::Frac => {
                let child = spec.child.as_ref().ok_or_else(bad_custom)?;
                let (frac_ops, mux_ops) = ops::FracBranch::new(spec, reg.clone());
                branches.push(ClkBranch::custom(
                    spec.index.ok_or_else(bad_custom)?,
                    spec.name,
                    frac_ops,
                    spec.parents,
                    spec.flags | kclk::ClkFlags::SET_RATE_PARENT,
                ));
                branches.push(ClkBranch::custom(
                    child.index,
                    child.name,
                    mux_ops,
                    child.parents,
                    kclk::ClkFlags::SET_RATE_PARENT,
                ));
                fracs.push((spec.index.ok_or_else(bad_custom)?, child));
            }
            CustomKind::GateLink => {
                // The gate itself is in the static table; only the link
                // enable is deferred until the tree is registered.
                let link = spec.link_clock.ok_or_else(bad_custom)?;
                deferred_links.push((spec.name, link));
            }
        }
    }

    // -- armclks ------------------------------------------------------------
    let armclks = ops::ArmClk::build_all(reg.clone(), &mut branches, internal_index);

    if let Err(err) = kclk::register_table(table, reg, &branches) {
        log::error!("clk-rk3588: clock tree registration failed: {err:?}");
        return Err(map_clk_error(err));
    }

    // Publish the provider ONLY after the whole batch succeeded, so no
    // consumer can observe a half-built tree (Linux of_clk_add_provider
    // ordering; the post-registration wiring below resolves through it).
    kclk::register_provider(table);

    // Post-registration wiring that needs live nodes (public handle API).
    ops::ArmClk::post_register(&armclks);
    ops::FracBranch::wire_notifiers(phandle, &fracs);

    // Gate links: hold each linked interconnect clock enabled for the
    // controller's lifetime (Linux: pm_clk reference in gate-link.c).
    for (gate, link_clock) in &deferred_links {
        let enabled = kclk::clk_get_by_name(link_clock).and_then(|clk| clk.prepare_enable());
        if let Err(err) = enabled {
            // Observable failure path (bring-up rule: never a bare Io).
            log::error!("clk-rk3588: link clock {link_clock} for gate {gate}: {err:?}");
        }
    }

    apply_assigned_clocks(device);
    Ok(())
}

fn bad_custom() -> DriverError {
    log::error!("clk-rk3588: malformed custom clock spec (see data.rs)");
    DriverError::InvalidInput
}

fn apply_assigned_clocks(device: &DeviceObject) {
    for assigned in device.assigned_clocks() {
        let AssignedClock {
            provider,
            index,
            rate_hz,
        } = assigned;
        if rate_hz == 0 {
            continue;
        }
        match kclk::clk_get_by_index(provider, index) {
            Ok(clk) => {
                // Same-value requests short-circuit in the core, so assigned
                // rates the bootloader already applied never touch hardware.
                if let Err(err) = clk.set_rate(rate_hz) {
                    log::warn!(
                        "clk-rk3588: assigned rate {rate_hz} for <&{provider:#x} {index}>: {err:?}"
                    );
                }
            }
            Err(err) => log::warn!(
                "clk-rk3588: assigned clock <&{provider:#x} {index}> unresolved: {err:?}"
            ),
        }
    }
}

/// Rates of the clocks a broken bring-up looks at first (reference: the
/// OrangePi 5 Plus BSP expects gpll 1188, cpll 786.432, ppll 850/1100 per DT
/// assignment, uart2 source from CLKSEL_CON(45)).
fn summarize() -> alloc::string::String {
    const NAMES: &[&str] = &[
        "gpll",
        "cpll",
        "npll",
        "ppll",
        "aclk_top_root",
        "pclk_top_root",
        "sclk_uart2",
    ];
    let mut out = alloc::string::String::new();
    for name in NAMES {
        match kclk::clk_get_by_name(name) {
            Ok(clk) => out.push_str(&format!("{name}={}Hz ", clk.get_rate())),
            Err(_) => out.push_str(&format!("{name}=? ")),
        }
    }
    out
}

fn map_clk_error(err: ClkError) -> DriverError {
    match err {
        ClkError::NoProvider => DriverError::WouldBlock,
        ClkError::Io => DriverError::Io,
        ClkError::Busy | ClkError::Aborted => DriverError::ResourceBusy,
        ClkError::NotSupported => DriverError::Unsupported,
        ClkError::NotFound
        | ClkError::InvalidRate
        | ClkError::InvalidParent
        | ClkError::BadState => DriverError::InvalidInput,
    }
}

/// Factory for the built-in driver registry.
pub fn rk3588_cru_descriptor() -> crate::driver_registry::BoxedDriver {
    Arc::new(Rk3588CruDriver)
}
