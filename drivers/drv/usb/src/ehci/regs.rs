// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! EHCI register and data-structure layout (EHCI 1.0 §2, §3.5, §3.6).

/// Capability register offsets (relative to the bar).
pub mod cap {
    pub const CAPLENGTH: usize = 0x00;
    pub const HCIVERSION: usize = 0x02;
    pub const HCSPARAMS: usize = 0x04;
    pub const HCCPARAMS: usize = 0x08;
}

/// Operational register offsets (relative to `bar + CAPLENGTH`).
pub mod op {
    pub const USBCMD: usize = 0x00;
    pub const USBSTS: usize = 0x04;
    pub const USBINTR: usize = 0x08;
    pub const FRINDEX: usize = 0x0c;
    pub const CTRLDSSEGMENT: usize = 0x10;
    pub const PERIODICLISTBASE: usize = 0x14;
    pub const ASYNCLISTADDR: usize = 0x18;
    pub const CONFIGFLAG: usize = 0x40;
    /// Port register array: one word per port, 1-based.
    pub const PORTSC_BASE: usize = 0x44;
    pub const PORT_STRIDE: usize = 0x04;
}

pub mod cmd {
    pub const RS: u32 = 1 << 0;
    /// Interrupt on Async Advance Doorbell: forces the controller out of the
    /// reclamation state so it re-fetches the asynchronous list. Software
    /// changes to the list are not guaranteed to be seen without it.
    pub const IAAD: u32 = 1 << 6;
    pub const HCRESET: u32 = 1 << 1;
    pub const PSE: u32 = 1 << 4;
    pub const ASE: u32 = 1 << 5;
    pub const INTR: u32 = 1 << 0;
}

pub mod sts {
    pub const USBINT: u32 = 1 << 0;
    pub const USBERRINT: u32 = 1 << 1;
    pub const PCD: u32 = 1 << 2;
    pub const FLR: u32 = 1 << 3;
    pub const HSE: u32 = 1 << 4;
    /// Controller halted.
    pub const HCH: u32 = 1 << 12;
    pub const HCHALTED: u32 = HCH;
    /// Reclamation: the asynchronous schedule is empty.
    pub const REC: u32 = 1 << 13;
    /// Asynchronous schedule status: the schedule is actually being executed.
    pub const ASS: u32 = 1 << 15;
}

pub mod portsc {
    pub const CCS: u32 = 1 << 0;
    pub const CSC: u32 = 1 << 1;
    pub const PED: u32 = 1 << 2;
    pub const PEC: u32 = 1 << 3;
    pub const OCA: u32 = 1 << 4;
    pub const PR: u32 = 1 << 8;
    pub const SUSP: u32 = 1 << 7;
    pub const RST: u32 = 1 << 8;
    /// Line status, bits [11:10]: 0 SE0, 1 J (full speed), 2 K (low speed).
    pub const LINE_MASK: u32 = 0x3 << 10;
    pub const PP: u32 = 1 << 12;
    pub const PO: u32 = 1 << 13;
    pub const CHANGE_BITS: u32 = CSC | PEC | (1 << 5) | (1 << 6);
}

/// Queue Head, 48 bytes of hardware fields (EHCI §3.6).
#[repr(C, align(64))]
#[derive(Clone, Copy, Default)]
pub struct Qh {
    pub next: u32,
    pub info1: u32,
    pub info2: u32,
    pub current: u32,
    pub qtd_next: u32,
    pub alt_next: u32,
    pub token: u32,
    pub buffer: [u32; 5],
}

/// Queue Element Transfer Descriptor, 32 bytes (EHCI §3.5).
#[repr(C, align(32))]
#[derive(Clone, Copy, Default)]
pub struct Qtd {
    pub next: u32,
    pub alt_next: u32,
    pub token: u32,
    pub buffer: [u32; 5],
}

/// `hw_token` bits, named as in Linux' `ehci.h`.
pub mod token {
    pub const PING: u32 = 1 << 0;
    pub const STS: u32 = 1 << 1;
    pub const MMF: u32 = 1 << 2;
    pub const XACT: u32 = 1 << 3;
    pub const BABBLE: u32 = 1 << 4;
    pub const DBE: u32 = 1 << 5;
    pub const HALT: u32 = 1 << 6;
    pub const ACTIVE: u32 = 1 << 7;
    pub const PID_SHIFT: u32 = 8;
    pub const PID_MASK: u32 = 0x3 << PID_SHIFT;
    pub const CERR_SHIFT: u32 = 10;
    pub const IOC: u32 = 1 << 15;
    pub const LEN_SHIFT: u32 = 16;
    pub const LEN_MASK: u32 = 0x7fff << LEN_SHIFT;
    pub const DT: u32 = 1 << 31;
}

pub mod pid {
    pub const OUT: u32 = 0;
    pub const IN: u32 = 1;
    pub const SETUP: u32 = 2;
}

/// `hw_info1` fields.
pub mod info1 {
    /// NAK Count Reload, `hw_info1[31:28]` (`EHCI_TUNE_RL_HS`): the number of
    /// times a NAKing transaction is retried before the controller reports it.
    /// Zero disables the counter, i.e. retry forever.
    pub const NAK_RELOAD: u32 = 4 << 28;
    /// Control endpoint flag: this QH carries control transfers.
    pub const CONTROL_EP: u32 = 1 << 27;
    pub const MAX_PACKET_SHIFT: u32 = 16;
    pub const MAX_PACKET_MASK: u32 = 0x7ff << MAX_PACKET_SHIFT;
    /// Head of the reclamation list: required for the one QH that is the
    /// asynchronous schedule's entry point.
    pub const HEAD: u32 = 1 << 15;
    /// Data toggle control: use each qTD's own DT bit instead of the queue
    /// head's, which is what a control transfer needs (DATA0 for the setup
    /// stage, DATA1 for the data and status stages).
    pub const TOGGLE_CTL: u32 = 1 << 14;
    pub const EPS_SHIFT: u32 = 12;
    pub const EPS_MASK: u32 = 0x3 << EPS_SHIFT;
    pub const ENDPOINT_SHIFT: u32 = 8;
    pub const ADDRESS_MASK: u32 = 0x7f;
}

/// `hw_info2[31:30]`: transactions per microframe, `EHCI_TUNE_MULT_HS`. The
/// field must be at least 1 for a high-speed endpoint; zero is not a valid
/// value and leaves the controller without a schedule slot for the transfer.
pub mod info2 {
    pub const MULT_HIGH_SPEED: u32 = 1 << 30;
}

/// Endpoint speed, `hw_info1[13:12]`.
pub const fn eps_full() -> u32 {
    0 << 12
}
pub const fn eps_low() -> u32 {
    1 << 12
}
pub const fn eps_high() -> u32 {
    2 << 12
}

/// Marks the end of a list (the hardware's "null pointer").
pub const LIST_END: u32 = 1;

/// Store barrier used between linking a queue head and publishing it in the
/// reclamation list (`qh_link_async()` does the same with `wmb()`).
pub fn list_end_barrier() {
    karch::dsb_ishst();
}
