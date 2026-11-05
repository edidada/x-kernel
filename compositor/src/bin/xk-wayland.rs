// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stage 2: a Wayland compositor that composites SHM client buffers into a
//! dumb buffer and presents it through Smithay's DRM surface.

use std::{
    fs::File,
    io::Read,
    os::fd::{AsRawFd, OwnedFd},
    sync::Arc,
    time::Duration,
};

use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::{
    backend::{
        allocator::{Allocator, Fourcc, Modifier, dumb::DumbAllocator},
        drm::{
            DrmDevice, DrmDeviceFd, PlaneConfig, PlaneState,
            dumb::{DumbFramebuffer, framebuffer_from_dumb_buffer},
        },
    },
    desktop::{Space, Window},
    output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel},
    utils::Serial,
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::Keycode,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::{
        calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic},
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer, wl_surface::WlSurface},
        },
    },
    utils::{Buffer, DeviceFd, Physical, Rectangle, Size, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState, get_parent, with_states},
        output::{OutputHandler, OutputManagerState},

        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        selection::{
            SelectionHandler,
            data_device::{ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler},
        },
        shell::xdg::{
            ToplevelSurface, XdgShellHandler, XdgShellState,
            decoration::{XdgDecorationHandler, XdgDecorationState},
        },
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
    data_device_state: DataDeviceState,
    xdg_activation_state: XdgActivationState,
    viewporter_state: ViewporterState,
    text_input_state: TextInputManagerState,
    output: Output,
    xdg_decoration_state: XdgDecorationState,
    seat_state: SeatState<State>,
    seat: Seat<State>,
    device: DrmDevice,
    crtc: smithay::reexports::drm::control::crtc::Handle,
    connector: smithay::reexports::drm::control::connector::Handle,
    buffer: smithay::backend::allocator::dumb::DumbBuffer,
    surface: smithay::backend::drm::DrmSurface,
    framebuffer: DumbFramebuffer,
    map: *mut u8,
    map_len: usize,
    pitch: usize,
    width: u32,
    height: u32,
    dirty: bool,
    first_commit: bool,
    focus: Option<WlSurface>,
    pointer_location: smithay::utils::Point<f64, smithay::utils::Logical>,
    serial: u32,
    start: std::time::Instant,
    frame: u32,
    selftest_at: Option<u32>,
    focus_sent: bool,
}

enum InputEvent {
    Key { code: u16, value: i32 },
    Rel { code: u16, value: i32 },
}

impl State {
    /// Re-read the host display size and follow it when it changed.
    ///
    /// The DRM connector query asks the backend for the current host size, so a
    /// resized QEMU window shows up here as a new mode.
    fn poll_output(&mut self) {
        let Ok((connector, crtc, mode)) = pick_output(&self.device) else {
            return;
        };
        let (width, height) = (mode.size().0 as u32, mode.size().1 as u32);
        tracing::debug!(
            host = %format!("{width}x{height}"),
            scanout = %format!("{}x{}", self.width, self.height),
            "polled display size"
        );
        if (width, height) == (self.width, self.height) {
            return;
        }
        if let Err(err) = self.switch_mode(connector, crtc, mode) {
            tracing::warn!(?err, "display mode switch failed");
        }
    }

    fn switch_mode(
        &mut self,
        connector: smithay::reexports::drm::control::connector::Handle,
        crtc: smithay::reexports::drm::control::crtc::Handle,
        mode: smithay::reexports::drm::control::Mode,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (width, height) = (mode.size().0 as u32, mode.size().1 as u32);
        let surface = self.device.create_surface(crtc, mode, &[connector])?;
        let mut allocator = DumbAllocator::new(self.device.device_fd().clone());
        let buffer =
            allocator.create_buffer(width, height, Fourcc::Xrgb8888, &[Modifier::Linear])?;
        let (map, map_len, pitch) = map_buffer(&self.device, &buffer)?;
        let framebuffer = framebuffer_from_dumb_buffer(self.device.device_fd(), &buffer, true)?;

        self.surface = surface;
        self.buffer = buffer;
        self.framebuffer = framebuffer;
        self.map = map;
        self.map_len = map_len;
        self.pitch = pitch;
        self.width = width;
        self.height = height;
        self.connector = connector;
        self.crtc = crtc;
        self.first_commit = true;
        self.dirty = true;

        let output_mode = OutputMode {
            size: (width as i32, height as i32).into(),
            refresh: 60_000,
        };
        self.output
            .change_current_state(Some(output_mode), None, None, None);
        self.output.set_preferred(output_mode);

        // Every toplevel follows the new output size; clients ack with a new
        // configure round trip.
        let toplevels: Vec<_> = self
            .space
            .elements()
            .filter_map(|window| window.toplevel().cloned())
            .collect();
        for toplevel in toplevels {
            toplevel.with_pending_state(|state| {
                state.size = Some((width as i32, height as i32).into());
            });
            toplevel.send_configure();
        }
        tracing::info!(width, height, "display followed the host size");
        Ok(())
    }

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

