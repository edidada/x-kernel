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
    /// OS-neutral name of the clock **producer** (the provider registered under
    /// this key in the `clk` registry). For the DT path this is the producer
    /// node's name with the `@unit` address suffix stripped (e.g.
    /// `scmi_clk`, `xin24m`), derived by the bus layer — not the raw DT phandle,
    /// so the descriptor and registry are firmware-description-agnostic and a
    /// future ACPI/PCI backend can use its own naming without changing this
    /// type.
    pub provider: &'static str,
    /// The `#clock-cells` specifier words selecting the clock within the
    /// provider.
    pub specifier: smallvec::SmallVec<[u32; 2]>,
}

impl ClkDesc {
    /// Build a clock descriptor. `provider` is the producer's OS-neutral name
    /// (DT path: the producer node name with `@unit` stripped); `specifier` is
    /// the `#clock-cells` words (the caller passes a plain slice; this hides
    /// the `SmallVec` so callers need not depend on `smallvec`).
    pub fn new(name: &'static [u8], provider: &'static str, specifier: &[u32]) -> Self {
        Self {
            name,
            provider,
            specifier: smallvec::SmallVec::from_slice(specifier),
        }
    }
}
