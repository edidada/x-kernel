// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A minimal Wayland client: one xdg_toplevel showing a gradient through
//! wl_shm. Used to exercise the compositor's SHM compositing path.

use std::time::{Duration, Instant};

use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const WIDTH: i32 = 480;
const HEIGHT: i32 = 320;

struct App {
    configured: bool,
    closed: bool,
}

impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for App {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        proxy: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.closed = true;
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<App>(&connection)?;
    let qh = queue.handle();
    let mut app = App {
        configured: false,
        closed: false,
    };

    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let wm_base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ())?;

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("xk-test-client".into());
    surface.commit();
    queue.roundtrip(&mut app)?;
    if !app.configured {
        return Err("no configure received".into());
    }

    // One shm buffer with a vertical gradient.
    let stride = WIDTH * 4;
    let size = stride * HEIGHT;
    let fd = unsafe {
        // SAFETY: plain memfd creation with a constant name.
        libc::memfd_create(c"xk-test-client".as_ptr(), libc::MFD_CLOEXEC)
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: the fd is valid and truncation to `size` is expected.
    if unsafe { libc::ftruncate(fd, size as libc::off_t) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: the fd is writable and `size` bytes are mapped.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if map == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: the mapping covers `size` bytes.
    unsafe {
        let pixels = std::slice::from_raw_parts_mut(map as *mut u32, (WIDTH * HEIGHT) as usize);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let r = (x * 255 / WIDTH) as u32;
                let g = (y * 255 / HEIGHT) as u32;
                pixels[(y * WIDTH + x) as usize] = (r << 16) | (g << 8) | 0x40;
            }
        }
        libc::munmap(map, size as usize);
    }

    // SAFETY: `fd` stays open for the lifetime of the pool creation call.
    let pool = shm.create_pool(
        unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
        size as i32,
        &qh,
        (),
    );
    let buffer = pool.create_buffer(
        0,
        WIDTH,
        HEIGHT,
        stride,
        wl_shm::Format::Xrgb8888,
        &qh,
        (),
    );
    pool.destroy();
    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, WIDTH, HEIGHT);
    surface.commit();

    println!("test client attached a {WIDTH}x{HEIGHT} shm buffer");

    let deadline = Instant::now() + Duration::from_secs(60);
    while !app.closed && Instant::now() < deadline {
        queue.blocking_dispatch(&mut app)?;
    }
    Ok(())
}
