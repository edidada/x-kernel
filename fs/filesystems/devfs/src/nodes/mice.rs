// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux mousedev `/dev/input/mice` PS/2 compatibility node.
//!
//! Links2 framebuffer mode opens this node, writes the IMPS/EXPS probe
//! sequence, and reads 3- or 4-byte PS/2 packets. Events come from the
//! existing input class devices; this module only translates them.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kclass::{
    ClassDevice,
    prelude::{DriverError, Event, EventType, InputDevice, InputDeviceImpl},
};
use kerrno::KError;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;
use ktask::future::{block_on, poll_io};
use ktime_types::TimeSpan;
use kvfs::{DeviceFileOps, VfsFile, VfsFileBuilder, VfsInode, VfsResult};

const EV_SYN: u16 = EventType::Synchronization as u16;
const EV_KEY: u16 = EventType::Key as u16;
const EV_REL: u16 = EventType::Relative as u16;
const EV_ABS: u16 = EventType::Absolute as u16;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_WHEEL: u16 = 8;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

const PACKET_QUEUE_LEN: usize = 16;
const MOUSEDEV_SEQ_LEN: usize = 6;
const IMPS_SEQ: [u8; MOUSEDEV_SEQ_LEN] = [0xf3, 200, 0xf3, 100, 0xf3, 80];
const IMEX_SEQ: [u8; MOUSEDEV_SEQ_LEN] = [0xf3, 200, 0xf3, 200, 0xf3, 80];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseEmul {
    Ps2,
    Imps,
    Exps,
}

#[derive(Clone, Copy, Debug, Default)]
struct Motion {
    dx: i32,
    dy: i32,
    dz: i32,
    buttons: u8,
}

/// PS/2 command/response and packet encoder, matching Linux `mousedev.c`.
pub(crate) struct Ps2MouseState {
    mode: MouseEmul,
    imps_seq: usize,
    imex_seq: usize,
    buttons: u8,
    pending: Motion,
    last_abs_x: Option<i32>,
    last_abs_y: Option<i32>,
    packets: VecDeque<Motion>,
    ready: bool,
    last_buttons: u8,
    response: [u8; 6],
    response_len: usize,
    response_off: usize,
}

impl Default for Ps2MouseState {
    fn default() -> Self {
        Self {
            mode: MouseEmul::Ps2,
            imps_seq: 0,
            imex_seq: 0,
            buttons: 0,
            pending: Motion::default(),
            last_abs_x: None,
            last_abs_y: None,
            packets: VecDeque::new(),
            ready: false,
            last_buttons: 0,
            response: [0; 6],
            response_len: 0,
            response_off: 0,
        }
    }
}

impl Ps2MouseState {
    pub(crate) fn apply_event(&mut self, event: Event) {
        match event.event_type {
            EV_REL => match event.code {
                REL_X => self.pending.dx += event.value as i32,
                REL_Y => self.pending.dy -= event.value as i32,
                REL_WHEEL => self.pending.dz -= event.value as i32,
                _ => {}
            },
            EV_ABS => match event.code {
                ABS_X => {
                    let x = event.value as i32;
                    if let Some(last) = self.last_abs_x {
                        self.pending.dx += x - last;
                    }
                    self.last_abs_x = Some(x);
                }
                ABS_Y => {
                    let y = event.value as i32;
                    if let Some(last) = self.last_abs_y {
                        // Linux mousedev inverts relative Y; keep the same sign
                        // convention for absolute-to-relative conversion.
                        self.pending.dy += y - last;
                    }
                    self.last_abs_y = Some(y);
                }
                _ => {}
            },
            EV_KEY => {
                let index = match event.code {
                    BTN_LEFT => 0,
                    BTN_RIGHT => 1,
                    BTN_MIDDLE => 2,
                    BTN_SIDE => 3,
                    BTN_EXTRA => 4,
                    _ => return,
                };
                if event.value != 0 {
                    self.buttons |= 1 << index;
                } else {
                    self.buttons &= !(1 << index);
                }
            }
            EV_SYN if event.code == SYN_REPORT => self.flush_packet(),
            _ => {}
        }
    }

