// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/dev/uinput` virtual input device support.
//!
//! Implements the uinput user-space ABI: a writer first configures the device
//! (name, id, event bitmaps) through `UI_SET_*` ioctls, then registers it by
//! writing a `uinput_user_dev` structure followed by `UI_DEV_CREATE`. Events
//! written after the device is created are injected into the device queue and
//! become readable through the matching `/dev/input/eventN` node published by
//! [`add_event_entries`].
//!
//! Devices are intentionally not routed through the `kclass`/`kdevice` device
//! model: uinput devices are pure kernel-side software constructs with no
//! parent bus object, so they live only in this module's registry.

use alloc::{collections::VecDeque, format, string::String, sync::Arc, vec::Vec};
use core::mem::size_of;

use kerrno::{KError, KResult};
use klazy::Lazy;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;
use kvfs::{
    DeviceFileOps, DeviceId, DirMapping, NodeFlags, SimpleFs, VfsFile, VfsFileBuilder, VfsInode,
    VfsResult,
};
use linux_raw_sys::ioctl::{
    UI_ABS_SETUP, UI_DEV_CREATE, UI_DEV_DESTROY, UI_GET_VERSION, UI_SET_ABSBIT, UI_SET_EVBIT,
    UI_SET_FFBIT, UI_SET_KEYBIT, UI_SET_LEDBIT, UI_SET_MSCBIT, UI_SET_PROPBIT, UI_SET_RELBIT,
    UI_SET_SNDBIT, UI_SET_SWBIT,
};
use posix_types::{InputId, UserPtr};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::{DeviceFile, add_device_entry};

/// Size of the fixed part of `struct uinput_user_dev` before the abs arrays.
const UINPUT_USER_DEV_HEADER_SIZE: usize = 80 + 8 + 4;
/// `ABS_CNT` in the Linux ABI.
const ABS_CNT: usize = 64;
/// Full size of `struct uinput_user_dev`.
const UINPUT_USER_DEV_SIZE: usize = UINPUT_USER_DEV_HEADER_SIZE + ABS_CNT * 4 * 4;

/// Linux input protocol event, matching evdev `struct input_event`.
#[repr(C)]
#[derive(Clone, Copy, Debug, FromBytes, Immutable, IntoBytes)]
struct KernelTimeval {
    tv_sec: i64,
    tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, FromBytes, Immutable, IntoBytes)]
struct InputEvent {
    time: KernelTimeval,
    event_type: u16,
    code: u16,
    value: i32,
}

impl InputEvent {
    fn now(event_type: u16, code: u16, value: i32) -> Self {
        let time = ktime::realtime();
        Self {
            time: KernelTimeval {
                tv_sec: time.unix_seconds(),
                tv_usec: (time.subsec_nanos() / 1_000) as i64,
            },
            event_type,
            code,
            value,
        }
    }
}

/// An established uinput virtual device.
struct VirtualDevice {
    name: String,
    id: InputId,
    /// Per event-type bitmaps: `[0]` stores `EV_KEY` codes, `[1]` `EV_REL`,
    /// `[2]` `EV_ABS`, and so on, indexed by event type number.
    ev_bits: [u8; 0x20],
    key_bits: [u8; 0x300usize.div_ceil(8)],
    rel_bits: [u8; 0x10usize.div_ceil(8)],
    abs_bits: [u8; 0x40usize.div_ceil(8)],
    msc_bits: [u8; 0x08usize.div_ceil(8)],
    led_bits: [u8; 0x10usize.div_ceil(8)],
    snd_bits: [u8; 0x08usize.div_ceil(8)],
    sw_bits: [u8; 0x12usize.div_ceil(8)],
    events: Mutex<VecDeque<InputEvent>>,
    poll_rx: PollSet,
}

