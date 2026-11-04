// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `snps,dwc3` core bring-up for the host path.
//!
//! The reference `dwc3_core_init()` is 2869 lines because it serves device and
//! OTG modes as well. Host-only needs a small, ordered subset:
//!
//! 1. soft reset the core and wait for `GSTS.CSRST` to drop,
//! 2. read the hardware parameters (they decide the FIFO layout),
//! 3. select the host role (`GCTL.PrtCapDir`),
//! 4. configure the UTMI+ PHY interface and the enable-sleep-mode handshake,
//! 5. hand the same register window to the xHCI driver.

use driver_base::{Device, IoMap, PlatformBus, ProbeError};
use log::info;

use crate::dwc3::regs::{self, GCTL, GSTS, GUSB2PHYCFG, gctl, gstst, gux2phycfg};

/// Bounded wait for a self-clearing bit, in microseconds.
const POLL_ITERS: usize = 10_000;

/// Hardware parameters `dwc3_cache_hwparams` keeps.
#[derive(Clone, Copy, Debug)]
pub struct HwParams {
    pub gsnpsid: u32,
    pub hwparams0: u32,
    pub hwparams1: u32,
    pub hwparams3: u32,
    pub hwparams4: u32,
}

/// Resets and configures the core for host mode.
pub fn init(
    dev: &Device<'_, PlatformBus>,
    regs: &IoMap,
    utmi_wide: bool,
    dis_enblslpm: bool,
) -> Result<HwParams, ProbeError> {
    // The capability word is the liveness check that was measured on a working
    // board: 0x01100020 = CAPLENGTH 0x20, xHCI 1.1. A dead window reads back as
    // 0 or all-ones instead of aborting.
    let capability = regs.read32(0);
    info!(
        "dwc3: xhci capability {capability:#010x} (caplen {}, version {}.{})",
        capability & 0xff,
        (capability >> 24) & 0xff,
        (capability >> 16) & 0xff,
    );
    if capability == 0 || capability == 0xffff_ffff {
        log::error!("dwc3: the controller does not answer at offset 0");
        return Err(ProbeError::Fatal(driver_base::RuntimeError::Io));
    }
    core_soft_reset(regs);
    let params = HwParams {
        gsnpsid: regs.read32(regs::DWC_USB3_GSNPSID),
        hwparams0: regs.read32(regs::GHWPARAMS0),
        hwparams1: regs.read32(regs::GHWPARAMS1),
        hwparams3: regs.read32(regs::GHWPARAMS3),
        hwparams4: regs.read32(regs::GHWPARAMS4),
    };
    info!(
        "dwc3: gsnpsid {:#010x} hwparams0 {:#010x} hwparams1 {:#010x} hwparams3 {:#010x} \
         hwparams4 {:#010x}",
        params.gsnpsid, params.hwparams0, params.hwparams1, params.hwparams3, params.hwparams4,
    );
    if params.gsnpsid == 0 || params.gsnpsid == 0xffff_ffff {
        log::error!(
            "dwc3: core id is {:#010x}, the window is not the controller",
            params.gsnpsid
        );
        return Err(ProbeError::Fatal(driver_base::RuntimeError::Io));
    }

    set_host_mode(regs);
    program_usb2_phy(regs, utmi_wide, dis_enblslpm);
    program_usb3_pipe(regs);
    let _ = dev;
    Ok(params)
}

fn core_soft_reset(regs: &IoMap) {
    let gctl = regs.read32(GCTL);
    regs.write32(GCTL, gctl | gctl::CORESOFTRESET);
    let mut waited = 0;
    while regs.read32(GCTL) & gctl::CORESOFTRESET != 0 && waited < POLL_ITERS {
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1));
        waited += 1;
    }
    let gsts = regs.read32(GSTS);
    info!(
        "dwc3: soft reset done after {waited} us, GSTS {gsts:#010x} (csrst {} curmod {})",
        gsts & gstst::CSRST != 0,
        (gsts & gstst::CURMOD_HOST) != 0,
    );
}

fn set_host_mode(regs: &IoMap) {
    let gctl = regs.read32(GCTL);
    regs.write32(GCTL, (gctl & !gctl::PRTCAPDIR_MASK) | gctl::PRTCAPDIR_HOST);
    let gsts = regs.read32(GSTS);
    info!(
        "dwc3: host mode selected, GCTL {:#010x} GSTS {:#010x}",
        regs.read32(GCTL),
        gsts,
    );
}

/// Programs the SuperSpeed PIPE control word.
///
/// The value is taken from a working board (`GUSB3PIPECTL0` reads 0x075e8020
/// there); this driver never touched the register before, so a device that
/// enumerates over SuperSpeed would find the PIPE unconfigured.
fn program_usb3_pipe(regs: &IoMap) {
    let offset = crate::dwc3::regs::GUSB3PIPECTL;
    let before = regs.read32(offset);
    regs.write32(offset, USB3_PIPE_CONTROL);
    info!(
        "dwc3: GUSB3PIPECTL {before:#010x} -> {:#010x}",
        regs.read32(offset),
    );
}

/// `GUSB3PIPECTL0` as measured on a board whose SuperSpeed ports work.
const USB3_PIPE_CONTROL: u32 = 0x075e_8020;

fn program_usb2_phy(regs: &IoMap, utmi_wide: bool, dis_enblslpm: bool) {
    // Port 0 of the USB2 PHY block; the reference programs every port it finds
    // in GHWPARAMS3, and this part has one per controller instance.
    let mut config = regs.read32(GUSB2PHYCFG);
    if utmi_wide {
        config |= gux2phycfg::PHYIF;
    } else {
        config &= !gux2phycfg::PHYIF;
    }
    if dis_enblslpm {
        config &= !gux2phycfg::ENABLSLPM;
    } else {
        config |= gux2phycfg::ENABLSLPM;
    }
    // The PHY must not stay suspended while the controller comes up: the
    // first port reset happens before anything else can wake it.
    config &= !gux2phycfg::SUSPHY;
    regs.write32(GUSB2PHYCFG, config);
    info!(
        "dwc3: GUSB2PHYCFG {:#010x} (utmi_wide {utmi_wide} dis_enblslpm {dis_enblslpm})",
        regs.read32(GUSB2PHYCFG),
    );
}