    fn flush_packet(&mut self) {
        self.pending.buttons = self.buttons;
        let changed = self.pending.dx != 0
            || self.pending.dy != 0
            || self.pending.dz != 0
            || self.pending.buttons != self.last_buttons;
        if changed {
            if self.packets.len() >= PACKET_QUEUE_LEN {
                let _ = self.packets.pop_front();
            }
            self.packets.push_back(self.pending);
            self.ready = true;
        }
        self.pending.dx = 0;
        self.pending.dy = 0;
        self.pending.dz = 0;
    }

    pub(crate) fn write_command(&mut self, byte: u8) {
        if self.imex_seq < MOUSEDEV_SEQ_LEN && byte == IMEX_SEQ[self.imex_seq] {
            self.imex_seq += 1;
            if self.imex_seq == MOUSEDEV_SEQ_LEN {
                self.imex_seq = 0;
                self.mode = MouseEmul::Exps;
            }
        } else {
            self.imex_seq = 0;
        }
        if self.imps_seq < MOUSEDEV_SEQ_LEN && byte == IMPS_SEQ[self.imps_seq] {
            self.imps_seq += 1;
            if self.imps_seq == MOUSEDEV_SEQ_LEN {
                self.imps_seq = 0;
                self.mode = MouseEmul::Imps;
            }
        } else {
            self.imps_seq = 0;
        }
        self.generate_response(byte);
    }

    fn generate_response(&mut self, command: u8) {
        self.response = [0; 6];
        self.response[0] = 0xfa;
        self.response_off = 0;
        match command {
            0xeb => {
                let packet = self.encode_front_packet();
                let n = packet.len();
                self.response[1..1 + n].copy_from_slice(&packet);
                self.response_len = 1 + n;
            }
            0xf2 => {
                self.response[1] = match self.mode {
                    MouseEmul::Ps2 => 0,
                    MouseEmul::Imps => 3,
                    MouseEmul::Exps => 4,
                };
                self.response_len = 2;
            }
            0xe9 => {
                self.response[1] = 0x60;
                self.response[2] = 3;
                self.response[3] = 200;
                self.response_len = 4;
            }
            0xff => {
                self.imps_seq = 0;
                self.imex_seq = 0;
                self.mode = MouseEmul::Ps2;
                self.response[1] = 0xaa;
                self.response[2] = 0x00;
                self.response_len = 3;
            }
            _ => {
                self.response_len = 1;
            }
        }
    }

    fn encode_front_packet(&mut self) -> Vec<u8> {
        let mut packet = [0u8; 4];
        let mut motion = if self.ready {
            self.packets.pop_front().unwrap_or(Motion {
                buttons: self.buttons,
                ..Motion::default()
            })
        } else {
            Motion {
                buttons: self.buttons,
                ..Motion::default()
            }
        };
        self.encode_motion(&mut motion, &mut packet);
        if motion.dx != 0 || motion.dy != 0 || motion.dz != 0 {
            self.packets.push_front(motion);
            self.ready = true;
        } else if self.packets.is_empty() {
            self.ready = false;
            self.last_buttons = motion.buttons;
        }
        packet[..self.packet_size()].to_vec()
    }

    fn packet_size(&self) -> usize {
        match self.mode {
            MouseEmul::Ps2 => 3,
            MouseEmul::Imps | MouseEmul::Exps => 4,
        }
    }

