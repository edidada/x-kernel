// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PC-compatible CMOS RTC driver.
//!
//! The x86 CMOS clock has two enumeration sources that share one physical
//! register pair: DT is not available on x86, so the composition root
//! enumerates a board-static platform device (`rtc-cmos`) whose descriptor
//! carries the architecturally fixed I/O-port range. The register access
//! itself is delegated to `x86_rtc`, which encapsulates the BCD/12-hour
//! format handling and the port pair; see `docs/design.md` for why the
//! port base is validated rather than passed through.

use alloc::sync::Arc;

use driver_base::{
    AliasMatch, Device, DriverOps, PlatformBus, PlatformId, ProbeError, Publication, ResourceKind,
    RuntimeError,
};
use kerrno::KResult;
use ktime_types::SystemTime;
use subsys_rtc::{RtcDevice, RtcDeviceImpl, class::RTC};

/// `struct platform_device_id cmos_rtc_ids[]`.
static IDS: &[PlatformId] = &[PlatformId::Alias(AliasMatch::new("rtc-cmos"))];

/// CMOS command port (`RTC_CMOS_COMMAND`).
const CMOS_COMMAND_PORT: u16 = 0x70;
/// Command + data port, the architecture-defined pair.
const CMOS_PORT_COUNT: usize = 2;

/// The published RTC instance.
struct CmosRtc;

impl RtcDevice for CmosRtc {
    fn read_time(&self) -> KResult<SystemTime> {
        crate::common::system_time_from_unsigned_seconds(x86_rtc::Rtc::new().get_unix_timestamp())
    }
}

/// Driver-private data: the publication owns the class entry; the CMOS
/// clock acquires no mapping (port I/O needs none).
pub struct CmosData {
    /// Dropping unpublishes.
    _publication: Publication<RtcDeviceImpl>,
}

/// The driver ops.
pub struct CmosDriver;

impl DriverOps<PlatformBus> for CmosDriver {
    type Data = CmosData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<CmosData, ProbeError> {
        // The descriptor states the port window this device owns; the
        // backend's register pair must agree with it. A mismatch means the
        // board description and the hardware model disagree — refuse rather
        // than read an unowned port.
        let range = dev
            .res()
            .map()
            .entries()
            .iter()
            .find_map(|entry| match entry.kind {
                ResourceKind::IoPort(range) => Some(range),
                _ => None,
            })
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
        if range.base != CMOS_COMMAND_PORT || usize::from(range.size) < CMOS_PORT_COUNT {
            log::warn!(
                "rtc-cmos: descriptor port range {:#x}+{} disagrees with the CMOS pair at {:#x}",
                range.base,
                range.size,
                CMOS_COMMAND_PORT
            );
            return Err(ProbeError::Fatal(RuntimeError::Invalid));
        }

        let publication = dev
            .publish(&RTC, Arc::new(CmosRtc), "rtc-cmos")
            .map_err(crate::common::map_driver_error)?;
        log::info!(
            "rtc-cmos: published (ports {:#x}..{:#x})",
            range.base,
            range.base + range.size
        );
        Ok(CmosData {
            _publication: publication,
        })
    }
}

driver_base::declare_driver!(PlatformBus, CmosDriver, "rtc-cmos", IDS);
