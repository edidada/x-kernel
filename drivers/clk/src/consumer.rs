// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Consumer entry point: acquire a device's named clock as a [`Clk`] handle.
//!
//! Mirrors the `devm_*` helpers in `device-res`: [`devm_clk_get`] takes a
//! `&dyn DeviceResource` (not a raw DT node), finds the clock by name among the
//! device's pre-parsed `ResourceDesc::Clock` resources, and resolves it through
//! the provider registry. The bus layer pre-parses `clocks`/`clock-names` at
//! enumeration, so neither this nor the driver ever touches the device tree.

use device_res::{DeviceResource, ResError, ResourceDesc};

use crate::{Clk, registry::try_provider};

/// Acquire the clock named `name` for `device`, as a [`Clk`] handle.
///
/// `device` is the abstract device (e.g. a `&DeviceObject` coerced to
/// `&dyn DeviceResource`); its `resources()` already carry the bus-pre-parsed
/// `ResourceDesc::Clock` entries. Returns
/// [`Err(InvalidResource)`](ResError::InvalidResource) if the device has no
/// clock by that name, or [`Err(NoProvider)`](ResError::NoProvider) if no clock
/// provider is registered for its phandle yet (e.g. the SCMI provider has not
/// probed).
///
/// Tier 0: the handle only supports [`Clk::rate`]; there is no enable/disable
/// refcount, so devm cleanup is a no-op for now (it becomes meaningful at
/// Tier 1).
pub fn devm_clk_get(device: &dyn DeviceResource, name: &str) -> Result<Clk, ResError> {
    let desc = device
        .resources()
        .iter()
        .find_map(|resource| match resource {
            ResourceDesc::Clock(clk) if clk.name == name.as_bytes() => Some(clk),
            _ => None,
        })
        .ok_or(ResError::InvalidResource)?;
    let provider = try_provider(desc.phandle).ok_or(ResError::NoProvider)?;
    Ok(Clk::new(provider, desc.specifier.clone()))
}