impl VirtualDevice {
    fn new(name: String, id: InputId) -> Self {
        Self {
            name,
            id,
            ev_bits: [0; 0x20],
            key_bits: [0; 0x300usize.div_ceil(8)],
            rel_bits: [0; 0x10usize.div_ceil(8)],
            abs_bits: [0; 0x40usize.div_ceil(8)],
            msc_bits: [0; 0x08usize.div_ceil(8)],
            led_bits: [0; 0x10usize.div_ceil(8)],
            snd_bits: [0; 0x08usize.div_ceil(8)],
            sw_bits: [0; 0x12usize.div_ceil(8)],
            events: Mutex::new(VecDeque::new()),
            poll_rx: PollSet::new(),
        }
    }

    fn take(&mut self) -> VirtualDevice {
        core::mem::replace(self, VirtualDevice::new(String::new(), InputId::default()))
    }

    fn bitmap_for(&self, event_type: u16) -> Option<&[u8]> {
        match event_type {
            0x01 => Some(&self.key_bits),
            0x02 => Some(&self.rel_bits),
            0x03 => Some(&self.abs_bits),
            0x04 => Some(&self.msc_bits),
            0x11 => Some(&self.led_bits),
            0x12 => Some(&self.snd_bits),
            0x05 => Some(&self.sw_bits),
            _ => None,
        }
    }

    fn set_bit(&mut self, event_type: u16, code: u16) -> bool {
        let bitmap: &mut [u8] = match event_type {
            0x01 => &mut self.key_bits,
            0x02 => &mut self.rel_bits,
            0x03 => &mut self.abs_bits,
            0x04 => &mut self.msc_bits,
            0x11 => &mut self.led_bits,
            0x12 => &mut self.snd_bits,
            0x05 => &mut self.sw_bits,
            _ => return false,
        };
        let index = code as usize;
        if index / 8 >= bitmap.len() {
            return false;
        }
        bitmap[index / 8] |= 1 << (index % 8);
        true
    }

    fn push_event(&self, event: InputEvent) {
        self.events.lock().push_back(event);
        self.poll_rx.wake();
    }
}

/// Global registry of created uinput devices, in creation order.
static DEVICES: Lazy<Mutex<Vec<Arc<VirtualDevice>>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// One in-progress device being configured through `/dev/uinput`.
struct UdeviceSetup {
    name: String,
    id: InputId,
    device: VirtualDevice,
    /// The registered device after `UI_DEV_CREATE`, if any.
    created: Option<Arc<VirtualDevice>>,
}

impl UdeviceSetup {
    fn new() -> Self {
        Self {
            name: String::new(),
            id: InputId::default(),
            device: VirtualDevice::new(String::new(), InputId::default()),
            created: None,
        }
    }
}

/// File state for an open `/dev/uinput` descriptor.
pub struct UinputDev {
    setup: Mutex<UdeviceSetup>,
}

impl UinputDev {
    fn handle_set_bit(&self, event_type: u16, arg: usize) -> VfsResult<()> {
        let code = arg as u16;
        let mut setup = self.setup.lock();
        if setup.created.is_some() {
            return Err(KError::InvalidInput);
        }
        if !setup.device.set_bit(event_type, code) {
            return Err(KError::InvalidInput);
        }
        Ok(())
    }

    fn create_device(&self) -> VfsResult<()> {
        let mut setup = self.setup.lock();
        if setup.created.is_some() {
            return Err(KError::InvalidInput);
        }
        setup.device.name = setup.name.clone();
        setup.device.id = setup.id;
        let device = Arc::new(setup.device.take());
        DEVICES.lock().push(device.clone());
        setup.created = Some(device.clone());
        info!("uinput: created virtual device '{}'", setup.name);
        Ok(())
    }

    fn destroy_device(&self) -> VfsResult<()> {
        let mut setup = self.setup.lock();
        let Some(device) = setup.created.take() else {
            return Err(KError::InvalidInput);
        };
        DEVICES
            .lock()
            .retain(|registered| !Arc::ptr_eq(registered, &device));
        *setup = UdeviceSetup::new();
        info!("uinput: destroyed virtual device '{}'", device.name);
        Ok(())
    }
}

