// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! evdev input forwarding into the seat.

use std::{
    fs::File,
    io::Read,
};

use crate::state::{InputEvent, State};
use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::{
    utils::Serial,
    input::{
        keyboard::Keycode,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
};


impl State {
    pub(crate) fn handle_input(&mut self, event: InputEvent) {
        let time = self.start.elapsed().as_millis() as u32;
        self.serial = self.serial.wrapping_add(1);
        let serial = Serial::from(self.serial);
        match event {
            InputEvent::Key { code, value } if code < 0x100 => {
                tracing::debug!(code, value, "key event");
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

pub(crate) fn spawn_evdev(index: u32, sender: smithay::reexports::calloop::channel::Sender<InputEvent>) {
    let path = format!("/dev/input/event{index}");
    std::thread::spawn(move || {
        let Ok(mut file) = File::open(&path) else {
            tracing::warn!(%path, "cannot open input device");
            return;
        };
        tracing::info!(%path, "input device opened");
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