    fn encode_motion(&mut self, motion: &mut Motion, ps2: &mut [u8; 4]) {
        let dx = motion.dx.clamp(-127, 127);
        motion.dx -= dx;
        let dy = motion.dy.clamp(-127, 127);
        motion.dy -= dy;
        ps2[0] = 1 << 3;
        ps2[0] |= ((dx as u8 & 0x80) >> 3) | ((dy as u8 & 0x80) >> 2);
        ps2[0] |= motion.buttons & 0x07;
        ps2[1] = dx as u8;
        ps2[2] = dy as u8;
        match self.mode {
            MouseEmul::Exps => {
                let dz = motion.dz.clamp(-7, 7);
                motion.dz -= dz;
                ps2[3] = (dz as u8 & 0x0f) | ((motion.buttons & 0x18) << 1);
            }
            MouseEmul::Imps => {
                let dz = motion.dz.clamp(-127, 127);
                motion.dz -= dz;
                ps2[0] |= ((motion.buttons & 0x10) >> 3) | ((motion.buttons & 0x08) >> 1);
                ps2[3] = dz as u8;
            }
            MouseEmul::Ps2 => {
                motion.dz = 0;
                ps2[0] |= ((motion.buttons & 0x10) >> 3) | ((motion.buttons & 0x08) >> 1);
            }
        }
    }

    pub(crate) fn has_bytes(&self) -> bool {
        self.response_off < self.response_len || self.ready
    }

    pub(crate) fn read_bytes(&mut self, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        let mut written = 0;
        while written < buf.len() && self.response_off < self.response_len {
            buf[written] = self.response[self.response_off];
            self.response_off += 1;
            written += 1;
        }
        while written < buf.len() && self.ready {
            let packet = self.encode_front_packet();
            let n = packet.len().min(buf.len() - written);
            buf[written..written + n].copy_from_slice(&packet[..n]);
            written += n;
            // Short reads must not drop the rest of a packet: stash remainder.
            if n < packet.len() {
                let remain = &packet[n..];
                self.response[..remain.len()].copy_from_slice(remain);
                self.response_len = remain.len();
                self.response_off = 0;
                break;
            }
        }
        written
    }
}

struct MouseInner {
    devices: Vec<ClassDevice<InputDeviceImpl>>,
    ps2: Ps2MouseState,
}

/// `/dev/input/mice` mix device.
pub struct MouseDev {
    inner: Mutex<MouseInner>,
    poll_set: PollSet,
    opens: AtomicUsize,
    feeder_started: AtomicBool,
}

impl MouseDev {
    pub fn new(devices: Vec<ClassDevice<InputDeviceImpl>>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(MouseInner {
                devices,
                ps2: Ps2MouseState::default(),
            }),
            poll_set: PollSet::new(),
            opens: AtomicUsize::new(0),
            feeder_started: AtomicBool::new(false),
        })
    }

    fn pump_events(&self) -> bool {
        let mut inner = self.inner.lock();
        let mut got = false;
        let device_count = inner.devices.len();
        for index in 0..device_count {
            loop {
                let event = {
                    let device = &inner.devices[index];
                    match device.read_event() {
                        Ok(event) => event,
                        Err(DriverError::WouldBlock) => break,
                        Err(err) => {
                            warn!("mousedev: failed to read event: {err:?}");
                            break;
                        }
                    }
                };
                inner.ps2.apply_event(event);
                got = true;
            }
        }
        got
    }

    fn start_feeder(self: &Arc<Self>) {
        if self.feeder_started.swap(true, Ordering::Relaxed) {
            return;
        }
        let this = Arc::clone(self);
        ktask::spawn_with_name(
            move || loop {
                let got = this.pump_events();
                if got {
                    this.poll_set.wake();
                }
                let delay = if this.opens.load(Ordering::Relaxed) == 0 {
                    TimeSpan::from_millis(250)
                } else if got {
                    TimeSpan::from_millis(8)
                } else {
                    TimeSpan::from_millis(16)
                };
                ktask::sleep(delay);
            },
            "mousedev".into(),
        );
    }

    fn try_read(&self, buf: &mut [u8]) -> Result<usize, KError> {
        let mut inner = self.inner.lock();
        if !inner.ps2.has_bytes() {
            return Err(KError::WouldBlock);
        }
        let n = inner.ps2.read_bytes(buf);
        if n == 0 {
            Err(KError::WouldBlock)
        } else {
            Ok(n)
        }
    }
}

