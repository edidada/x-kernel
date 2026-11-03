// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Synopsys DWC3 USB controller, Rockchip glue + core bring-up (host path).
//!
//! Two layers, exactly as the reference splits them:
//!
//! - the glue (`rockchip,rk3588-dwc3`) owns the clocks, the `usb3-otg` reset
//!   and the PHY power sequence, and then lets the core look at the registers;
//! - the core (`snps,dwc3`) resets the controller, selects host mode and hands
//!   the xHCI register window to [`drv_usb_xhci`].
//!
//! The xHCI part is a separate driver because it is a separate block: the DWC3
//! only wraps it, and a second controller (EHCI/OHCI) would share the bus core
//! above instead.

pub mod core;
pub mod glue;
pub mod regs;

pub use glue::{Dwc3Data, Dwc3Driver, register};
