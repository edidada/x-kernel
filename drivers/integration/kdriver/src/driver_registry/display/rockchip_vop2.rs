// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Rockchip RK3588 VOP2 platform driver glue.
//!
//! The controller itself lives in the `rockchip-vop2` device crate; this module
//! only matches the firmware node, maps its register window and publishes the
//! resulting display device into the class registry, the same way the VirtIO
//! glue publishes `virtio-gpu`.

use alloc::{boxed::Box, format, sync::Arc};

use device_res::TimeOp;
use display::DisplayInfo;
use driver_base::{DeviceKind, DriverError};
use kdevice::{BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject};
use rockchip_vop2::{Vop2Display, Vop2Port};

use crate::driver_registry::{BoxedDriver, firmware_specs::RK3588_VOP};

/// Upper bounds for the device-tree graph walk.
const MAX_VIDEO_PORTS: usize = 4;
const MAX_PORT_ENDPOINTS: usize = 8;

struct Vop2Driver;

impl DeviceDriver for Vop2Driver {
    fn name(&self) -> &'static str {
        "rockchip-vop2"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Display
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &RK3588_VOP
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> driver_base::DriverResult<()> {
        let (vaddr, _size) = crate::iomap_first_mmio(device.as_ref(), "vop2")?;
        let info = panel_info().ok_or(DriverError::InvalidInput)?;
        let video_port = panel_video_port().ok_or(DriverError::InvalidInput)?;
        // The controller needs frame-accurate spacing between its register
        // writes; hand it the host clock capability instead of letting it spin
        // on device reads.
        let clock: &'static dyn TimeOp = crate::resource::resource_provider();

        // SAFETY: `iomap_first_mmio` returned a kernel virtual address mapping
        // the controller's MMIO window described by firmware and installed in
        // `memspace`. The mapping is owned by this device and outlives the
        // published display device, so the register window stays valid.
        let display = unsafe {
            Vop2Display::new(
                vaddr.as_ptr() as usize,
                Vop2Port {
                    id: video_port,
                    info,
                },
                clock,
            )
        };

        log::info!(
            "vop2: controller version {:#x}, panel {}x{}, video port {}",
            display.version(),
            info.width,
            info.height,
            video_port
        );
        kclass::publish_display(device, Box::new(display)).map(drop)
    }
}

/// Resolves the video port that scans the panel out of the device-tree graph.
///
/// The board wiring is expressed as an OF graph: the enabled input endpoint of
/// the DSI controller carries a `remote-endpoint` phandle that points at the
/// VOP endpoint feeding it, and the port that owns that endpoint is the video
/// port to program. Disabled endpoints are skipped, which is what separates
/// `dsi0_in_vp3` from the inactive `dsi0_in_vp2` link.
fn panel_video_port() -> Option<u8> {
    let vop = of::find_compatible("rockchip,rk3588-vop")?;
    let dsi = of::find_compatible("rockchip,rk3588-mipi-dsi2")?;

    // VOP endpoints referenced by the DSI controller's input port.
    let mut linked = [0u32; MAX_PORT_ENDPOINTS];
    let mut linked_len = 0;
    for endpoint in 0..MAX_PORT_ENDPOINTS {
        let path = format!("/{}/ports/port@0/endpoint@{endpoint}", dsi.name);
        let Some(node) = of::resolve_node(&path) else {
            continue;
        };
        if !node_is_enabled(node) {
            continue;
        }
        if let Some(phandle) = node.property_u32("remote-endpoint")
            && linked_len < linked.len()
        {
            linked[linked_len] = phandle;
            linked_len += 1;
        }
    }

    // The port whose endpoint carries one of those phandles feeds the panel.
    for port in 0..MAX_VIDEO_PORTS {
        for endpoint in 0..MAX_PORT_ENDPOINTS {
            let path = format!("/{}/ports/port@{port}/endpoint@{endpoint}", vop.name);
            let Some(node) = of::resolve_node(&path) else {
                continue;
            };
            let Some(phandle) = node.property_u32("phandle") else {
                continue;
            };
            if linked[..linked_len].contains(&phandle) {
                return u8::try_from(port).ok();
            }
        }
    }
    None
}

/// Whether a device-tree node is enabled (`status` absent or `okay`).
fn node_is_enabled(node: of::FdtNode<'static, 'static>) -> bool {
    match node.property_str("status") {
        None => true,
        Some(status) => status != "disabled" && status != "fail",
    }
}

/// Reads the active panel resolution from the firmware display timing node.
///
/// The panel node exports `display-timings` with `hactive`/`vactive`; the
/// controller has no way to learn the attached panel's geometry on its own, so
/// the timing comes from the same description the panel driver uses.
fn panel_info() -> Option<DisplayInfo> {
    let fdt = of::fdt()?;
    for node in fdt.all_nodes() {
        let (Some(width), Some(height)) =
            (node.property_u32("hactive"), node.property_u32("vactive"))
        else {
            continue;
        };
        if width > 0 && height > 0 {
            return Some(DisplayInfo { width, height });
        }
    }
    None
}

pub(super) fn descriptor() -> BoxedDriver {
    Arc::new(Vop2Driver)
}
