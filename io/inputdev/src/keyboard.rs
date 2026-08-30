// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Translate virtio-keyboard `EV_KEY` events into TTY bytes for Links2.

use core::sync::atomic::{AtomicU8, Ordering};

use kclass::prelude::{DriverError, EventType, InputDevice};

use crate::input_snapshot_devices;

const EV_KEY: u16 = EventType::Key as u16;
const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_CAPSLOCK: u16 = 58;
const BTN_MOUSE: usize = 0x110;

const MOD_SHIFT: u8 = 1 << 0;
const MOD_CTRL: u8 = 1 << 1;
const MOD_CAPS: u8 = 1 << 2;

static MODIFIERS: AtomicU8 = AtomicU8::new(0);

fn is_mouse_device(device: &kclass::ClassDevice<kclass::InputDeviceImpl>) -> bool {
    let mut keys = [0u8; 0x300usize.div_ceil(8)];
    match device.get_event_bits(EventType::Key, &mut keys) {
        Ok(true) => keys[BTN_MOUSE / 8] & (1 << (BTN_MOUSE % 8)) != 0,
        _ => false,
    }
}

fn update_modifiers(code: u16, value: u32) {
    let bit = match code {
        KEY_LEFTSHIFT | KEY_RIGHTSHIFT => MOD_SHIFT,
        KEY_LEFTCTRL | KEY_RIGHTCTRL => MOD_CTRL,
        KEY_CAPSLOCK if value == 1 => {
            MODIFIERS.fetch_xor(MOD_CAPS, Ordering::Relaxed);
            return;
        }
        _ => return,
    };
    if value == 0 {
        MODIFIERS.fetch_and(!bit, Ordering::Relaxed);
    } else {
        MODIFIERS.fetch_or(bit, Ordering::Relaxed);
    }
}

fn letter_for_code(code: u16) -> Option<u8> {
    Some(match code {
        16 => b'q',
        17 => b'w',
        18 => b'e',
        19 => b'r',
        20 => b't',
        21 => b'y',
        22 => b'u',
        23 => b'i',
        24 => b'o',
        25 => b'p',
        30 => b'a',
        31 => b's',
        32 => b'd',
        33 => b'f',
        34 => b'g',
        35 => b'h',
        36 => b'j',
        37 => b'k',
        38 => b'l',
        44 => b'z',
        45 => b'x',
        46 => b'c',
        47 => b'v',
        48 => b'b',
        49 => b'n',
        50 => b'm',
        _ => return None,
    })
}

fn map_key(code: u16, mods: u8) -> Option<u8> {
    let shift = mods & MOD_SHIFT != 0;
    let caps = mods & MOD_CAPS != 0;
    let ctrl = mods & MOD_CTRL != 0;

    if let Some(letter) = letter_for_code(code) {
        if ctrl {
            return Some(letter & 0x1f);
        }
        let upper = shift ^ caps;
        return Some(if upper {
            letter.to_ascii_uppercase()
        } else {
            letter
        });
    }

    Some(match code {
        1 => 0x1b,
        14 => 0x7f,
        15 => b'\t',
        28 => b'\r',
        57 => b' ',
        2 => {
            if shift {
                b'!'
            } else {
                b'1'
            }
        }
        3 => {
            if shift {
                b'@'
            } else {
                b'2'
            }
        }
        4 => {
            if shift {
                b'#'
            } else {
                b'3'
            }
        }
        5 => {
            if shift {
                b'$'
            } else {
                b'4'
            }
        }
        6 => {
            if shift {
                b'%'
            } else {
                b'5'
            }
        }
        7 => {
            if shift {
                b'^'
            } else {
                b'6'
            }
        }
        8 => {
            if shift {
                b'&'
            } else {
                b'7'
            }
        }
        9 => {
            if shift {
                b'*'
            } else {
                b'8'
            }
        }
        10 => {
            if shift {
                b'('
            } else {
                b'9'
            }
        }
        11 => {
            if shift {
                b')'
            } else {
                b'0'
            }
        }
        12 => {
            if shift {
                b'_'
            } else {
                b'-'
            }
        }
        13 => {
            if shift {
                b'+'
            } else {
                b'='
            }
        }
        26 => {
            if shift {
                b'{'
            } else {
                b'['
            }
        }
        27 => {
            if shift {
                b'}'
            } else {
                b']'
            }
        }
        39 => {
            if shift {
                b':'
            } else {
                b';'
            }
        }
        40 => {
            if shift {
                b'"'
            } else {
                b'\''
            }
        }
        41 => {
            if shift {
                b'~'
            } else {
                b'`'
            }
        }
        43 => {
            if shift {
                b'|'
            } else {
                b'\\'
            }
        }
        51 => {
            if shift {
                b'<'
            } else {
                b','
            }
        }
        52 => {
            if shift {
                b'>'
            } else {
                b'.'
            }
        }
        53 => {
            if shift {
                b'?'
            } else {
                b'/'
            }
        }
        _ => return None,
    })
}

fn map_escape(code: u16) -> Option<&'static [u8]> {
    match code {
        103 => Some(b"\x1b[A"),
        108 => Some(b"\x1b[B"),
        106 => Some(b"\x1b[C"),
        105 => Some(b"\x1b[D"),
        111 => Some(b"\x1b[3~"),
        _ => None,
    }
}

/// Fill `buf` with TTY bytes translated from pending keyboard input events.
///
/// Mouse devices are skipped so mousedev can consume their events. Only key
/// press and repeat (`value != 0`) produce bytes.
pub fn read_keyboard_bytes(buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let devices = input_snapshot_devices();
    let mut written = 0;
    for device in devices {
        if is_mouse_device(&device) {
            continue;
        }
        while written < buf.len() {
            match device.read_event() {
                Ok(event) => {
                    if event.event_type != EV_KEY {
                        continue;
                    }
                    update_modifiers(event.code, event.value);
                    if event.value == 0 {
                        continue;
                    }
                    let mods = MODIFIERS.load(Ordering::Relaxed);
                    if let Some(bytes) = map_escape(event.code) {
                        let n = bytes.len().min(buf.len() - written);
                        buf[written..written + n].copy_from_slice(&bytes[..n]);
                        written += n;
                    } else if let Some(byte) = map_key(event.code, mods) {
                        buf[written] = byte;
                        written += 1;
                    }
                }
                Err(DriverError::WouldBlock) => break,
                Err(_) => break,
            }
        }
        if written == buf.len() {
            break;
        }
    }
    written
}
