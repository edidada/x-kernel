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

/// Command Ring Control Register (xHCI 1.1 §5.4.5).
pub mod crcr {
    /// Consumer Cycle State: the cycle bit expected on the next TRB fetched.
    pub const CS: u32 = 1 << 0;
    /// Command Abort: set by software to stop a running command ring. The
    /// controller aborts the command in progress, clears CRR and posts a
    /// Command Ring Stopped event (TRB type 37).
    pub const CA: u32 = 1 << 2;
    /// Command Ring Running, read-only. Restarting a stopped ring means writing
    /// this register with the new dequeue pointer and CS, then ringing the
    /// command doorbell.
    pub const CRR: u32 = 1 << 3;
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
    /// `COMP_BABBLE_DETECTED_ERROR` (`xhci.h:1105`) — 3, not 7: 7 is
    /// `COMP_TRB_ERROR`. A babbling endpoint halts like a transaction error
    /// (`finish_td()`, 5.10), so the value has to be the reference's.
    pub const BABBLE: u8 = 3;
    /// `COMP_USB_TRANSACTION_ERROR` (`xhci.h:1106`).
    pub const TRANSACTION_ERROR: u8 = 4;
    pub const STALL: u8 = 6;
    /// `COMP_CONTEXT_STATE_ERROR` (`xhci.h:841`): the command was refused
    /// because the slot/endpoint is in the wrong state — a halted endpoint
    /// answers Stop Endpoint with exactly this (`xhci_handle_cmd_stop_ep`).
    pub const CONTEXT_STATE_ERROR: u8 = 19;
    pub const SHORT_PACKET: u8 = 13;
    /// `COMP_SPLIT_TRANSACTION_ERROR` (`xhci.h:868`) — 36, not 5: code 5 is
    /// `COMP_TRB_ERROR`. The two mean opposite things (a wire error halts the
    /// endpoint, a TRB error does not), so the value has to come from the
    /// reference rather than from the ordering of the enum.
    pub const SPLIT_TRANSACTION_ERROR: u8 = 36;
    /// `COMP_COMMAND_RING_STOPPED` (`xhci.h:866`): the command the ring was
    /// processing was abandoned because software aborted the ring.
    pub const COMMAND_RING_STOPPED: u8 = 24;
    /// `COMP_CMD_ABORT`: the command was aborted while in flight.
    pub const COMMAND_ABORTED: u8 = 25;
}

/// TRB types this driver builds or consumes.
pub mod trb {
    pub const NORMAL: u32 = 1;
    pub const SETUP: u32 = 2;
    pub const DATA: u32 = 3;
    pub const STATUS: u32 = 4;
    pub const LINK: u32 = 6;
    /// Link TRB "Toggle Cycle" (bit 1): flip the consumer's cycle bit when it
    /// follows this link, i.e. what makes a wrapping ring work.
    pub const LINK_TOGGLE_CYCLE: u32 = 1 << 1;
    /// Command TRB types start at 9, not 8: 8 is No-Op *Transfer*. Writing 8 here
    /// made the controller answer TRB Error for every `Enable Slot` while a No-Op
    /// Command (23) on the same ring succeeded -- the whole command range had been
    /// numbered from the transfer table.
    pub const ENABLE_SLOT: u32 = 9;
    pub const DISABLE_SLOT: u32 = 10;
    pub const ADDRESS_DEVICE: u32 = 11;
    pub const CONFIGURE_ENDPOINT: u32 = 12;
    /// `TRB_RESET_EP`: returns a halted endpoint to Running without touching its
    /// ring; paired with `SET_TR_DEQUEUE` to re-point it.
    ///
    /// 15 is NOT Reset Endpoint — it is Stop Endpoint (`TRB_STOP_RING`); the
    /// command range is 9 Enable Slot, 10 Disable Slot, 11 Address Device,
    /// 12 Configure Endpoint, 13 Evaluate Context, **14 Reset Endpoint**,
    /// 15 Stop Endpoint, 16 Set TR Dequeue (`xhci.h` `TRB_RESET_EP`).
    pub const RESET_ENDPOINT: u32 = 14;
    /// `TRB_STOP_RING`: halts the endpoint's ring processing where it is; the
    /// dequeue pointer is then moved with `SET_TR_DEQUEUE` (the dequeue half of
    /// `xhci_urb_dequeue()`).
    pub const STOP_ENDPOINT: u32 = 15;
    /// `TRB_SET_DEQ`: hands the endpoint a dequeue pointer (and its DCS) again.
    pub const SET_TR_DEQUEUE: u32 = 16;
    /// `TRB_COMMAND_RING_STOPPED` (37): the controller acknowledged a Command Ring
    /// Abort (CRCR.CA) and stopped fetching the command ring. `xhci_handle_event()`
    /// has a case for it; a driver that treats it as an unknown event keeps
    /// believing the ring is running.
    pub const COMMAND_RING_STOPPED: u32 = 37;
    pub const TRANSFER_EVENT: u32 = 32;
    pub const COMMAND_COMPLETION: u32 = 33;
    pub const PORT_STATUS_CHANGE: u32 = 34;
    /// `TRB_RESET_DEV`: tells the controller the slot's device was reset, so it
    /// drops the context state it kept for the device
    /// (`hcd->driver->reset_device` = `xhci_discover_or_reset_device()`).
    pub const RESET_DEVICE: u32 = 17;
    pub const NO_OP: u32 = 23;
    /// No-Op *Transfer* TRB (type 8). Unlike the No-Op Command (23) it travels
    /// on a slot's transfer ring and completes with a Transfer Event without
    /// touching the device — the only way to tell "this endpoint's ring is not
    /// being consumed" from "the device is not answering".
    pub const NO_OP_TRANSFER: u32 = 8;

