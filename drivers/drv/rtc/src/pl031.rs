// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ARM PrimeCell PL031 RTC driver (the Linux `rtc-pl031.c` shape).
//!
//! DT discovery matches the specific `arm,pl031` compatible first; nodes
//! that describe the block as a generic `arm,primecell` are claimed only
//! after the periphid register identifies a PL031, so the PL011 UART and
//! other PrimeCell devices keep flowing to their own drivers.

use alloc::sync::Arc;

use driver_base::{
    Device, DriverOps, IoMap, OfMatch, PlatformBus, PlatformId, ProbeError, Publication,
    RuntimeError,
};
use kerrno::KResult;
use ktime_types::SystemTime;
use subsys_rtc::{RtcDevice, RtcDeviceImpl, class::RTC};

/// `static const struct of_device_id pl031_of_match[]`.
static IDS: &[PlatformId] = &[
    PlatformId::Of(OfMatch::compatible("arm,pl031")),
    PlatformId::Of(OfMatch::compatible("arm,primecell")),
];

/// PL031 peripheral-ID value (PrimeCell identification).
const PL031_PRIMECELL_PERIPHID: u32 = 0x41030;

/// Whether the probed node lists `compatible` in its `compatible` string
/// list (`of_device_id` any-match semantics — see the platform bus).
fn has_compatible(dev: &Device<'_, PlatformBus>, compatible: &str) -> bool {
    dev.info()
        .bus_data()
        .identity
        .compatibles
        .iter()
        .any(|candidate| candidate == compatible)
}

/// The published RTC instance: the live MMIO base of the PL031 block.
struct Pl031Rtc {
    vaddr: usize,
}

impl RtcDevice for Pl031Rtc {
    fn read_time(&self) -> KResult<SystemTime> {
        // SAFETY: `vaddr` is the device-owned PL031 mapping held by
        // `Pl031Data` for as long as this instance can be reached, and the
        // register block stays mapped for that lifetime.
        let rtc = unsafe { arm_pl031::Rtc::new(self.vaddr as *mut u32) };
        crate::common::system_time_from_unsigned_seconds(u64::from(rtc.get_unix_timestamp()))
    }
}

/// Driver-private data (`struct pl031_local`): the publication is the owner
/// of the class entry, the mapping outlives the hardware access it backs.
pub struct Pl031Data {
    /// Dropping unpublishes. First field: the class device withdraws before
    /// the register mapping goes away.
    _publication: Publication<RtcDeviceImpl>,
    _regs: IoMap,
}

/// The driver ops.
pub struct Pl031Driver;

impl DriverOps<PlatformBus> for Pl031Driver {
    type Data = Pl031Data;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<Pl031Data, ProbeError> {
        // Generic-PrimeCell matches must prove their identity before
        // claiming the node; `Fatal` hands the device back to the domain
        // for the next candidate driver.
        if !has_compatible(dev, "arm,pl031")
            && dev.fw().u32("arm,primecell-periphid") != Some(PL031_PRIMECELL_PERIPHID)
        {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        let regs = dev.res().iomap(0)?;
        let device: RtcDeviceImpl = Arc::new(Pl031Rtc {
            vaddr: regs.as_ptr() as usize,
        });
        let publication = dev
            .publish(&RTC, device, "rtc-pl031")
            .map_err(crate::common::map_driver_error)?;
        log::info!(
            "rtc-pl031: published at {:#x} ({} bytes)",
            regs.region().base,
            regs.region().size
        );
        Ok(Pl031Data {
            _publication: publication,
            _regs: regs,
        })
    }
}

driver_base::declare_driver!(PlatformBus, Pl031Driver, "rtc-pl031", IDS);
