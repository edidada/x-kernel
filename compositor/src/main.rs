// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel compositor: a Wayland compositor built on Smithay.
//!
//! `stage1` modesets a solid-color dumb buffer through Smithay's DRM surface
//! path, so the scanout path is exercised by Smithay rather than by raw
//! ioctls. `probe` only reports the DRM resources.

use std::{env, fs::OpenOptions, os::fd::OwnedFd, thread::sleep, time::Duration};

use smithay::{
    backend::{
        allocator::{
            Allocator, Buffer as AllocBuffer, Fourcc, Modifier, dumb::DumbAllocator,
        },
        drm::{
            DrmDevice, DrmDeviceFd, PlaneConfig, PlaneState, dumb::{DumbFramebuffer, framebuffer_from_dumb_buffer},
        },
    },
    reexports::drm::{
        buffer::{Buffer as DrmBuffer, Handle as BufferHandle},
        control::{Device as ControlDevice, Mode, connector, crtc, framebuffer},
    },
    utils::{Buffer, DeviceFd, Physical, Rectangle, Size, Transform},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let args: Vec<String> = env::args().collect();
    let path = args.get(1).cloned().unwrap_or_else(|| "/dev/dri/card0".into());
    let file = OpenOptions::new().read(true).write(true).open(&path)?;
    let owned: OwnedFd = file.into();
    let (mut device, _notifier) = DrmDevice::new(DrmDeviceFd::new(DeviceFd::from(owned)), true)?;

    let (connector, crtc, mode) = pick_output(&device)?;
    println!(
        "output: connector {connector:?} crtc {crtc:?} mode {}x{}@{}",
        mode.size().0,
        mode.size().1,
        mode.vrefresh()
    );

    if args.iter().any(|arg| arg == "probe") {
        return Ok(());
    }

    // Stage 1: modeset a solid-color dumb buffer through Smithay's DRM surface.
    let surface = device.create_surface(crtc, mode, &[connector])?;
    let plane = surface
        .planes()
        .primary
        .first()
        .ok_or("the CRTC exposes no primary plane")?
        .handle;

    let (width, height) = mode.size();
    let (width, height) = (width as u32, height as u32);
    let mut allocator = DumbAllocator::new(device.device_fd().clone());
    // Dumb buffers are linear; the allocator rejects a request whose modifier
    // list contains no linear entry (an empty list counts as none).
    let buffer = allocator.create_buffer(
        width,
        height,
        Fourcc::Xrgb8888,
        &[Modifier::Linear],
    )?;
    fill(&device, &buffer, 0x0030_60c0)?;
    println!("dumb buffer: {width}x{height} {:?}", AllocBuffer::format(&buffer));

    let framebuffer = framebuffer_from_dumb_buffer(device.device_fd(), &buffer, true)?;
    let state = PlaneState {
        handle: plane,
        config: Some(PlaneConfig {
            src: Rectangle::from_size(Size::<f64, Buffer>::from((width as f64, height as f64))),
            dst: Rectangle::from_size(Size::<i32, Physical>::from((width as i32, height as i32))),
            transform: Transform::Normal,
            alpha: 1.0,
            damage_clips: None,
            fb: *<DumbFramebuffer as AsRef<framebuffer::Handle>>::as_ref(&framebuffer),
            fence: None,
        }),
    };
    surface.commit([state], false)?;
    println!("committed solid color through Smithay, holding 60s");

    sleep(Duration::from_secs(60));
    Ok(())
}

/// Picks the first connected connector with a mode and its CRTC.
fn pick_output(
    device: &DrmDevice,
) -> Result<(connector::Handle, crtc::Handle, Mode), Box<dyn std::error::Error>> {
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
            .ok_or("the connector has no usable CRTC")?;
        return Ok((*connector, crtc, info.modes()[0]));
    }
    Err("no connected connector with a mode".into())
}

/// Fills the whole dumb buffer with `color` (XRGB8888).
fn fill(
    device: &DrmDevice,
    buffer: &smithay::backend::allocator::dumb::DumbBuffer,
    color: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsRawFd;

    #[repr(C)]
    struct MapDumb {
        handle: u32,
        pad: u32,
        offset: u64,
    }

    // `_IOWR('d', 0xb3, struct drm_mode_map_dumb)`.
    const DRM_IOCTL_MODE_MAP_DUMB: std::ffi::c_ulong =
        ((3u64 << 30) | (16u64 << 16) | ((b'd' as u64) << 8) | 0xb3) as std::ffi::c_ulong;

    let fd = device.device_fd().as_raw_fd();
    let inner = buffer.handle();
    let mut request = MapDumb {
        handle: u32::from(DrmBuffer::handle(inner)),
        pad: 0,
        offset: 0,
    };
    // SAFETY: `request` is a valid `drm_mode_map_dumb` and `fd` is the DRM fd.
    let rc = unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut request) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    let (_, height) = DrmBuffer::size(inner);
    let size = DrmBuffer::pitch(inner) as usize * height as usize;
    // SAFETY: the dumb buffer is mapped read/write and covers `size` bytes.
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
    // SAFETY: `map` points at `size` writable bytes owned by this process.
    unsafe {
        let pixels = std::slice::from_raw_parts_mut(map as *mut u32, size / 4);
        pixels.fill(color);
        libc::munmap(map, size);
    }
    Ok(())
}
