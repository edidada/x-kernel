// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real-time clock drivers on the driver-core model.
//!
//! One module per concrete RTC, mirroring Linux's `drivers/rtc/`:
//!
//! | module | device | discovery |
//! |---|---|---|
//! | [`goldfish`] | Goldfish virtual RTC | DT `google,goldfish-rtc` |
//! | `cmos` | PC CMOS RTC | board-static alias `rtc-cmos` |
//! | `ls7a` | Loongson LS7A TOY counters | board-static alias `rtc-ls7a` |
//!
//! Every driver samples its register window through the `Device` resource
//! door (`dev.res().iomap(…)`) and publishes the instance into the
//! [`subsys_rtc::RTC`] class with `dev.publish(…)`; the boot service
//! (`subsys_rtc::hctosys`) reads the class after the probe storm.
//!
//! Each backend is one feature. Adding an RTC touches one module, one
//! feature line, one `driver-init` registration and one Kconfig block.

#![no_std]

extern crate alloc;

pub mod common;

#[cfg(all(feature = "cmos", target_arch = "x86_64"))]
pub mod cmos;
#[cfg(feature = "goldfish")]
pub mod goldfish;
#[cfg(feature = "ls7a")]
pub mod ls7a;
#[cfg(any(feature = "ls7a", unittest))]
mod ls7a_toy;