        self.draw_cursor();

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
        // Give the keyboard focus only once the toplevel has a committed
        // buffer; clients ignore input for an unmapped surface.
        if !self.focus_sent && self.focus.as_ref() == Some(surface) {
            if let Some(keyboard) = self.seat.get_keyboard() {
                self.serial = self.serial.wrapping_add(1);
                keyboard.set_focus(self, self.focus.clone(), Serial::from(self.serial));
                self.focus_sent = true;
                // First configure: the size and activation hint go out once,
                // on the commit of the mapped surface.
                let toplevel = self
                    .space
                    .elements()
                    .find(|window| {
                        window.toplevel().is_some_and(|t| t.wl_surface() == surface)
                    })
                    .and_then(|window| window.toplevel())
                    .cloned();
                if let Some(toplevel) = toplevel {
                    toplevel.with_pending_state(|state| {
                        state.size = Some((self.width as i32, self.height as i32).into());
                        state.states.set(
                            smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated,
                        );
                    });
                    toplevel.send_configure();
                }
                eprintln!("focus: keyboard focus set after commit");
            }
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

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}

impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        _token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Activation only raises; keyboard focus is granted by the commit
        // path so that the initial configure and the focus stay in order.
        let target = self
            .space
            .elements()
            .find(|window| window.toplevel().is_some_and(|t| t.wl_surface() == &surface))
            .cloned();
        if let Some(window) = target {
            self.space.raise_element(&window, true);
            eprintln!("activation: raised window");
        }
    }
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        self.request_mode(toplevel, DecorationMode::ClientSide);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        // The compositor draws no decorations, so always agree on client-side.
        let mode = if mode == DecorationMode::ServerSide {
            DecorationMode::ClientSide
        } else {
            mode
        };
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, _toplevel: ToplevelSurface) {}
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        eprintln!("new_toplevel: creating window");
        // Tell the client which output it is on and give it an explicit size;
        // Chromium only treats the window as on-screen once both are known.
        self.output.enter(surface.wl_surface());
        // `Activated` is what tells the client its window is focused; without
        // it Chromium drops pointer and keyboard input.
        let activated = smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated;
        surface.with_pending_state(|state| {
            state.size = Some((self.width as i32, self.height as i32).into());
            state.states.set(activated);
        });
        surface.send_configure();
        self.focus = Some(surface.wl_surface().clone());
        self.focus_sent = false;
        if std::env::var("XK_SELFTEST").is_ok() {
            self.selftest_at = Some(self.frame + 900);
        }
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

/// A 12x18 arrow cursor: 0 transparent, 1 black, 2 white.
const CURSOR: [[u8; 12]; 18] = [
    [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 2, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 2, 2, 2, 1, 0, 0, 0, 0, 0, 0, 0],
    [1, 2, 2, 2, 2, 1, 0, 0, 0, 0, 0, 0],
    [1, 2, 2, 2, 2, 2, 1, 0, 0, 0, 0, 0],
    [1, 2, 2, 2, 2, 2, 2, 1, 0, 0, 0, 0],
    [1, 2, 2, 2, 2, 2, 2, 2, 1, 0, 0, 0],
    [1, 2, 2, 2, 2, 2, 2, 2, 2, 1, 0, 0],
    [1, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 0],
    [1, 2, 2, 1, 2, 2, 1, 0, 0, 0, 0, 0],
    [1, 2, 1, 0, 1, 2, 2, 1, 0, 0, 0, 0],
    [1, 1, 0, 0, 1, 2, 2, 1, 0, 0, 0, 0],
    [1, 0, 0, 0, 0, 1, 2, 2, 1, 0, 0, 0],
    [0, 0, 0, 0, 0, 1, 2, 2, 1, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 1, 2, 1, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0],
];

impl State {
    fn draw_cursor(&self) {
        let origin_x = self.pointer_location.x as i32;
        let origin_y = self.pointer_location.y as i32;
        for (row_index, row) in CURSOR.iter().enumerate() {
            let y = origin_y + row_index as i32;
            if y < 0 || y >= self.height as i32 {
                continue;
            }
            for (column_index, pixel) in row.iter().enumerate() {
                if *pixel == 0 {
                    continue;
                }
                let x = origin_x + column_index as i32;
                if x < 0 || x >= self.width as i32 {
                    continue;
                }
                let color: u32 = if *pixel == 1 { 0x0000_0000 } else { 0x00ff_ffff };
                let offset = y as usize * self.pitch + x as usize * 4;
                // SAFETY: the offset stays inside the dumb-buffer mapping.
                unsafe {
                    *(self.map.add(offset) as *mut u32) = color;
                }
            }
        }
    }

