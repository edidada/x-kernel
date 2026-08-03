// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock resource descriptor ([`ClkDesc`]) — the payload of
//! [`ResourceDesc::Clock`](crate::ResourceDesc::Clock).

/// Description of one clock a device consumes (the payload of
/// [`ResourceDesc::Clock`](crate::ResourceDesc::Clock)).
///
/// Produced by the bus layer from the DT `clocks`/`clock-names` properties at
/// enumeration, so a driver never has to touch the device tree to get its
/// clocks. Resolved into a clock handle by the `clk` subsystem's
/// `devm_clk_get(device, name)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClkDesc {
    /// The `clock-names` entry for this clock (a slice into the static DTB).
    /// Empty when the device has `clocks` but no `clock-names`.
    pub name: &'static [u8],
    /// DT phandle of the clock producer node.
    pub phandle: u32,
    /// The `#clock-cells` specifier words following the phandle in `clocks`.
    pub specifier: smallvec::SmallVec<[u32; 2]>,
}

impl ClkDesc {
    /// Build a clock descriptor. `specifier` is the `#clock-cells` words (the
    /// caller passes a plain slice; this hides the `SmallVec` so callers need
    /// not depend on `smallvec`).
    pub fn new(name: &'static [u8], phandle: u32, specifier: &[u32]) -> Self {
        Self {
            name,
            phandle,
            specifier: smallvec::SmallVec::from_slice(specifier),
        }
    }
}
