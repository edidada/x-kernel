// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Smithay protocol handler implementations.


use crate::state::{ClientState, State};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::{
    desktop::Window,
    utils::Serial,
    input::{
        Seat, SeatHandler, SeatState,
    },
    reexports::wayland_server::protocol::{wl_buffer, wl_surface::WlSurface},
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState, get_parent},
        output::OutputHandler,

        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
        selection::{
            SelectionHandler,
            data_device::{ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler},
        },
        shell::xdg::{
            ToplevelSurface, XdgShellHandler, XdgShellState,
            decoration::XdgDecorationHandler,
        },
        shm::{ShmHandler, ShmState},
    },
};


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
                tracing::debug!("keyboard focus set after commit");
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
            tracing::debug!("activation: raised window");
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
        tracing::info!("new toplevel");
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


