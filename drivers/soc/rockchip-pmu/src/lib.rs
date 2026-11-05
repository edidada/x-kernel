// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn>
// See LICENSES for license details.

#![no_std]

//! Shared access to the RK3588 PMU power-domain registers (`0xfd8d8000`).
//!
//! The reference drives domains through `drivers/pmdomain/rockchip/
//! pm-domains.c`, where every transition runs inside `pmu->mutex`, the
//! transition itself early-returns when the domain already matches the
//! requested state, and the domain's clocks are held on across the window.
//! Two drivers here (PCIe and USB) sit in domains of the same PMU block;
//! without a shared lock, one driver's transition can interleave with the
//! other's status reads, and the reference's `rockchip_pd_power()` exists in
//! exactly that shape to prevent it.

use driver_base::IoMap;
use kerrno::{KError, KResult};
use khal::time::busy_wait;
use ksync::Mutex;
use ktime_types::TimeSpan;

/// PMU register window (`pm-domains.c`'s `pmu->regs`), shared by every domain.
const PMU_BASE: u64 = 0xfd8d_8000;
const PMU_SIZE: usize = 0x400;

/// `PD_PWR_CON` (`pwr_offset 0x14c` + domain offset 0x4): one hiword-written
/// bit per domain, set = powered.
const PD_PWR_CON: usize = 0x150;
/// `PD_MEM_PWR` for the same domains.
const PD_MEM_PWR: usize = 0x1a4;
/// `repair_status_offset` (0x290): bit set means powered for domains with a
/// `repair_status_mask` (PD_USB has BIT(25), PD_PCIE has BIT(22)).
const PD_REPAIR_STATUS: usize = 0x290;

/// Domain 34 (`RK3588_PD_PCIE`, pwr BIT(7), repair BIT(22)).
pub const PD_PCIE_BIT: u32 = 1 << 7;
pub const PD_PCIE_REPAIR_BIT: u32 = 1 << 22;
/// Domain 31 (`RK3588_PD_USB`, pwr BIT(11), repair BIT(25)).
pub const PD_USB_BIT: u32 = 1 << 11;
pub const PD_USB_REPAIR_BIT: u32 = 1 << 25;

/// The one PMU window, mapped by the first caller and kept for the kernel's
/// lifetime (domain registers live as long as the kernel does).
static PMU: Mutex<Option<IoMap>> = Mutex::new(None);

fn with_pmu<T>(f: impl FnOnce(&IoMap) -> T) -> Option<T> {
    let guard = PMU.lock();
    guard.as_ref().map(f)
}

/// Attaches the shared PMU window. Idempotent: the first caller's mapping is
/// kept, later calls are no-ops (the reference keeps `pmu->regs` for the
/// lifetime of the PMU device).
/// Attaches the PMU window through a device's resource services.
pub fn attach_via(
    dev: &driver_base::Device<'_, driver_base::PlatformBus>,
) -> Result<(), kerrno::KError> {
    let map = dev
        .res()
        .iomap_region(
            driver_base::MmioRegion::new(PMU_BASE as usize, PMU_SIZE),
            "rk-pmu-domains",
        )
        .map_err(|_| kerrno::KError::from(kerrno::KErrorKind::Io))?;
    // The mapping is kept for the kernel's lifetime -- drivers are never
    // unbound here, and the reference's `pmu->regs` lives as long as the PMU
    // device (the same `core::mem::forget` the PCIe clock handles make).
    attach(map);
    // The mapping is owned by the static now; `core::mem::forget` is implicit
    // because `attach` takes ownership and never drops it while the static
    // lives.
    Ok(())
}

pub fn attach(map: IoMap) {
    let mut guard = PMU.lock();
    if guard.is_none() {
        *guard = Some(map);
    }
}

/// Reports whether the domain reads as powered.
///
/// `rockchip_pmu_domain_is_on()`: domains with a `repair_status_mask` read
/// `PD_REPAIR_STATUS` (bit set = powered); the power bit is the fallback.
fn is_on(pmu: &IoMap, _bit: u32, repair_bit: u32) -> bool {
    pmu.read32(PD_REPAIR_STATUS) & repair_bit != 0
}

/// Powers a domain on, exactly once per state change.
///
/// `rockchip_pd_power()`: the sequence sits in the PMU mutex, a domain already
/// in the requested state is a no-op, and the enable write is polled until the
/// power bit acknowledges. The early return is what keeps a second driver's
/// probe from touching the PMU while the first owns a transition.
pub fn power_domain_on(name: &str, bit: u32, repair_bit: u32) -> KResult {
    {
        let guard = PMU.lock();
        let Some(pmu) = guard.as_ref() else {
            log::error!("rk-pmu: PMU window not attached");
            return Err(KError::Io);
        };
        if is_on(pmu, bit, repair_bit) {
            return Ok(());
        }
        // Hiword write: write-enable the bit in the high half, set it in the
        // low half (`pwr_w_mask` alone, per the reference's polarity).
        pmu.write32(PD_PWR_CON, (bit << 16) | bit);
    }
    for _ in 0..10_000 {
        let done = {
            let guard = PMU.lock();
            let Some(pmu) = guard.as_ref() else {
                return Err(KError::Io);
            };
            is_on(pmu, bit, repair_bit)
        };
        if done {
            log::info!("rk-pmu: domain {name} powered on");
            return Ok(());
        }
        busy_wait(TimeSpan::from_micros(100));
    }
    let (pwr, repair) =
        with_pmu(|pmu| (pmu.read32(PD_PWR_CON), pmu.read32(PD_REPAIR_STATUS))).unwrap_or((0, 0));
    log::error!(
        "rk-pmu: domain {name} failed to power on (pwr_con {pwr:#010x} repair {repair:#010x})"
    );
    Err(KError::Io)
}

/// Reads the domain's state words for diagnostics, under the PMU mutex.
pub fn state_words() -> Option<(u32, u32, u32)> {
    with_pmu(|pmu| {
        (
            pmu.read32(PD_PWR_CON),
            pmu.read32(PD_MEM_PWR),
            pmu.read32(PD_REPAIR_STATUS),
        )
    })
}
