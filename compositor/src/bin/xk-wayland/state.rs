// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stage 2: a Wayland compositor that composites SHM client buffers into a
//! dumb buffer and presents it through Smithay's DRM surface.


use crate::drm::{map_buffer, pick_output};

use smithay::backend::input::ButtonState;
use smithay::{
    wayland::shell::xdg::SurfaceCachedState,
    backend::{
        allocator::{Allocator, Fourcc, Modifier, dumb::DumbAllocator},
        drm::{
            DrmDevice, PlaneConfig, PlaneState,
            dumb::{DumbFramebuffer, framebuffer_from_dumb_buffer},
        },
    },
    desktop::{Space, Window},
    output::{Mode as OutputMode, Output},
    utils::Serial,
    input::{
        Seat, SeatState,
        pointer::{ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::{
            DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    utils::{Buffer, Physical, Rectangle, Size, Transform},
    wayland::{
        compositor::{CompositorClientState, CompositorState, with_states},
        output::OutputManagerState,

        xdg_activation::XdgActivationState,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        selection::data_device::DataDeviceState,
        shell::xdg::{
            XdgShellState,
            decoration::XdgDecorationState,
        },
        shm::{ShmState, with_buffer_contents},
    },
};


#[derive(Default)]
pub(crate) struct ClientState {
    pub(crate) compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Several fields only keep a Wayland global alive; they are deliberately never
/// read after construction.
#[allow(dead_code)]
pub(crate) struct State {
    pub(crate) display_handle: DisplayHandle,
    pub(crate) socket_name: std::ffi::OsString,
    pub(crate) space: Space<Window>,
    pub(crate) compositor_state: CompositorState,
    pub(crate) xdg_shell_state: XdgShellState,
    pub(crate) shm_state: ShmState,
    pub(crate) output_manager_state: OutputManagerState,
    pub(crate) data_device_state: DataDeviceState,
    pub(crate) xdg_activation_state: XdgActivationState,
    pub(crate) viewporter_state: ViewporterState,
    pub(crate) text_input_state: TextInputManagerState,
    pub(crate) output: Output,
    pub(crate) xdg_decoration_state: XdgDecorationState,
    pub(crate) seat_state: SeatState<State>,
    pub(crate) seat: Seat<State>,
    pub(crate) device: DrmDevice,
    pub(crate) crtc: smithay::reexports::drm::control::crtc::Handle,
    pub(crate) connector: smithay::reexports::drm::control::connector::Handle,
    pub(crate) buffer: smithay::backend::allocator::dumb::DumbBuffer,
    pub(crate) surface: smithay::backend::drm::DrmSurface,
    pub(crate) framebuffer: DumbFramebuffer,
    pub(crate) map: *mut u8,
    pub(crate) map_len: usize,
    pub(crate) pitch: usize,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) dirty: bool,
    pub(crate) first_commit: bool,
    pub(crate) focus: Option<WlSurface>,
    pub(crate) pointer_location: smithay::utils::Point<f64, smithay::utils::Logical>,
    pub(crate) serial: u32,
    pub(crate) start: std::time::Instant,
    pub(crate) frame: u32,
    pub(crate) selftest_at: Option<u32>,
    pub(crate) focus_sent: bool,
}

pub(crate) enum InputEvent {
    Key { code: u16, value: i32 },
    Rel { code: u16, value: i32 },
}

impl State {
    /// Re-read the host display size and follow it when it changed.
    ///
    /// The DRM connector query asks the backend for the current host size, so a
    /// resized QEMU window shows up here as a new mode.
    pub(crate) fn poll_output(&mut self) {
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

    pub(crate) fn composite(&mut self) {
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
            // Clients that draw their own decorations put them inside the
            // buffer and name the visible window rectangle with
            // xdg_surface.set_window_geometry. Place that rectangle at the
            // window position; the decoration margins fall outside the output.
            let geometry = with_states(surface, |states| {
                states
                    .cached_state
                    .get::<SurfaceCachedState>()
                    .current()
                    .geometry
            });
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
                    let (gx, gy) = geometry.map_or((0, 0), |geo| (geo.loc.x, geo.loc.y));
                    let base_x = location.x - gx;
                    let base_y = location.y - gy;
                    let (skip_x, dst_x) = if base_x < 0 {
                        ((-base_x) as usize, 0usize)
                    } else {
                        (0usize, base_x as usize)
                    };
                    let (skip_y, dst_y) = if base_y < 0 {
                        ((-base_y) as usize, 0usize)
                    } else {
                        (0usize, base_y as usize)
                    };
                    let stride = data.stride as usize;
                    let offset = data.offset as usize;
                    let src_width = data.width as usize;
                    let src_height = data.height as usize;
                    for row in skip_y..src_height {
                        let out_y = dst_y + (row - skip_y);
                        if out_y >= height || dst_x >= width {
                            break;
                        }
                        let copy_width = src_width.saturating_sub(skip_x).min(width - dst_x);
                        if copy_width == 0 {
                            continue;
                        }
                        let src = ptr.wrapping_add(offset + row * stride + skip_x * 4);
                        let dst = map.wrapping_add(out_y * pitch + dst_x * 4);
                        // SAFETY: both ranges are inside the buffer and the mapping.
                        unsafe {
                            std::ptr::copy_nonoverlapping(src as *const u8, dst, copy_width * 4);
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

    pub(crate) fn tick_selftest(&mut self) {
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
}