    fn tick_selftest(&mut self) {
        let Some(at) = self.selftest_at else { return };
        if self.frame < at {
            return;
        }
        self.selftest_at = None;
        let time = self.start.elapsed().as_millis() as u32;
        // Click into the page first: Chromium only focuses its document once the
        // window receives a pointer press.
        self.serial = self.serial.wrapping_add(1);
        let serial = Serial::from(self.serial);
        self.pointer_location = (200.0, 200.0).into();
        if let Some(pointer) = self.seat.get_pointer() {
            // Smithay expects (surface, surface origin in global space); the
            // pointer location itself is `MotionEvent.location`.
            let origin = (0.0, 0.0).into();
            let focus = self.focus.clone().map(|surface| (surface, origin));
            pointer.motion(
                self,
                focus,
                &MotionEvent { location: self.pointer_location, serial, time },
            );
            pointer.frame(self);
            pointer.button(
                self,
                &ButtonEvent { button: 0x110, state: ButtonState::Pressed, serial, time },
            );
            pointer.frame(self);
            pointer.button(
                self,
                &ButtonEvent { button: 0x110, state: ButtonState::Released, serial, time },
            );
            pointer.frame(self);
            eprintln!("selftest: clicked at {:?}", self.pointer_location);
        }
        // evdev keycodes for "hello"
        for code in [35u16, 18, 38, 38, 24] {
            self.handle_input(InputEvent::Key { code, value: 1 });
            self.handle_input(InputEvent::Key { code, value: 0 });
        }
        eprintln!("selftest: injected hello");
    }

    fn handle_input(&mut self, event: InputEvent) {
        let time = self.start.elapsed().as_millis() as u32;
        self.serial = self.serial.wrapping_add(1);
        let serial = Serial::from(self.serial);
        match event {
            InputEvent::Key { code, value } if code < 0x100 => {
                eprintln!("key {code} {value}");
                if let Some(keyboard) = self.seat.get_keyboard() {
                    let state = if value == 0 {
                        KeyState::Released
                    } else {
                        KeyState::Pressed
                    };
                    keyboard.input::<(), _>(
                        self,
                        Keycode::new(u32::from(code) + 8),
                        state,
                        serial,
                        time,
                        |_, _, _| smithay::input::keyboard::FilterResult::Forward,
                    );
                }
            }
            InputEvent::Key { code, value } if (0x110..=0x11f).contains(&code) => {
                if value != 0 {
                    if let Some(surface) = self.focus.clone() {
                        if let Some(keyboard) = self.seat.get_keyboard() {
                            keyboard.set_focus(self, Some(surface), serial);
                        }
                    }
                }
                if let Some(pointer) = self.seat.get_pointer() {
                    let state = if value == 0 {
                        ButtonState::Released
                    } else {
                        ButtonState::Pressed
                    };
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button: u32::from(code),
                            state,
                            serial,
                            time,
                        },
                    );
                    pointer.frame(self);
                }
            }
            InputEvent::Rel { code, value } => {
                // Keep the pointer on screen: relative motion accumulates and
                // would otherwise push the (software) cursor out of view.
                let max_x = self.width.saturating_sub(1) as f64;
                let max_y = self.height.saturating_sub(1) as f64;
                match code {
                    0 => {
                        self.pointer_location.x =
                            (self.pointer_location.x + f64::from(value)).clamp(0.0, max_x)
                    }
                    1 => {
                        self.pointer_location.y =
                            (self.pointer_location.y + f64::from(value)).clamp(0.0, max_y)
                    }
                    // REL_WHEEL / REL_HWHEEL: one notch is 15 logical pixels.
                    8 | 6 => {
                        let axis = if code == 8 { Axis::Vertical } else { Axis::Horizontal };
                        let v120 = -value * 120;
                        let amount = -f64::from(value) * 15.0;
                        let axis_event = AxisFrame::new(time)
                            .source(AxisSource::Wheel)
                            .value(axis, amount)
                            .v120(axis, v120);
                        if let Some(pointer) = self.seat.get_pointer() {
                            pointer.axis(self, axis_event);
                            pointer.frame(self);
                        }
                    }
                    _ => return,
                }
                self.dirty = true;
                if let Some(pointer) = self.seat.get_pointer() {
                    let origin = (0.0, 0.0).into();
                    let focus = self.focus.clone().map(|surface| (surface, origin));
                    pointer.motion(
                        self,
                        focus,
                        &MotionEvent {
                            location: self.pointer_location,
                            serial,
                            time,
                        },
                    );
                    pointer.frame(self);
                }
            }
            _ => {}
        }
    }
}

