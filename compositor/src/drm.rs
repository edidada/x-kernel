// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DRM device setup and dumb-buffer mapping.

use std::os::fd::{AsRawFd, OwnedFd};

use smithay::{
    backend::drm::{
            DrmDevice, DrmDeviceFd,
        },
    utils::DeviceFd,
};


pub(crate) fn open_device() -> Result<(DrmDevice, ()), Box<dyn std::error::Error>> {
    use std::fs::OpenOptions;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dri/card0")?;
    let owned: OwnedFd = file.into();
    let (device, _notifier) = DrmDevice::new(DrmDeviceFd::new(DeviceFd::from(owned)), true)?;
    Ok((device, ()))
}

pub(crate) fn pick_output(
    device: &DrmDevice,
) -> Result<
    (
        smithay::reexports::drm::control::connector::Handle,
        smithay::reexports::drm::control::crtc::Handle,
        smithay::reexports::drm::control::Mode,
    ),
    Box<dyn std::error::Error>,
> {
    use smithay::reexports::drm::control::Device as ControlDevice;
    let resources = device.resource_handles()?;
    for connector in resources.connectors() {
        let info = device.get_connector(*connector, false)?;
        if info.state() != smithay::reexports::drm::control::connector::State::Connected
            || info.modes().is_empty()
        {
            continue;
        }
        let crtc = info
            .encoders()
            .iter()
            .find_map(|encoder| device.get_encoder(*encoder).ok()?.crtc())
            .or_else(|| resources.crtcs().first().copied())
            .ok_or("no crtc")?;
        return Ok((*connector, crtc, info.modes()[0]));
    }
    Err("no connected connector".into())
}

pub(crate) fn map_buffer(
    device: &DrmDevice,
    buffer: &smithay::backend::allocator::dumb::DumbBuffer,
) -> Result<(*mut u8, usize, usize), Box<dyn std::error::Error>> {
    use smithay::reexports::drm::buffer::Buffer as DrmBuffer;

    #[repr(C)]
    struct MapDumb {
        handle: u32,
        pad: u32,
        offset: u64,
    }
    const DRM_IOCTL_MODE_MAP_DUMB: std::ffi::c_ulong =
        ((3u64 << 30) | (16u64 << 16) | ((b'd' as u64) << 8) | 0xb3) as std::ffi::c_ulong;

    let inner = buffer.handle();
    let (_, height) = DrmBuffer::size(inner);
    let pitch = DrmBuffer::pitch(inner) as usize;
    let size = pitch * height as usize;
    let fd = device.device_fd().as_raw_fd();
    let mut request = MapDumb {
        handle: u32::from(DrmBuffer::handle(inner)),
        pad: 0,
        offset: 0,
    };
    // SAFETY: `request` is a valid `drm_mode_map_dumb`; `fd` is the DRM fd.
    if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut request) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: mapping the dumb buffer read/write for the whole size.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            request.offset as libc::off_t,
        )
    };
    if map == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((map as *mut u8, size, pitch))
}