impl DeviceFileOps for UinputDev {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        file.stream_open();
        Ok(())
    }

    fn supports_read(&self) -> bool {
        false
    }

    fn read(&self, _file: &VfsFile, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(KError::InvalidInput)
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        let mut setup = self.setup.lock();
        if buf.is_empty() {
            return Ok(0);
        }
        if setup.created.is_none() {
            // A full `uinput_user_dev` write registers device identity.
            if buf.len() < UINPUT_USER_DEV_SIZE {
                return Err(KError::InvalidInput);
            }
            let name_bytes: [u8; 80] = buf[..80].try_into().unwrap();
            let name_len = name_bytes.iter().position(|&b| b == 0).unwrap_or(80);
            setup.name = String::from_utf8_lossy(&name_bytes[..name_len]).into_owned();
            setup.id = InputId {
                bus_type: u16::from_ne_bytes(buf[80..82].try_into().unwrap()),
                vendor: u16::from_ne_bytes(buf[82..84].try_into().unwrap()),
                product: u16::from_ne_bytes(buf[84..86].try_into().unwrap()),
                version: u16::from_ne_bytes(buf[86..88].try_into().unwrap()),
            };
            setup.device.name = setup.name.clone();
            setup.device.id = setup.id;
            return Ok(UINPUT_USER_DEV_SIZE);
        }

        // After UI_DEV_CREATE, writes are input events for injection into
        // the device created through this descriptor.
        let Some(device) = setup.created.clone() else {
            return Err(KError::NotConnected);
        };
        let mut written = 0;
        for chunk in buf.chunks_exact(size_of::<InputEvent>()) {
            let event = InputEvent::read_from_bytes(chunk).map_err(|_| KError::InvalidInput)?;
            // Events must carry a zero timestamp; the kernel stamps them.
            let event = InputEvent::now(event.event_type, event.code, event.value);
            device.push_event(event);
            written += chunk.len();
        }
        if written == 0 {
            return Err(KError::InvalidInput);
        }
        Ok(written)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    fn ioctl(&self, _file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            UI_GET_VERSION => {
                UserPtr::<u32>::from(arg).write_vm(5)?;
                Ok(0)
            }
            UI_DEV_CREATE => {
                self.create_device()?;
                Ok(0)
            }
            UI_DEV_DESTROY => {
                self.destroy_device()?;
                Ok(0)
            }
            UI_SET_EVBIT => {
                let mut setup = self.setup.lock();
                if setup.created.is_some() || arg > u16::MAX as usize {
                    return Err(KError::InvalidInput);
                }
                let ty = arg as u16;
                if ty as usize / 8 >= setup.device.ev_bits.len() {
                    return Err(KError::InvalidInput);
                }
                setup.device.ev_bits[ty as usize / 8] |= 1 << (ty % 8);
                Ok(0)
            }
            UI_SET_KEYBIT => {
                self.handle_set_bit(0x01, arg)?;
                Ok(0)
            }
            UI_SET_RELBIT => {
                self.handle_set_bit(0x02, arg)?;
                Ok(0)
            }
            UI_SET_ABSBIT => {
                self.handle_set_bit(0x03, arg)?;
                Ok(0)
            }
            UI_SET_MSCBIT => {
                self.handle_set_bit(0x04, arg)?;
                Ok(0)
            }
            UI_SET_LEDBIT => {
                self.handle_set_bit(0x11, arg)?;
                Ok(0)
            }
            UI_SET_SNDBIT => {
                self.handle_set_bit(0x12, arg)?;
                Ok(0)
            }
            UI_SET_SWBIT => {
                self.handle_set_bit(0x05, arg)?;
                Ok(0)
            }
            UI_SET_PROPBIT | UI_SET_FFBIT | UI_ABS_SETUP => Err(KError::InvalidInput),
            _ => Err(KError::InvalidInput),
        }
    }

    fn poll(&self, _file: &VfsFile) -> IoEvents {
        IoEvents::OUT
    }

    fn register_poll(
        &self,
        _file: &VfsFile,
        _context: &mut PollContext<'_>,
        _events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        Ok(())
    }
}

