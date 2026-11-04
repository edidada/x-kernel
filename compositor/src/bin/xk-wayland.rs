// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stage 2: a Wayland compositor that composites SHM client buffers into a
//! dumb buffer and presents it through Smithay's DRM surface.

use std::{
    os::fd::{AsRawFd, OwnedFd},
    sync::{Arc, Mutex},
    time::Duration,
};

use smithay::{
    backend::{
        allocator::{Allocator, Buffer as AllocBuffer, Fourcc, Modifier, dumb::DumbAllocator},
        drm::{
            DrmDevice, DrmDeviceFd, PlaneConfig, PlaneState,
            dumb::{DumbFramebuffer, framebuffer_from_dumb_buffer},
        },
    },
    desktop::{Space, Window},
    input::{Seat, SeatHandler, SeatState},
    reexports::{
        calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic},
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer, wl_surface::WlSurface},
        },
    },
    utils::{Buffer, DeviceFd, Logical, Physical, Point, Rectangle, Size, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState, get_parent, with_states},
        output::{OutputHandler, OutputManagerState},

        shell::xdg::{ToplevelSurface, XdgShellHandler, XdgShellState},
        shm::{ShmHandler, ShmState, with_buffer_contents},
        socket::ListeningSocketSource,
    },
};

#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

struct State {
    display_handle: DisplayHandle,
    socket_name: std::ffi::OsString,
    space: Space<Window>,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    output_manager_state: OutputManagerState,
    seat_state: SeatState<State>,
    seat: Seat<State>,
    surface: smithay::backend::drm::DrmSurface,
    framebuffer: DumbFramebuffer,
    map: *mut u8,
    map_len: usize,
    pitch: usize,
    width: u32,
    height: u32,
    dirty: bool,
    first_commit: bool,
}

impl State {
    fn composite(&mut self) {
        // Background, then every mapped toplevel at its space location.
        // SAFETY: the mapping is owned by this state and covers `map_len`.
        unsafe {
            let pixels = std::slice::from_raw_parts_mut(self.map as *mut u32, self.map_len / 4);
            pixels.fill(0x0020_2020);
        }
        let width = self.width as usize;
        let height = self.height as usize;
        let pitch = self.pitch;
        let map = self.map;
        for window in self.space.elements() {
            let Some(toplevel) = window.toplevel() else { continue };
            let surface = toplevel.wl_surface();
            let location = self.space.element_location(window).unwrap_or_default();
            // The committed buffer lives in the renderer surface state; the raw
            // `SurfaceAttributes.buffer` is only the pending assignment and is
            // consumed by `on_commit_buffer_handler`.
            let buffer = with_states(surface, |states| {
                let data = states
                    .data_map
                    .get::<smithay::backend::renderer::utils::RendererSurfaceStateUserData>()?;
                let data = data.lock().unwrap();
                data.buffer().cloned()
            });
            let Some(buffer) = buffer else { continue };
            {
                let _ = with_buffer_contents(&buffer, |ptr, _len, data| {
                    let stride = data.stride as usize;
                    let offset = data.offset as usize;
                    let copy_width = (data.width as usize).min(width);
                    let copy_height = (data.height as usize).min(height);
                    let dst_x = location.x.max(0) as usize;
                    let dst_y = location.y.max(0) as usize;
                    for row in 0..copy_height {
                        if dst_y + row >= height {
                            break;
                        }
                        let src = ptr.wrapping_add(offset + row * stride);
                        let dst_row = (dst_y + row) * pitch;
                        let dst = map.wrapping_add(dst_row + dst_x * 4);
                        // SAFETY: both ranges are inside the buffer and the mapping.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                src as *const u8,
                                dst,
                                copy_width.saturating_sub(dst_x).min(width - dst_x) * 4,
                            );
                        }
                    }
                });
            }
        }

        let state = PlaneState {
            handle: self.surface.planes().primary[0].handle,
            config: Some(PlaneConfig {
                src: Rectangle::from_size(Size::<f64, Buffer>::from((
                    self.width as f64,
                    self.height as f64,
                ))),
                dst: Rectangle::from_size(Size::<i32, Physical>::from((
                    self.width as i32,
                    self.height as i32,
                ))),
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: None,
                fb: *AsRef::<smithay::reexports::drm::control::framebuffer::Handle>::as_ref(
                    &self.framebuffer,
                ),
                fence: None,
            }),
        };
        let result = if self.first_commit {
            self.first_commit = false;
            self.surface.commit([state], false)
        } else {
            self.surface.page_flip([state], false)
        };
        if let Err(err) = result {
            tracing::warn!(?err, "present failed");
        }
        self.dirty = false;
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);
        if get_parent(surface).is_none() {
            self.dirty = true;
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: smithay::input::pointer::CursorImageStatus) {}
}

