// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Goldfish virtual RTC driver (`google,goldfish-rtc`), the QEMU virt
//! RISC-V / AArch64 battery-backed clock.

use alloc::sync::Arc;

use driver_base::{
    Device, DriverOps, IoMap, OfMatch, PlatformBus, PlatformId, ProbeError, Publication,
};
use kerrno::KResult;
use ktime_types::SystemTime;
use subsys_rtc::{RtcDevice, RtcDeviceImpl, class::RTC};

/// `static const struct of_device_id goldfish_rtc_of_match[]`.
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("google,goldfish-rtc"))];

/// The published RTC instance: the live MMIO base of the Goldfish block.
struct GoldfishRtc {
    vaddr: usize,
}

impl RtcDevice for GoldfishRtc {
    fn read_time(&self) -> KResult<SystemTime> {
        let rtc = riscv_goldfish::Rtc::new(self.vaddr);
        crate::common::system_time_from_unsigned_seconds(rtc.get_unix_timestamp())
    }
}

/// Driver-private data: publication owner plus the mapping guard.
pub struct GoldfishData {
    /// Dropping unpublishes. First field, before the mapping.
    _publication: Publication<RtcDeviceImpl>,
    _regs: IoMap,
}

/// The driver ops.
pub struct GoldfishDriver;

impl DriverOps<PlatformBus> for GoldfishDriver {
    type Data = GoldfishData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<GoldfishData, ProbeError> {
        let regs = dev.res().iomap(0)?;
        let device: RtcDeviceImpl = Arc::new(GoldfishRtc {
            vaddr: regs.as_ptr() as usize,
        });
        let publication = dev
            .publish(&RTC, device, "rtc-goldfish")
            .map_err(crate::common::map_driver_error)?;
        log::info!(
            "rtc-goldfish: published at {:#x} ({} bytes)",
            regs.region().base,
            regs.region().size
        );
        Ok(GoldfishData {
            _publication: publication,
            _regs: regs,
        })
    }
}

driver_base::declare_driver!(PlatformBus, GoldfishDriver, "rtc-goldfish", IDS);