/// The evdev-style node exposed for one uinput virtual device.
struct UinputEventDev {
    device: Arc<VirtualDevice>,
}

/// Adds the event nodes for all registered uinput devices to `/dev/input`.
///
/// `first_uinput_index` is the `eventN` index after the last hardware input
/// device, so uinput nodes never collide with hardware evdev numbering.
pub(crate) fn add_event_entries(inputs: &mut DirMapping, fs: Arc<SimpleFs>, first_index: usize) {
    for (i, device) in DEVICES.lock().iter().enumerate() {
        let event_dev = Arc::new(UinputEventDev {
            device: device.clone(),
        });
        let dev = DeviceFile::new_character(
            fs.clone(),
            DeviceId::new(13, (first_index + i + 1) as _),
            event_dev,
        );
        add_device_entry(inputs, format!("event{}", first_index + i), dev);
    }
}

impl DeviceFileOps for UinputEventDev {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        file.stream_open();
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if buf.len() < size_of::<InputEvent>() {
            return Err(KError::InvalidInput);
        }
        let mut read = 0;
        let mut events = self.device.events.lock();
        for out in buf.chunks_exact_mut(size_of::<InputEvent>()) {
            let Some(event) = events.pop_front() else {
                break;
            };
            out.copy_from_slice(event.as_bytes());
            read += out.len();
        }
        if read == 0 {
            Err(KError::WouldBlock)
        } else {
            Ok(read)
        }
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    fn ioctl(&self, _file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            linux_raw_sys::ioctl::EVIOCGVERSION => {
                UserPtr::<u32>::from(arg).write_vm(0x10001)?;
                Ok(0)
            }
            linux_raw_sys::ioctl::EVIOCGID => {
                UserPtr::<InputId>::from(arg).write_vm(self.device.id)?;
                Ok(0)
            }
            linux_raw_sys::ioctl::EVIOCGRAB => Ok(0),
            other => {
                let mut tmp = other;
                let nr = (tmp & 0xff) as u8;
                tmp >>= 8;
                let ty = (tmp & 0xff) as u8;
                tmp >>= 8;
                let size = (tmp & 0x3fff) as usize;
                tmp >>= 14;
                let dir = tmp & 0x3;

                if ty != b'E' {
                    return Err(KError::InvalidInput);
                }

                if dir == 2 {
                    match nr {
                        // EVIOCGNAME
                        0x06 => return return_str(arg, size, &self.device.name),
                        // EVIOCGPHYS
                        0x07 => return return_str(arg, size, ""),
                        // EVIOCGUNIQ
                        0x08 => return return_str(arg, size, ""),
                        _ => {}
                    }
                    // Variable-length EVIOCGBIT: nr is `EV_TYPE << 5 | map` where
                    // the low 5 bits select the bitmap size class.
                    let ty = (nr >> 5) as u16;
                    let map = nr & 0x1f;
                    if let Some(bits) = self.device.bitmap_for(ty)
                        && map == ty as u8 % 0x20
                    {
                        let len = bits.len().min(size);
                        UserPtr::<u8>::from(arg).write_vm_slice(&bits[..len])?;
                        return Ok(len);
                    }
                }
                Err(KError::InvalidInput)
            }
        }
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

impl Pollable for UinputEventDev {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT;
        events.set(IoEvents::IN, !self.device.events.lock().is_empty());
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.device.poll_rx)?;
        }
        Ok(())
    }
}

fn return_str(arg: usize, size: usize, s: &str) -> KResult<usize> {
    let len = s.len().min(size);
    UserPtr::<u8>::from(arg).write_vm_slice(&s.as_bytes()[..len])?;
    Ok(len)
}

/// Adds the `/dev/uinput` control node to the devfs root.
pub(crate) fn add_root_entry(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    let dev = DeviceFile::new_character(
        fs,
        // The canonical uinput device number is (10, 223).
        DeviceId::new(10, 223),
        Arc::new(UinputDev {
            setup: Mutex::new(UdeviceSetup::new()),
        }),
    );
    add_device_entry(root, "uinput", dev);
}
