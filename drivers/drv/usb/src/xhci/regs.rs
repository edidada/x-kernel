// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! xHCI register layout (xHCI 1.1 §5).
//!
//! Three regions, each at a fixed relationship to the next: the capability
//! registers at the bar start, the operational registers at
//! `CAPLENGTH` bytes into the bar, and the runtime, doorbell and port
//! registers at offsets the capability registers report.

/// Capability register offsets (relative to the bar start).
pub mod cap {
    pub const CAPLENGTH: usize = 0x00;
    pub const HCIVERSION: usize = 0x02;
    pub const HCSPARAMS1: usize = 0x04;
    pub const HCSPARAMS2: usize = 0x08;
    pub const HCSPARAMS3: usize = 0x0c;
    pub const HCCPARAMS1: usize = 0x10;
    pub const DBOFF: usize = 0x14;
    pub const RTSOFF: usize = 0x18;
    pub const HCCPARAMS2: usize = 0x1c;
}

/// Operational register offsets (relative to the operational region).
pub mod op {
    pub const USBCMD: usize = 0x00;
    pub const USBSTS: usize = 0x04;
    pub const PAGESIZE: usize = 0x08;
    pub const DNCTRL: usize = 0x14;
    pub const CRCR: usize = 0x18;
    pub const DCBAAP: usize = 0x30;
    pub const CONFIG: usize = 0x38;
    /// Port register array: one 16-byte block per port, 1-based.
    pub const PORTSC_BASE: usize = 0x400;
    pub const PORT_STRIDE: usize = 0x10;
}

pub mod cmd {
    pub const RS: u32 = 1 << 0;
    pub const HCRST: u32 = 1 << 1;
    pub const INTE: u32 = 1 << 2;
    pub const HSEE: u32 = 1 << 3;
    pub const EWE: u32 = 1 << 10;
}

pub mod sts {
    pub const HCH: u32 = 1 << 0;
    pub const HSE: u32 = 1 << 2;
    pub const EINT: u32 = 1 << 3;
    pub const PCD: u32 = 1 << 4;
    pub const CNR: u32 = 1 << 11;
    pub const HCE: u32 = 1 << 12;
}

pub mod portsc {
    /// Current connect status: a device is attached.
    pub const CCS: u32 = 1 << 0;
    /// Port enabled.
    pub const PED: u32 = 1 << 1;
    /// Port reset in progress / requested.
    pub const PR: u32 = 1 << 4;
    /// Port power.
    pub const PP: u32 = 1 << 9;
    /// Port speed field, bits [13:10].
    pub const PSIV_SHIFT: u32 = 10;
    pub const PSIV_MASK: u32 = 0xf << PSIV_SHIFT;
    /// Port link state, bits [8:5].
    pub const PLS_SHIFT: u32 = 5;
    pub const PLS_MASK: u32 = 0xf << PLS_SHIFT;
    /// Connect status change, write-one-to-clear.
    pub const CSC: u32 = 1 << 17;
    /// Port reset change, write-one-to-clear.
    pub const PRC: u32 = 1 << 21;
    /// Port link state change, write-one-to-clear.
    pub const PLC: u32 = 1 << 22;
    /// The change bits this driver acknowledges when it reads a port.
    pub const CHANGE_BITS: u32 = CSC | PRC | PLC | (1 << 18) | (1 << 19) | (1 << 20) | (1 << 23);
    /// Bits that must not be set to 0 by a read-modify-write.
    pub const PRESERVE: u32 = PP | (1 << 16) | (1 << 30);
}

/// Completion codes (`TRB Completion Code`, xHCI 1.1 §6.4.2).
pub mod code {
    pub const SUCCESS: u8 = 1;
    pub const STALL: u8 = 6;
    pub const SHORT_PACKET: u8 = 13;
}

/// TRB types this driver builds or consumes.
pub mod trb {
    pub const NORMAL: u32 = 1;
    pub const SETUP: u32 = 2;
    pub const DATA: u32 = 3;
    pub const STATUS: u32 = 4;
    pub const LINK: u32 = 6;
    pub const ENABLE_SLOT: u32 = 8;
    pub const ADDRESS_DEVICE: u32 = 10;
    pub const CONFIGURE_ENDPOINT: u32 = 11;
    pub const TRANSFER_EVENT: u32 = 32;
    pub const COMMAND_COMPLETION: u32 = 33;
    pub const PORT_STATUS_CHANGE: u32 = 34;
    pub const NO_OP: u32 = 23;

    /// Control (dword 3) field masks.
    pub const CYCLE: u32 = 1 << 0;
    pub const TYPE_SHIFT: u32 = 10;
    pub const TYPE_MASK: u32 = 0x3f << TYPE_SHIFT;
    pub const SLOT_SHIFT: u32 = 24;
    pub const SLOT_MASK: u32 = 0xff << SLOT_SHIFT;
    /// Interrupt-on-completion, dword 2 bit 4.
    pub const IOC: u32 = 1 << 4;
    /// Direction bit (IN) in Setup/Data stage TRBs, dword 2 bit 16.
    pub const DIR_IN: u32 = 1 << 16;
    /// Immediate data (Setup TRB carries the packet in the parameter word).
    pub const IDT: u32 = 1 << 6;
}

/// Port speed (`PSIV`) to bus speed.
pub const fn speed_from_psiv(psiv: u32) -> usb::Speed {
    match psiv {
        1 => usb::Speed::Full,
        2 => usb::Speed::Low,
        3 => usb::Speed::High,
        4 => usb::Speed::Super,
        5 => usb::Speed::SuperPlus,
        _ => usb::Speed::Unknown,
    }
}

/// One 16-byte Transfer Request Block, the unit of every xHCI ring.
#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
pub struct Trb {
    pub parameter: u64,
    pub status: u32,
    pub control: u32,
}

impl Trb {
    pub const fn new(parameter: u64, status: u32, control: u32) -> Self {
        Self {
            parameter,
            status,
            control,
        }
    }

    pub const fn trb_type(self) -> u32 {
        (self.control & trb::TYPE_MASK) >> trb::TYPE_SHIFT
    }

    pub const fn completion_code(self) -> u8 {
        (self.status >> 24) as u8
    }

    pub const fn slot_id(self) -> u8 {
        ((self.control & trb::SLOT_MASK) >> trb::SLOT_SHIFT) as u8
    }

    pub const fn transfer_length(self) -> u32 {
        self.status & 0x00ff_ffff
    }
}
