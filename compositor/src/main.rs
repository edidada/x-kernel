// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A Wayland kiosk compositor that composites SHM client buffers into a dumb
//! buffer and presents it through Smithay's DRM surface.

use std::{
    sync::Arc,
    time::Duration,
};

use smithay::{
    backend::{
        allocator::{Allocator, Fourcc, Modifier, dumb::DumbAllocator},
        drm::dumb::framebuffer_from_dumb_buffer,
    },
    desktop::{Space, Window},
    output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel},
    input::SeatState,
    reexports::{
        calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic},
        wayland_server::Display,
    },
    wayland::{
        compositor::CompositorState,
        output::OutputManagerState,

        xdg_activation::XdgActivationState,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        selection::data_device::DataDeviceState,
        shell::xdg::{
            XdgShellState,
            decoration::XdgDecorationState,
        },
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};


mod drm;
mod handlers;
mod input;
mod state;

use drm::{map_buffer, open_device, pick_output};
use input::{spawn_evdev};
use state::{ClientState, InputEvent, State};

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