fn spawn_evdev(index: u32, sender: smithay::reexports::calloop::channel::Sender<InputEvent>) {
    let path = format!("/dev/input/event{index}");
    std::thread::spawn(move || {
        let Ok(mut file) = File::open(&path) else {
            eprintln!("input: cannot open {path}");
            return;
        };
        eprintln!("input: opened {path}");
        let mut buf = [0u8; 24];
        let mut filled = 0usize;
        loop {
            // Accumulate partial reads: an input event is exactly 24 bytes and
            // a driver may hand out fewer at a time.
            match file.read(&mut buf[filled..]) {
                Ok(0) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                Ok(n) => {
                    filled += n;
                    if filled < 24 {
                        continue;
                    }
                    filled = 0;
                }
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
            }
            let kind = u16::from_ne_bytes([buf[16], buf[17]]);
            let code = u16::from_ne_bytes([buf[18], buf[19]]);
            let value = i32::from_ne_bytes([buf[20], buf[21], buf[22], buf[23]]);
            let event = match kind {
                1 => InputEvent::Key { code, value },
                2 => InputEvent::Rel { code, value },
                _ => continue,
            };
            if sender.send(event).is_err() {
                break;
            }
        }
    });
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
    let data_device_state = DataDeviceState::new::<State>(&display_handle);
    let xdg_activation_state = XdgActivationState::new::<State>(&display_handle);
    let viewporter_state = ViewporterState::new::<State>(&display_handle);
    let text_input_state = TextInputManagerState::new::<State>(&display_handle);
    let xdg_decoration_state = XdgDecorationState::new::<State>(&display_handle);
    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&display_handle, "seat0");
    seat.add_keyboard(Default::default(), 200, 25)?;
    seat.add_pointer();

    // Chromium asks for an output before it creates its window.
    let output = Output::new(
        "Virtual-1".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "X-Kernel".into(),
            model: "Virtual-1".into(),
        },
    );
    output.change_current_state(
        Some(OutputMode {
            size: (width as i32, height as i32).into(),
            refresh: 60_000,
        }),
        None,
        None,
        None,
    );
    output.set_preferred(OutputMode {
        size: (width as i32, height as i32).into(),
        refresh: 60_000,
    });
    output.create_global::<State>(&display_handle);

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

    let (input_tx, input_rx) = smithay::reexports::calloop::channel::channel::<InputEvent>();
    event_loop
        .handle()
        .insert_source(input_rx, |event, _, state: &mut State| {
            if let smithay::reexports::calloop::channel::Event::Msg(event) = event {
                state.handle_input(event);
            }
        })?;
    for index in 0..8 {
        spawn_evdev(index, input_tx.clone());
    }

    let mut state = State {
        display_handle,
        socket_name: socket_name.clone(),
        space: Space::default(),
        compositor_state,
        xdg_shell_state,
        shm_state,
        output_manager_state,
        data_device_state,
        xdg_activation_state,
        viewporter_state,
        text_input_state,
        xdg_decoration_state,
        output,
        seat_state,
        seat,
        device,
        crtc,
        connector,
        buffer,
        surface,
        framebuffer,
        map,
        map_len,
        pitch,
        width,
        height,
        dirty: true,
        first_commit: true,
        focus: None,
        pointer_location: (0.0, 0.0).into(),
        serial: 0,
        start: std::time::Instant::now(),
        frame: 0,
        selftest_at: None,
        focus_sent: false,
    };

    state.space.map_output(&state.output, (0, 0));

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

    event_loop.run(Some(Duration::from_millis(16)), &mut state, |state: &mut State| {
        let _ = state.display_handle.flush_clients();
        if state.dirty || state.frame % 60 == 0 {
            state.composite();
        }
        // Frame callbacks: clients (Chromium) wait for these before the next
        // frame and before considering the surface fully mapped.
        let output = state.output.clone();
        let windows: Vec<Window> = state.space.elements().cloned().collect();
        for window in windows {
            window.send_frame(
                &output,
                Duration::from_millis(16),
                Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
        state.space.refresh();

        state.frame = state.frame.wrapping_add(1);
        if state.frame % 15 == 0 {
            state.poll_output();
        }
        state.tick_selftest();

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
smithay::delegate_data_device!(State);
smithay::delegate_xdg_decoration!(State);
smithay::delegate_xdg_activation!(State);
smithay::delegate_viewporter!(State);
smithay::delegate_text_input_manager!(State);
