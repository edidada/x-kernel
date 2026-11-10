// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Loongson LS7A RTC driver.
//!
//! The counter protocol and its year-boundary consistency check live in
//! `ls7a_toy` (transport-agnostic, exercised by unit tests on every
//! architecture); this module owns discovery, the MMIO mapping and the RTC
//! class publication.
//!
//! Discovery: QEMU's LoongArch virt machine describes the block in the
//! device tree (`loongson,ls7a-rtc`); boards without a node are enumerated
//! by the composition root as a board-static `rtc-ls7a` device carrying the
//! aperture from `RTC_PADDR`.

use alloc::sync::Arc;

use driver_base::{
    AliasMatch, Device, DriverOps, IoMap, OfMatch, PlatformBus, PlatformId, ProbeError, Publication,
};
use kerrno::{KError, KResult};
use ktime_types::SystemTime;
use subsys_rtc::{RtcDevice, RtcDeviceImpl, class::RTC};

use crate::ls7a_toy::{RtcToyAccess, read_rtc_from};

/// `static const struct of_device_id ls7a_rtc_of_match[]` plus the
/// board-static alias the composition root enumerates when there is no node.
static IDS: &[PlatformId] = &[
    PlatformId::Of(OfMatch::compatible("loongson,ls7a-rtc")),
    PlatformId::Alias(AliasMatch::new("rtc-ls7a")),
];

const SYS_TOY_READ0: usize = 0x2C;
const SYS_TOY_READ1: usize = 0x30;
const SYS_RTCCTRL: usize = 0x40;

/// MMIO-backed register access; the production backend.
struct MmioToy {
    base: usize,
}

impl RtcToyAccess for MmioToy {
    fn write_ctrl(&mut self, val: u32) {
        // SAFETY: `base` is the kernel virtual address of the LS7A RTC
        // aperture, mapped and owned by the publishing driver's `IoMap`.
        // `SYS_RTCCTRL` is a 32-bit register offset within that mapped
        // region, and the volatile write models device-memory semantics,
        // not synchronization.
        unsafe {
            ((self.base + SYS_RTCCTRL) as *mut u32).write_volatile(val);
        }
    }

    fn read_toy_high(&mut self) -> u32 {
        // SAFETY: `SYS_TOY_READ1` is a 32-bit register offset within the
        // mapped LS7A RTC aperture; the volatile read models device-memory
        // semantics.
        unsafe { ((self.base + SYS_TOY_READ1) as *const u32).read_volatile() }
    }

    fn read_toy_low(&mut self) -> u32 {
        // SAFETY: `SYS_TOY_READ0` is a 32-bit register offset within the
        // mapped LS7A RTC aperture; the volatile read models device-memory
        // semantics.
        unsafe { ((self.base + SYS_TOY_READ0) as *const u32).read_volatile() }
    }
}

/// The published RTC instance: the live LS7A aperture base.
struct Ls7aRtc {
    base: usize,
}

impl RtcDevice for Ls7aRtc {
    fn read_time(&self) -> KResult<SystemTime> {
        let mut regs = MmioToy { base: self.base };
        read_rtc_from(&mut regs).ok_or(KError::Io)
    }
}

/// Driver-private data: publication owner plus the aperture mapping guard.
pub struct Ls7aData {
    /// Dropping unpublishes. First field, before the mapping.
    _publication: Publication<RtcDeviceImpl>,
    _regs: IoMap,
}

/// The driver ops.
pub struct Ls7aDriver;

impl DriverOps<PlatformBus> for Ls7aDriver {
    type Data = Ls7aData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<Ls7aData, ProbeError> {
        let regs = dev.res().iomap(0)?;
        let device: RtcDeviceImpl = Arc::new(Ls7aRtc {
            base: regs.as_ptr() as usize,
        });
        let publication = dev
            .publish(&RTC, device, "rtc-ls7a")
            .map_err(crate::common::map_driver_error)?;
        log::info!(
            "rtc-ls7a: published at {:#x} ({} bytes)",
            regs.region().base,
            regs.region().size
        );
        Ok(Ls7aData {
            _publication: publication,
            _regs: regs,
        })
    }
}

driver_base::declare_driver!(PlatformBus, Ls7aDriver, "rtc-ls7a", IDS);
