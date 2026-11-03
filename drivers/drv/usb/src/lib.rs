// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! USB host controller drivers.
//!
//! Two blocks, one family: `xhci` is the standard host controller interface and
//! `dwc3` is the Synopsys wrapper it sits inside on this SoC (glue clocks, the
//! `usb3-otg` reset, the USB2 PHY, and the core's own global registers). The
//! device-facing bus logic — descriptors, the transfer contract, enumeration —
//! lives in the `usb` bus crate so a second controller family reuses it.

#![no_std]

extern crate alloc;

pub mod dwc3;
pub mod xhci;

pub use dwc3::{Dwc3Data, Dwc3Driver, register};