impl DeviceFileOps for MouseDev {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        file.stream_open();
        self.opens.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> VfsResult<()> {
        let _ = self
            .opens
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(1))
            });
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        block_on(poll_io(self, IoEvents::IN, file.is_nonblocking(), || {
            self.try_read(buf)
        }))
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        {
            let mut inner = self.inner.lock();
            for &byte in buf {
                inner.ps2.write_command(byte);
            }
        }
        self.poll_set.wake();
        Ok(buf.len())
    }

    fn poll(&self, _file: &VfsFile) -> IoEvents {
        Pollable::poll(self)
    }

    fn register_poll(
        &self,
        _file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        Pollable::register(self, context, events)
    }
}

impl Pollable for MouseDev {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT;
        events.set(IoEvents::IN, self.inner.lock().ps2.has_bytes());
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.poll_set)?;
        }
        Ok(())
    }
}

/// Linux mousedev mix minor (`MOUSEDEV_MIX`).
pub const MOUSEDEV_MIX_MINOR: u32 = 63;

pub fn attach_feeder(dev: &Arc<MouseDev>) {
    dev.start_feeder();
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn imps_then_exps_probe_returns_ids() {
        let mut ps2 = Ps2MouseState::default();
        for byte in [0xf3, 200, 0xf3, 100, 0xf3, 80, 0xf2] {
            ps2.write_command(byte);
        }
        let mut buf = [0u8; 2];
        assert_eq!(ps2.read_bytes(&mut buf), 2);
        assert_eq!(buf, [0xfa, 3]);

        for byte in [0xf3, 200, 0xf3, 200, 0xf3, 80, 0xf2] {
            ps2.write_command(byte);
        }
        assert_eq!(ps2.read_bytes(&mut buf), 2);
        assert_eq!(buf, [0xfa, 4]);
    }

    #[def_test]
    fn relative_motion_encodes_ps2_and_allows_short_read() {
        let mut ps2 = Ps2MouseState::default();
        ps2.apply_event(Event::new(EV_REL, REL_X, 5));
        ps2.apply_event(Event::new(EV_REL, REL_Y, 3));
        ps2.apply_event(Event::new(EV_SYN, SYN_REPORT, 0));

        let mut one = [0u8; 1];
        assert_eq!(ps2.read_bytes(&mut one), 1);
        assert_eq!(one[0] & (1 << 3), 1 << 3);

        let mut rest = [0u8; 8];
        let n = ps2.read_bytes(&mut rest);
        assert_eq!(n, 2);
        assert_eq!(rest[0], 5);
        // Linux inverts REL_Y, so +3 becomes -3, encoded as 0xfd.
        assert_eq!(rest[1], (-3i8) as u8);
    }

    #[def_test]
    fn left_button_and_wheel_use_exps_packet() {
        let mut ps2 = Ps2MouseState::default();
        for byte in [0xf3, 200, 0xf3, 200, 0xf3, 80, 0xf2] {
            ps2.write_command(byte);
        }
        let mut id = [0u8; 2];
        ps2.read_bytes(&mut id);

        ps2.apply_event(Event::new(EV_KEY, BTN_LEFT, 1));
        ps2.apply_event(Event::new(EV_REL, REL_WHEEL, 1));
        ps2.apply_event(Event::new(EV_SYN, SYN_REPORT, 0));

        let mut pkt = [0u8; 4];
        assert_eq!(ps2.read_bytes(&mut pkt), 4);
        assert_eq!(pkt[0] & 1, 1);
        // Wheel +1 is inverted to dz=-1, low nibble 0xf.
        assert_eq!(pkt[3] & 0x0f, 0x0f);
    }
}