impl OutputHandler for State {}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface);
        self.space.map_element(window, (0, 0), false);
        // Give the client an initial configure so it can map.
        if let Some(toplevel) = self
            .space
            .elements()
            .last()
            .and_then(|window| window.toplevel())
        {
            toplevel.send_configure();
        }
    }

    fn new_popup(
        &mut self,
        _surface: smithay::wayland::shell::xdg::PopupSurface,
        _positioner: smithay::wayland::shell::xdg::PositionerState,
    ) {
    }

    fn grab(
        &mut self,
        _surface: smithay::wayland::shell::xdg::PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
    }

    fn reposition_request(
        &mut self,
        _surface: smithay::wayland::shell::xdg::PopupSurface,
        _positioner: smithay::wayland::shell::xdg::PositionerState,
        _token: u32,
    ) {
    }


}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let (mut device, _notifier) = open_device()?;
    let (connector, crtc, mode) = pick_output(&device)?;
    let (width, height) = (mode.size().0 as u32, mode.size().1 as u32);

    let surface = device.create_surface(crtc, mode, &[connector])?;
    let mut allocator = DumbAllocator::new(device.device_fd().clone());
    let buffer = allocator.create_buffer(width, height, Fourcc::Xrgb8888, &[Modifier::Linear])?;
    let (map, map_len, pitch) = map_buffer(&device, &buffer)?;
    let framebuffer = framebuffer_from_dumb_buffer(device.device_fd(), &buffer, true)?;

    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let display_handle = display.handle();

    let compositor_state = CompositorState::new::<State>(&display_handle);
    let xdg_shell_state = XdgShellState::new::<State>(&display_handle);
    let shm_state = ShmState::new::<State>(&display_handle, vec![]);
    let output_manager_state = OutputManagerState::new_with_xdg_output::<State>(&display_handle);
    let mut seat_state = SeatState::new();
    let seat = seat_state.new_wl_seat(&display_handle, "seat0");

    let socket = ListeningSocketSource::new_auto()?;
    let socket_name = socket.socket_name().to_os_string();

    event_loop
        .handle()
        .insert_source(socket, |stream, _, state: &mut State| {
            if let Err(err) = state
                .display_handle
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                tracing::warn!(?err, "client insert failed");
            }
        })?;
    event_loop.handle().insert_source(
        Generic::new(display, Interest::READ, Mode::Level),
        |_, display, state: &mut State| {
            // SAFETY: the display lives in this source for the loop lifetime.
            unsafe {
                let _ = display.get_mut().dispatch_clients(state);
            }
            Ok(PostAction::Continue)
        },
    )?;

    let mut state = State {
        display_handle,
        socket_name: socket_name.clone(),
        space: Space::default(),
        compositor_state,
        xdg_shell_state,
        shm_state,
        output_manager_state,
        seat_state,
        seat,
        surface,
        framebuffer,
        map,
        map_len,
        pitch,
        width,
        height,
        dirty: true,
        first_commit: true,
    };

    // SAFETY: the process is single-threaded at this point.
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &socket_name);
    }
    println!(
        "compositor listening on {:?}, {}x{}",
        socket_name, width, height
    );

    // Spawn the test client, if present.
    let client = std::env::args().nth(1);
    if let Some(client) = client {
        // SAFETY: single-threaded startup.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run".into()));
        }
        std::process::Command::new(client).spawn()?;
    }

    let mut frame = 0u32;
    event_loop.run(Some(Duration::from_millis(16)), &mut state, |state: &mut State| {
        let _ = state.display_handle.flush_clients();
        if state.dirty || frame % 60 == 0 {
            state.composite();
        }
        frame = frame.wrapping_add(1);
    })?;
    Ok(())
}

fn open_device() -> Result<(DrmDevice, ()), Box<dyn std::error::Error>> {
    use std::fs::OpenOptions;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dri/card0")?;
    let owned: OwnedFd = file.into();
    let (device, _notifier) = DrmDevice::new(DrmDeviceFd::new(DeviceFd::from(owned)), true)?;
    Ok((device, ()))
}

fn pick_output(
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

fn map_buffer(
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

smithay::delegate_compositor!(State);
smithay::delegate_xdg_shell!(State);
smithay::delegate_shm!(State);
smithay::delegate_seat!(State);
smithay::delegate_output!(State);