    /// Block Set Address Request (`SETUP_CONTEXT_ONLY`): build the contexts
    /// without assigning an address yet.
    pub const BSR: u32 = 1 << 9;
    /// Control (dword 3) field masks.
    pub const CYCLE: u32 = 1 << 0;
    pub const TYPE_SHIFT: u32 = 10;
    pub const TYPE_MASK: u32 = 0x3f << TYPE_SHIFT;
    pub const SLOT_SHIFT: u32 = 24;
    /// Endpoint ID lives in the same control word as the slot ID (bits 23:16);
    /// EP0 is ID 1.
    pub const EP_SHIFT: u32 = 16;
    pub const EP0: u32 = 1;
    pub const SLOT_MASK: u32 = 0xff << SLOT_SHIFT;
    /// Interrupt-on-completion, dword 3 bit 5.
    ///
    /// Bit 4 is CH (Chain), not IOC. Defining IOC as `1 << 4` set the chain bit
    /// on every "last TRB" instead, so a transfer announced three chained TRBs
    /// and completed none: the controller posted no Transfer Event at all and
    /// every control transfer timed out with an otherwise perfect ring
    /// (correct cycle bits, correct TRT, EP0 Running, dequeue matching). Command
    /// TRBs were unaffected — command completions do not need IOC — which is
    /// why the command ring, Enable Slot and Address Device all succeeded while
    /// the first real transfer never finished.
    pub const IOC: u32 = 1 << 5;
    /// Interrupt on Short Packet (dword 3 bit 2).
    pub const ISP: u32 = 1 << 2;
    /// `TRB_TD_SIZE`: packets *remaining after* this TRB, in max-packet units
    /// (`xhci_td_remainder()`). A wrong value makes the controller treat a
    /// multi-TRB transfer as finished after the first TRB.
    pub const TD_SIZE_SHIFT: u32 = 17;
    /// Transfer-completion events report the residual in status bits 23:0.
    pub const STATUS_LEN_MASK: u32 = 0x00ff_ffff;
    /// Chain this TRB to the next one (dword 3 bit 4).
    pub const CHAIN: u32 = 1 << 4;
    /// Direction bit (IN) in Setup/Data stage TRBs, dword 2 bit 16.
    pub const DIR_IN: u32 = 1 << 16;
    /// Immediate data (Setup TRB carries the packet in the parameter word).
    pub const IDT: u32 = 1 << 6;
    /// `TRB_TSP` (`xhci.h:978`): "Transfer State Preserve", which makes Reset
    /// Endpoint the soft variant (`xhci_queue_reset_ep`, xhci-ring.c:4483).
    pub const TSP: u32 = 1 << 9;
    /// Transfer Type on the Setup Stage TRB (dword 2 bits 17:16), xHCI 1.0
    /// §6.4.1.2.1: 2 = OUT data stage, 3 = IN data stage. It is REQUIRED —
    /// with it absent the controller treats the transfer as having no data
    /// stage, never executes the Data TRB, and the transfer never completes
    /// (observed as a control-transfer timeout while the command ring, Enable
    /// Slot and Address Device all returned Success).
    pub const TRT_SHIFT: u32 = 16;
    pub const TRT_OUT: u32 = 2;
    pub const TRT_IN: u32 = 3;
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
