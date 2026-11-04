// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel compositor: a Wayland compositor built on Smithay.
//!
//! Stage 0 only inspects the DRM device: which connectors exist, which modes
//! the host reports, and whether the CRTCs are usable. Stage 1 adds a surface
//! with a pixman renderer.

use std::{env, fs::OpenOptions, os::fd::OwnedFd};

use smithay::{
    backend::drm::{DrmDevice, DrmDeviceFd},
    reexports::drm::control::Device as ControlDevice,
    utils::DeviceFd,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let path = env::args().nth(1).unwrap_or_else(|| "/dev/dri/card0".into());
    let file = OpenOptions::new().read(true).write(true).open(&path)?;
    let owned: OwnedFd = file.into();
    let (device, _notifier) = DrmDevice::new(DrmDeviceFd::new(DeviceFd::from(owned)), true)?;

    let resources = device.resource_handles()?;
    println!("{path}: crtcs={:?} encoders={:?}", resources.crtcs(), resources.encoders());

    for handle in resources.connectors() {
        let info = device.get_connector(*handle, false)?;
        println!(
            "connector {:?}: interface={:?} state={:?} modes={}",
            handle,
            info.interface(),
            info.state(),
            info.modes().len()
        );
        for mode in info.modes().iter().take(4) {
            println!(
                "  {}x{} @ {} Hz",
                mode.size().0,
                mode.size().1,
                mode.vrefresh()
            );
        }
    }

    Ok(())
}
