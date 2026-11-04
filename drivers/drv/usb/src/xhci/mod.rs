// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! xHCI host controller driver.
//!
//! Bring-up order follows xHCI 1.1 §4.2 "Host Controller Initialization":
//! wait for the controller not to be ready, halt it, reset it, program the
//! device context array and the command ring, then the event ring and only
//! then the run bit. Every later transfer is a TRB on one of those rings, so
//! the rings have to be in place before the controller is allowed to run.

// Stage 1 of the port: the rings, the doorbells and the context sizes are in
// place, the transfer TRBs land next; keeping the ring machinery together is
// cheaper to review than adding it back piecemeal.
#![allow(
    dead_code,
    reason = "stage 1 of the port: the ring, doorbell and context machinery is in place, the \
              transfer TRBs that use all of it land next"
)]

pub mod context;
pub mod regs;

use alloc::vec::Vec;

use driver_base::{CoherentMem, Device, IoMap, ProbeError};
use kerrno::{KError, KResult};
use log::{info, warn};
use usb::{Hcd, PortStatus, SetupPacket, Speed};

use crate::xhci::regs::{Trb, cap, cmd, op, portsc, sts};

/// TRB ring capacity in TRBs; xHCI requires a multiple of the page size worth
/// of entries, 256 covers enumeration and leaves room to grow.
pub const RING_TRBS: usize = 256;
/// xHCI 1.1 requires the command ring to be at least 16 TRBs.
const COMMAND_RING_MIN_TRBS: usize = 16;
/// Size of one TRB in bytes.
const TRB_LEN: usize = core::mem::size_of::<Trb>();
/// Runtime register: Event Ring Dequeue Pointer (interrupter 0).
const RT_ERDP: usize = 0x18;
/// Interrupter 0 starts this far into the runtime block; RTSOFF itself points
/// at MFINDEX, so everything at RTSOFF+0x00..0x1f is not an interrupter.
const RT_IR0: usize = 0x20;
/// Writing this bit to ERDP clears the "event handler busy" state.
const ERDP_EHB: u32 = 1 << 3;
/// The controller's scratch buffer for control data stages.
const BOUNCE_LEN: usize = 4096;

/// Events buffered before the interrupter has to be serviced.
const EVENT_RING_TRBS: usize = 256;
/// Contexts are 32 bytes unless `HCCPARAMS1.CSZ` says 64.
const CONTEXT_32: usize = 32;
const CONTEXT_64: usize = 64;

/// A TRB ring: coherent memory the controller fetches from, with a software
/// producer index and the cycle bit that makes a TRB visible to the device.
struct Ring {
    mem: CoherentMem,
    trbs: usize,
    index: usize,
    cycle: bool,
}

impl Ring {
    fn new(mem: CoherentMem, trbs: usize) -> Self {
        Self {
            mem,
            trbs,
            index: 0,
            // The producer starts with the cycle bit set: every TRB begins
            // life as "not mine" from the controller's point of view until the
            // doorbell write that carries the matching cycle bit.
            cycle: true,
        }
    }

    fn trb(&self, index: usize) -> *mut Trb {
        // SAFETY: the ring holds `trbs` 16-byte TRBs and `index < trbs`.
        unsafe { self.mem.as_ptr().cast::<Trb>().add(index % self.trbs) }
    }

    /// Publishes one TRB and returns the physical address the controller
    /// should be pointed at, i.e. the TRB that was just written.
    fn push(&mut self, mut trb: Trb) -> u64 {
        if trb.control & regs::trb::CYCLE == 0 {
            if self.cycle {
                trb.control |= regs::trb::CYCLE;
            } else {
                trb.control &= !regs::trb::CYCLE;
            }
        }
        let index = self.index % self.trbs;
        // SAFETY: `index < trbs` inside this ring's own allocation.
        unsafe {
            core::ptr::write_volatile(self.trb(index), trb);
        }
        let address = self.mem.bus_addr() + (index * core::mem::size_of::<Trb>()) as u64;
        self.index += 1;
        if self.index.is_multiple_of(self.trbs) {
            self.cycle = !self.cycle;
        }
        address
    }

    /// The TRB most recently published (for diagnostics).
    fn trb_at(&self, index: usize) -> Trb {
        self.read(index)
    }

    /// Reads one TRB without claiming ownership of it (event rings are consumed,
    /// not produced).
    fn read(&self, index: usize) -> Trb {
        // SAFETY: `index < trbs` inside this ring's own allocation.
        unsafe { core::ptr::read_volatile(self.trb(index)) }
    }

    fn dequeue_address(&self) -> u64 {
        self.mem.bus_addr() + ((self.index % self.trbs) * core::mem::size_of::<Trb>()) as u64
    }

    fn current_cycle(&self) -> bool {
        self.cycle
    }
}

/// Event Ring Segment Table entry (xHCI 1.1 §6.5).
#[repr(C, align(64))]
#[derive(Clone, Copy, Default)]
struct ErstEntry {
    ring_segment_base: u64,
    ring_segment_size: u32,
    reserved: u32,
}

/// The xHCI controller instance.
pub struct Xhci {
    bar: IoMap,
    /// Operational register base: bar + CAPLENGTH.
    operational: usize,
    /// Runtime register base: bar + RTSOFF.
    runtime: usize,
    /// Doorbell array base: bar + DBOFF.
    doorbell: usize,
    ports: usize,
    max_slots: u32,
    context_size: usize,
    command: Ring,
    event: Ring,
    erst: CoherentMem,
    device_contexts: CoherentMem,
    /// One device-visible scratch buffer for control data stages.
    bounce: CoherentMem,
    /// Event ring consumer position, independent of the producer side.
    event_dequeue: usize,
    event_cycle: bool,
    /// One addressed device slot: its input context (reused for every command)
    /// and the EP0 transfer ring the slot context points at.
    slots: Vec<Slot>,
}

/// One enabled + addressed device slot.
struct Slot {
    id: u8,
    speed: u8,
    input: CoherentMem,
    ep0: Ring,
    /// Device context array entry handed to the controller by `Address Device`.
    device: CoherentMem,
}

/// Brings the controller to the post-reset state (§4.2).
///
/// Halt, reset, then program the number of enabled device slots. The
/// self-clearing bits are polled: a reset that does not complete is reported,
/// because every later step assumes the controller is idle.
/// BRING-UP PROBE: writes a distinctive value to a register and reports every
/// offset in the first 2 KiB of the block that then holds it.
///
/// "The register reads back zero" has two very different causes -- the write was
/// dropped, or it landed somewhere else because the offset is wrong -- and only a
/// search for the value tells them apart.
/// BRING-UP PROBE: logs every non-zero dword in the register window.
///
/// Read-back of one register cannot distinguish "the write was dropped" from
/// "it landed somewhere else", so dump what the window actually holds.
fn dump_nonzero(bar: &IoMap) {
    let mut line = alloc::string::String::new();
    for offset in (0..0x600).step_by(4) {
        let value = bar.read32(offset);
        if value != 0 {
            line.push_str(&alloc::format!(" {offset:#05x}={value:#010x}"));
        }
    }
    info!("xhci: non-zero registers:{}", line);
}

fn scan_marker(bar: &IoMap, value: u32, what: &str) {
    let mut found = 0usize;
    for offset in (0..0x800).step_by(4) {
        if bar.read32(offset) == value {
            info!("xhci: {what} marker {value:#010x} found at bar+{offset:#05x}");
            found += 1;
        }
    }
    if found == 0 {
        info!("xhci: {what} marker {value:#010x} not present anywhere in 0..0x800");
    }
}

fn reset_controller(bar: &IoMap, operational: usize, max_slots: u32) {
    // §4.2: the controller must not be running before it is reset.
    let status = bar.read32(operational + op::USBSTS);
    if status & sts::CNR != 0 {
        warn!("xhci: controller still not ready (USBSTS {status:#010x})");
    }
    bar.write32(operational + op::USBCMD, 0);
    let mut waited = 0;
    while bar.read32(operational + op::USBSTS) & sts::HCH == 0 && waited < RESET_POLL_ITERS {
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1));
        waited += 1;
    }
    if bar.read32(operational + op::USBSTS) & sts::HCH == 0 {
        warn!(
            "xhci: controller did not halt (USBSTS {:#010x})",
            bar.read32(operational + op::USBSTS)
        );
    }

    bar.write32(operational + op::USBCMD, cmd::HCRST);
    let mut waited = 0;
    while bar.read32(operational + op::USBCMD) & cmd::HCRST != 0 && waited < RESET_POLL_ITERS {
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1));
        waited += 1;
    }
    if bar.read32(operational + op::USBCMD) & cmd::HCRST != 0 {
        warn!("xhci: reset did not complete");
    }
    // §4.2: after a reset the controller is "not ready" until it says so, and
    // operational register writes made while CNR is set are ignored. Not waiting
    // here is why the command ring register kept reading zero while DCBAAP --
    // written later, once the controller had become ready -- took its value.
    for _ in 0..200_000 {
        if bar.read32(operational + op::USBSTS) & sts::CNR == 0 {
            break;
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1));
    }
    let status = bar.read32(operational + op::USBSTS);
    if status & sts::CNR != 0 {
        warn!("xhci: controller never became ready after reset (USBSTS {status:#010x})");
    } else {
        info!("xhci: controller ready after reset (USBSTS {status:#010x})");
    }
    // §4.2: everything is written again after a reset; the controller has
    // forgotten the rings, the DCBAA and the number of enabled slots.
    bar.write32(operational + op::CONFIG, max_slots);
}

impl Xhci {
    /// Takes over the controller at `bar`: parses the capability registers,
    /// resets the controller and brings its rings up.
    pub fn init(
        dev: &Device<'_, driver_base::PlatformBus>,
        bar: IoMap,
    ) -> Result<Self, ProbeError> {
        let caplen = bar.read8(cap::CAPLENGTH) as usize;
        let hciversion = bar.read16(cap::HCIVERSION);
        let hcsparams1 = bar.read32(cap::HCSPARAMS1);
        let hcsparams2 = bar.read32(cap::HCSPARAMS2);
        let hccparams1 = bar.read32(cap::HCCPARAMS1);
        let db_offset = bar.read32(cap::DBOFF) & !0x3;
        let rt_offset = bar.read32(cap::RTSOFF) & !0x1f;
        // HCSPARAMS1: bits 7:0 MaxSlots, 15:8 MaxIntrs, 31:24 MaxPorts. Getting
        // these two mixed up reads 64 ports and 1 slot on this controller, which
        // is 2 ports and 64 slots -- and a CONFIG written from the wrong field
        // limits the whole controller to a single device slot.
        let max_slots = hcsparams1 & 0xff;
        let max_ports = (hcsparams1 >> 24) & 0xff;
        let context_size = if hccparams1 & (1 << 2) != 0 {
            CONTEXT_64
        } else {
            CONTEXT_32
        };

        info!(
            "xhci: version {}.{} caplen {caplen} ports {max_ports} slots {max_slots} dboff \
             {db_offset:#x} rtsoff {rt_offset:#x} hcsparams2 {hcsparams2:#010x} hccparams1 \
             {hccparams1:#010x} context {context_size}",
            hciversion >> 8,
            hciversion & 0xff,
        );

        let operational = caplen;
        let runtime = rt_offset as usize;
        let doorbell = db_offset as usize;

        reset_controller(&bar, operational, max_slots);
        // RTSOFF points at MFINDEX; interrupter 0's register set starts 0x20
        // into the runtime block. Writing ERSTSZ/ERSTBA/ERDP at RTSOFF+0x08.. is
        // writing into a reserved hole, which is why those registers never held
        // anything and the controller never had an event ring.
        bar.write32(runtime + RT_IR0 + 0x08, 0); // ERSTSZ0: no segments yet
        bar.write32(runtime + RT_IR0, 0); // IMAN0: no interrupt pending
        bar.write32(runtime + RT_IR0 + 0x04, 0); // IMOD0

        let erst = dev
            .res()
            .alloc_coherent(core::mem::size_of::<ErstEntry>(), 64)?;
        let event = Ring::new(
            dev.res()
                .alloc_coherent(EVENT_RING_TRBS * core::mem::size_of::<Trb>(), 64)?,
            EVENT_RING_TRBS,
        );
        let command = Ring::new(
            dev.res()
                .alloc_coherent(RING_TRBS * core::mem::size_of::<Trb>(), 64)?,
            RING_TRBS,
        );
        let bounce = dev.res().alloc_coherent(BOUNCE_LEN, 64)?;
        let device_contexts = dev
            .res()
            .alloc_coherent(max_slots as usize * core::mem::size_of::<u64>(), 64)?;
        // The controller must find zeroed structures: a stale descriptor
        // written by the previous owner of this memory reads as a valid ring
        // entry, and the numbers here are the sizes of the allocations above.
        for_each_alloc_zeroed(&[
            (erst.as_ptr(), core::mem::size_of::<ErstEntry>()),
            (
                event.mem.as_ptr(),
                EVENT_RING_TRBS * core::mem::size_of::<Trb>(),
            ),
            (
                command.mem.as_ptr(),
                RING_TRBS * core::mem::size_of::<Trb>(),
            ),
            (
                device_contexts.as_ptr(),
                max_slots as usize * core::mem::size_of::<u64>(),
            ),
        ]);

        let mut xhci = Self {
            bar,
            operational,
            runtime,
            doorbell,
            ports: max_ports as usize,
            max_slots,
            context_size,
            command,
            event,
            erst,
            device_contexts,
            bounce,
            event_dequeue: 0,
            // The controller publishes the first event with the cycle bit set.
            event_cycle: true,
            slots: Vec::new(),
        };
        xhci.program_rings()?;
        // §4.2: run only after the rings and the DCBAA are visible.
        xhci.bar.write32(
            xhci.operational + op::USBCMD,
            cmd::RS | cmd::INTE | cmd::EWE,
        );
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        let status = xhci.bar.read32(xhci.operational + op::USBSTS);
        info!("xhci: running, USBSTS {status:#010x}");
        // The command ring register was refused while the controller was
        // stopped. Re-programme it now that it runs and report the read-back; a
        // working board reads a non-zero CRCR here.
        let crcr = xhci.operational + op::CRCR;
        let cmd_addr = xhci.command.mem.bus_addr() | 1;
        // lo_hi_writeq: low half first (Linux writeq order), high half after.
        xhci.bar.write32(crcr, cmd_addr as u32);
        xhci.bar.write32(crcr + 4, (cmd_addr >> 32) as u32);
        info!(
            "xhci: crcr after run {:#018x} (wrote {:#x})",
            xhci.bar.read64(crcr),
            cmd_addr,
        );
        Ok(xhci)
    }

    fn program_rings(&mut self) -> Result<(), ProbeError> {
        // Event Ring Segment Table: one segment covering the whole ring.
        let erst = self.erst.as_ptr().cast::<ErstEntry>();
        // SAFETY: `erst` owns exactly one `ErstEntry`.
        unsafe {
            core::ptr::write_volatile(
                erst,
                ErstEntry {
                    ring_segment_base: self.event.mem.bus_addr(),
                    ring_segment_size: EVENT_RING_TRBS as u32,
                    reserved: 0,
                },
            );
        }
        self.bar.write32(self.runtime + RT_IR0 + 0x08, 1); // ERSTSZ0
        self.bar
            .write64(self.runtime + RT_IR0 + 0x10, self.erst.bus_addr()); // ERSTBA0
        self.bar
            .write64(self.runtime + 0x18, self.event.mem.bus_addr()); // ERDP
        self.bar.write64(
            self.operational + op::DCBAAP,
            self.device_contexts.bus_addr(),
        );
        // CRCR: the ring's base with the initial cycle state (RCS) set.
        let crcr_reg = self.operational + op::CRCR;
        let crcr_addr = self.command.mem.bus_addr() | 1;
        self.bar.write32(crcr_reg, crcr_addr as u32);
        self.bar.write32(crcr_reg + 4, (crcr_addr >> 32) as u32);
        // Read every ring register back: a write that silently does not stick is
        // indistinguishable from a controller that ignores commands, and that
        // cost a round on this port already.
        dump_nonzero(&self.bar);
        scan_marker(&self.bar, self.command.mem.bus_addr() as u32 | 1, "crcr");
        scan_marker(&self.bar, self.erst.bus_addr() as u32, "erstba");
        info!(
            "xhci: rings read back crcr {:#018x} dcbaa {:#x} erstsz {:#x} erstba {:#x} erdp {:#x}",
            self.bar.read64(self.operational + op::CRCR),
            self.bar.read64(self.operational + op::DCBAAP),
            self.bar.read32(self.runtime + RT_IR0 + 0x08),
            self.bar.read64(self.runtime + RT_IR0 + 0x10),
            self.bar.read64(self.runtime + RT_IR0 + 0x18),
        );
        info!(
            "xhci: command ring {:#x}, event ring {:#x}, dcbaa {:#x}",
            self.command.mem.bus_addr(),
            self.event.mem.bus_addr(),
            self.device_contexts.bus_addr(),
        );
        Ok(())
    }

    /// Reads one root port, acknowledging its change bits.
    pub fn port_status_raw(&self, port: usize) -> u32 {
        let offset = self.operational + op::PORTSC_BASE + port * op::PORT_STRIDE;
        let value = self.bar.read32(offset);
        if value & portsc::CHANGE_BITS != 0 {
            self.bar.write32(
                offset,
                (value & !portsc::CHANGE_BITS) | (value & portsc::PRESERVE),
            );
        }
        value
    }

    /// Logs every port's state once. Bring-up evidence: a port that reports
    /// `PP=0` was never powered, one that reports `CCS=1` has a device.
    pub fn log_ports(&self) {
        for port in 0..self.ports {
            let value = self.port_status_raw(port);
            info!(
                "xhci: port {} PORTSC {value:#010x} ccs {} ped {} pr {} pp {} pls {} psiv {}",
                port + 1,
                value & portsc::CCS != 0,
                value & portsc::PED != 0,
                value & portsc::PR != 0,
                value & portsc::PP != 0,
                (value & portsc::PLS_MASK) >> portsc::PLS_SHIFT,
                (value & portsc::PSIV_MASK) >> portsc::PSIV_SHIFT,
            );
        }
    }

    /// Access to the controller's MMIO window, for the parent driver's logs.
    pub const fn bar(&self) -> &IoMap {
        &self.bar
    }

    pub const fn context_size(&self) -> usize {
        self.context_size
    }

    pub const fn max_slots(&self) -> u32 {
        self.max_slots
    }
}

const RESET_POLL_ITERS: usize = 20_000;

/// Zeroes a set of freshly allocated coherent buffers.
///
/// The controller must find zeroed structures: a stale word reads as a valid
/// ring entry or context, and every `(pointer, length)` here is the size of an
/// allocation made immediately above the call.
fn for_each_alloc_zeroed(allocations: &[(*mut u8, usize)]) {
    for (pointer, length) in allocations {
        // SAFETY: the caller passes the base pointer and length of an
        // allocation it owns and has not handed to the controller yet.
        unsafe {
            core::ptr::write_bytes(*pointer, 0, *length);
        }
    }
}

// ---- enumeration: event ring, command ring, control transfers ----

/// How long to wait for the controller to answer a command or a transfer, in
/// 1 µs steps. A device that is not answering must not wedge the boot.
const EVENT_POLL_STEPS: usize = 200_000;

/// xHCI speed id for a bus speed (Table 6-70 `PSIV` / slot context `Speed`).
fn speed_id(speed: Speed) -> u8 {
    match speed {
        Speed::Full => 1,
        Speed::Low => 2,
        Speed::High => 3,
        Speed::Super => 4,
        Speed::SuperPlus => 5,
        Speed::Unknown => 3,
    }
}

impl Xhci {
    /// Takes the next event off the ring, if the controller published one.
    ///
    /// This is the consumer side of the event ring: the cycle bit it compares
    /// against flips every time the dequeue pointer wraps, so a stale TRB from
    /// the previous lap can never be mistaken for a new event.
    fn next_event(&mut self) -> Option<Trb> {
        let trb = self.event.read(self.event_dequeue);
        if trb.control & regs::trb::CYCLE != u32::from(self.event_cycle) {
            return None;
        }
        self.event_dequeue += 1;
        if self.event_dequeue == self.event.trbs {
            self.event_dequeue = 0;
            self.event_cycle = !self.event_cycle;
        }
        // Hand the consumed slot back to the controller (ERDP, clearing EHB).
        let dequeue = self.event.mem.bus_addr() + (self.event_dequeue * TRB_LEN) as u64;
        self.bar.write64(
            self.runtime + RT_IR0 + RT_ERDP,
            dequeue | u64::from(ERDP_EHB),
        );
        Some(trb)
    }

    /// Waits for the event that belongs to `trb_type`, returning it after the
    /// completion code has been checked.
    fn wait_event(&mut self, trb_type: u32, what: &str) -> Result<Trb, KError> {
        for _ in 0..EVENT_POLL_STEPS {
            if let Some(event) = self.next_event() {
                if event.trb_type() == trb_type {
                    return Ok(event);
                }
                // Anything else (a port status change, a stray transfer event)
                // is not ours to consume here; the port scan reads PORTSC
                // directly, so dropping it is safe and cheaper than a queue.
                log::debug!(
                    "xhci: dropped event type {} during {what}",
                    event.trb_type()
                );
                continue;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1));
        }
        // Nothing arrived: report what the event slot actually holds and where
        // the consumer thinks it is, so "controller never wrote" and "we are
        // looking at the wrong address" cannot be confused.
        // Does the controller write into the ring at all? A non-zero TRB here
        // with a zero dequeue means the events arrive and our consumer logic is
        // wrong; all-zero means the ring was never published to the controller.
        let first = self.event.read(0);
        log::error!(
            "xhci: event ring[0] p {:#018x} s {:#010x} c {:#010x}",
            first.parameter,
            first.status,
            first.control,
        );
        let raw = self.event.read(self.event_dequeue);
        log::error!(
            "xhci: {what} timed out after {EVENT_POLL_STEPS} us (dequeue {} cycle {} raw {:#010x} \
             type {})",
            self.event_dequeue,
            self.event_cycle,
            raw.control,
            raw.trb_type(),
        );
        Err(KError::TimedOut)
    }

    /// Issues a command on the command ring and returns its completion event.
    fn command(&mut self, trb: Trb, what: &str) -> Result<Trb, KError> {
        if self.command.index >= self.command.trbs {
            // A command ring that wrapped needs a Link TRB in a production
            // driver; enumeration issues a handful of commands, so the ring is
            // simply not allowed to wrap before that is implemented.
            log::error!("xhci: command ring exhausted during {what}");
            return Err(KError::NoMemory);
        }
        let published = self.command.push(trb);
        let _ = published;
        // Doorbell 0, target 0 is the command ring.
        info!(
            "xhci: {what}: cmd trb {:#010x} crcr {:#018x} usbcmd {:#010x} usbsts {:#010x} dcbaa \
             {:#x}",
            self.command.trb_at(self.command.index - 1).control,
            self.bar.read64(self.operational + op::CRCR),
            self.bar.read32(self.operational + op::USBCMD),
            self.bar.read32(self.operational + op::USBSTS),
            self.bar.read64(self.operational + op::DCBAAP),
        );
        self.bar.write32(self.doorbell, 0);
        let event = self.wait_event(regs::trb::COMMAND_COMPLETION, what)?;
        let code = event.completion_code();
        if code != regs::code::SUCCESS {
            log::error!("xhci: {what} failed, completion code {code}");
            return Err(KError::Io);
        }
        Ok(event)
    }

    /// `Enable Slot`: asks the controller for a device slot.
    fn enable_slot(&mut self) -> Result<u8, KError> {
        // Discriminator: a No-Op command carries no parameters, so if it succeeds
        // the command ring is being fetched correctly and any later TRB Error is
        // about that command's contents rather than about the ring pointer.
        let noop = Trb::new(0, 0, regs::trb::NO_OP << regs::trb::TYPE_SHIFT);
        match self.command(noop, "no-op") {
            Ok(event) => info!(
                "xhci: no-op completed (code {}), command ring works",
                event.completion_code()
            ),
            Err(err) => {
                log::error!("xhci: no-op failed ({err:?}), the command ring is not being fetched")
            }
        }
        let trb = Trb::new(0, 0, regs::trb::ENABLE_SLOT << regs::trb::TYPE_SHIFT);
        let event = self.command(trb, "enable slot")?;
        let slot = event.slot_id();
        if slot == 0 {
            log::error!("xhci: enable slot returned slot 0");
            return Err(KError::Io);
        }
        log::info!("xhci: enabled slot {slot}");
        Ok(slot)
    }

    /// Allocates the per-slot structures (input context, device context, EP0
    /// ring) — everything `Address Device` needs.
    fn allocate_slot(
        &mut self,
        dev: &Device<'_, driver_base::PlatformBus>,
        slot_id: u8,
    ) -> Result<Slot, KError> {
        let context_size = self.context_size;
        // Input context: 1 control context + 32 device contexts.
        let input = dev
            .res()
            .alloc_coherent(context_size * 33, 64)
            .map_err(|_| KError::NoMemory)?;
        let device = dev
            .res()
            .alloc_coherent(context_size * 32, 64)
            .map_err(|_| KError::NoMemory)?;
        let ep0 = Ring::new(
            dev.res()
                .alloc_coherent(RING_TRBS * TRB_LEN, 64)
                .map_err(|_| KError::NoMemory)?,
            RING_TRBS,
        );
        for allocation in [
            (input.as_ptr(), context_size * 33),
            (device.as_ptr(), context_size * 32),
            (ep0.mem.as_ptr(), RING_TRBS * TRB_LEN),
        ] {
            // SAFETY: each pair is the base and length of an allocation made
            // immediately above and not yet handed to the controller.
            unsafe { core::ptr::write_bytes(allocation.0, 0, allocation.1) };
        }
        // The device context array entry has to point at this slot's context
        // block before `Address Device` runs.
        let dcbaa_entry = self.device_contexts.as_ptr().cast::<u64>();
        let entry = dcbaa_entry.wrapping_add(slot_id as usize);
        // SAFETY: `slot_id >= 1` and the DCBAA holds `max_slots` entries.
        unsafe {
            core::ptr::write_volatile(entry, device.bus_addr());
        }
        Ok(Slot {
            id: slot_id,
            speed: 0,
            input,
            ep0,
            device,
        })
    }

    /// `Address Device`: hands the controller the slot and EP0 contexts, which
    /// is what gives the device its USB address.
    fn address_device(
        &mut self,
        slot_index: usize,
        port: usize,
        speed: Speed,
        ep0_max_packet: u16,
    ) -> Result<(), KError> {
        let context_size = self.context_size;
        let (input_addr, ep0_ring, dcs) = {
            let slot = &self.slots[slot_index];
            (
                slot.input.bus_addr(),
                slot.ep0.mem.bus_addr(),
                slot.ep0.cycle,
            )
        };
        let speed_id = speed_id(speed);
        // Contexts are written as dwords: the input control context first, then
        // the slot context at index 0 and EP0 at index 1.
        let base = self.slots[slot_index].input.as_ptr().cast::<u32>();
        let dwords_per_context = context_size / 4;
        // The offsets below stay inside the control context and the first two
        // device contexts of this slot's own input allocation, so every store
        // is a single volatile write to memory the controller does not hold yet.
        let write = |offset: usize, value: u32| {
            let cell = base.wrapping_add(offset);
            // SAFETY: `cell` is `base + offset` inside the input context
            // allocation described above.
            unsafe {
                core::ptr::write_volatile(cell, value);
            }
        };
        // Add slot(0) and EP0(1).
        write(
            1,
            context::input_control_dword1(
                (1 << context::SLOT_CONTEXT) | (1 << context::EP0_CONTEXT),
            ),
        );
        let slot_ctx = dwords_per_context * (context::SLOT_CONTEXT + 1);
        write(slot_ctx, context::slot_dword0(speed_id, 1));
        write(slot_ctx + 1, context::slot_dword1((port + 1) as u8));
        let ep0_ctx = dwords_per_context * (context::EP0_CONTEXT + 1);
        write(ep0_ctx + 1, context::endpoint_dword1(4, ep0_max_packet));
        write(ep0_ctx + 2, context::endpoint_dword2(ep0_ring, dcs));
        write(ep0_ctx + 3, (ep0_ring >> 32) as u32);
        write(ep0_ctx + 4, context::endpoint_dword4(8));
        let slot_id = u32::from(self.slots[slot_index].id);
        let trb = Trb::new(
            input_addr,
            0,
            (regs::trb::ADDRESS_DEVICE << regs::trb::TYPE_SHIFT)
                | (slot_id << regs::trb::SLOT_SHIFT),
        );
        self.command(trb, "address device")?;
        self.slots[slot_index].speed = speed_id;
        log::info!(
            "xhci: slot {slot_id} addressed on port {}, speed {}, ep0 max packet {ep0_max_packet}",
            port + 1,
            speed.as_str(),
        );
        Ok(())
    }

    /// One control transfer on a slot's EP0 ring: setup, optional data, status.
    fn control_transfer_slot(
        &mut self,
        slot_index: usize,
        setup: &usb::SetupPacket,
        data: &mut [u8],
    ) -> Result<usize, KError> {
        let is_in = setup.request_type & 0x80 != 0;
        let length = data.len().min(setup.length as usize);
        let slot_id = self.slots[slot_index].id;
        // The data buffer has to be device-visible: the controller writes into
        // it (IN) or reads from it (OUT), so it cannot be stack memory.
        if length > BOUNCE_LEN {
            return Err(KError::InvalidInput);
        }
        if !is_in && length > 0 {
            // SAFETY: the bounce buffer is BOUNCE_LEN bytes and `length` was
            // bounds-checked against it; the caller's slice holds `length`.
            unsafe {
                core::ptr::copy_nonoverlapping(data.as_ptr(), self.bounce().as_ptr(), length);
            }
        }

        // The setup packet travels in the TRB parameter word (IDT).
        let setup_word = u64::from_le_bytes([
            setup.request_type,
            setup.request,
            (setup.value & 0xff) as u8,
            (setup.value >> 8) as u8,
            (setup.index & 0xff) as u8,
            (setup.index >> 8) as u8,
            (setup.length & 0xff) as u8,
            (setup.length >> 8) as u8,
        ]);
        let mut control = (regs::trb::SETUP << regs::trb::TYPE_SHIFT) | regs::trb::IDT;
        if is_in {
            control |= regs::trb::DIR_IN;
        }
        self.slots[slot_index]
            .ep0
            .push(Trb::new(setup_word, 8, control));
        if length > 0 {
            let mut data_control = regs::trb::DATA << regs::trb::TYPE_SHIFT;
            if is_in {
                data_control |= regs::trb::DIR_IN;
            }
            // IOC only on the last TRB of the transfer.
            data_control |= regs::trb::IOC;
            // SAFETY: as above.
            let bounce_addr = self.bounce.bus_addr();
            self.slots[slot_index]
                .ep0
                .push(Trb::new(bounce_addr, length as u32, data_control));
        }
        // Status stage runs in the opposite direction of the data stage.
        let mut status_control = regs::trb::STATUS << regs::trb::TYPE_SHIFT;
        if !is_in {
            status_control |= regs::trb::DIR_IN;
        }
        if length == 0 {
            status_control |= regs::trb::IOC;
        }
        self.slots[slot_index]
            .ep0
            .push(Trb::new(0, 0, status_control));

        // Doorbell for this slot, target = EP0's device slot context index (1).
        self.bar
            .write32(self.doorbell + 4 * u32::from(slot_id) as usize, 1);
        let event = self.wait_event(regs::trb::TRANSFER_EVENT, "control transfer")?;
        let code = event.completion_code();
        if code != regs::code::SUCCESS && code != regs::code::SHORT_PACKET {
            log::error!("xhci: control transfer completion code {code}");
            return Err(KError::Io);
        }
        let residual = event.transfer_length() as usize;
        let transferred = length.saturating_sub(residual.min(length));
        if is_in && transferred > 0 {
            // SAFETY: as above, inverted direction; `transferred <= length`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.bounce().as_ptr(),
                    data.as_mut_ptr(),
                    transferred,
                );
            }
        }
        Ok(transferred)
    }

    /// The device-visible scratch buffer for control data stages.
    ///
    /// One buffer is enough because enumeration is sequential; a bulk path will
    /// want a pool instead of sharing this.
    fn bounce(&self) -> &CoherentMem {
        &self.bounce
    }

    /// Resets a root port and returns the speed the device negotiated.
    fn reset_port(&mut self, port: usize) -> Result<Speed, KError> {
        let offset = self.operational + op::PORTSC_BASE + port * op::PORT_STRIDE;
        let current = self.bar.read32(offset);
        // PORTSC is write-one-to-clear for the change bits and holds reserved
        // bits that must be preserved exactly as read.
        self.bar
            .write32(offset, (current & portsc::PRESERVE) | portsc::PR);
        for _ in 0..EVENT_POLL_STEPS {
            let value = self.bar.read32(offset);
            if value & portsc::PRC != 0 || (value & portsc::PED != 0 && value & portsc::PR == 0) {
                // Acknowledge every change bit we caused, then report the speed.
                self.bar
                    .write32(offset, (value & portsc::PRESERVE) | portsc::CHANGE_BITS);
                let speed =
                    regs::speed_from_psiv((value & portsc::PSIV_MASK) >> portsc::PSIV_SHIFT);
                log::info!("xhci: port {} reset, speed {}", port + 1, speed.as_str());
                return Ok(speed);
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
        }
        log::error!("xhci: port {} reset timed out", port + 1);
        Err(KError::TimedOut)
    }
}

impl Hcd for Xhci {
    fn port_count(&self) -> usize {
        self.ports
    }

    fn port_status(&self, port: usize) -> KResult<PortStatus> {
        if port >= self.ports {
            return Err(KError::InvalidInput);
        }
        let value = self.port_status_raw(port);
        Ok(PortStatus {
            connected: value & portsc::CCS != 0,
            enabled: value & portsc::PED != 0,
            speed: regs::speed_from_psiv((value & portsc::PSIV_MASK) >> portsc::PSIV_SHIFT),
            changed: value & portsc::CHANGE_BITS != 0,
        })
    }

    fn port_reset(&mut self, port: usize) -> KResult<Speed> {
        self.reset_port(port)
    }

    fn control_transfer(
        &mut self,
        address: u8,
        setup: &SetupPacket,
        data: &mut [u8],
    ) -> KResult<usize> {
        // xHCI uses the slot id as the device address, so the address the bus
        // core passes back is the slot whose EP0 ring we transfer on.
        let slot_index = self
            .slots
            .iter()
            .position(|slot| slot.id == address)
            .ok_or(KError::NotFound)?;
        self.control_transfer_slot(slot_index, setup, data)
    }

    fn bulk_transfer(
        &mut self,
        _address: u8,
        _endpoint: u8,
        _data: &mut [u8],
        _is_in: bool,
    ) -> KResult<usize> {
        Err(KError::Unsupported)
    }

    fn name(&self) -> &'static str {
        "xhci"
    }
}

impl Xhci {
    /// Enumerates every port that reports a connection.
    ///
    /// Ports are scanned once, after the controller runs: a device that is
    /// already attached shows up here, and hot-plug is a port status change
    /// event away once interrupt delivery exists.
    pub fn enumerate(&mut self, dev: &Device<'_, driver_base::PlatformBus>) -> (usize, usize) {
        let mut found = 0usize;
        let mut ready = 0usize;
        // VBUS has just been switched on: a device needs time to see it and
        // pull up, and this port has no connect interrupt to wait on. Poll the
        // ports for a while so a device that attaches "late" is not reported as
        // absent -- that is exactly the confusion this port already paid for.
        for attempt in 0..20 {
            let connected: usize = (0..self.ports)
                .filter(|port| {
                    self.port_status(*port)
                        .map(|status| status.connected)
                        .unwrap_or(false)
                })
                .count();
            if connected > 0 || attempt == 19 {
                if connected > 0 {
                    log::info!(
                        "xhci: {connected} port(s) reported a device after {} ms",
                        attempt * 100
                    );
                }
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100_000));
        }
        for port in 0..self.ports {
            match self.port_status(port) {
                Ok(status) if status.connected => found += 1,
                _ => continue,
            }
            log::info!("xhci: port {} has a device, bringing it up", port + 1);
            // Behaviour, not read-back: a port reset must make the controller
            // write a Port Status Change Event into the event ring. If the ring
            // memory is untouched afterwards, the event ring was never published
            // -- which is a fact about the controller instead of a guess based on
            // a register whose read-back this IP does not honour.
            let before = self.event.read(0);
            log::info!(
                "xhci: event ring[0] before reset p {:#x} c {:#x}",
                before.parameter,
                before.control
            );
            match self.prepare_device(dev, port) {
                Ok(address) => {
                    // The bus core owns every request from here: descriptors,
                    // address assignment, configuration.
                    match usb::enumerate::enumerate_port(self, port, address) {
                        Ok(device) => {
                            log::info!("xhci: {}", device.summary());
                            ready += 1;
                        }
                        Err(err) => {
                            log::error!("xhci: port {} enumeration failed: {err:?}", port + 1)
                        }
                    }
                }
                Err(err) => {
                    let after = self.event.read(0);
                    log::error!(
                        "xhci: port {} could not be prepared: {err:?}; event ring[0] now p {:#x} \
                         c {:#x}",
                        port + 1,
                        after.parameter,
                        after.control
                    );
                }
            }
        }
        (found, ready)
    }

    /// Brings one port up far enough that the bus core can talk to the device:
    /// enable a slot, reset the port, address the device.
    ///
    /// The device then answers on address `slot` and [`usb::enumerate_port`]
    /// asks it what it is.
    pub fn prepare_device(
        &mut self,
        dev: &Device<'_, driver_base::PlatformBus>,
        port: usize,
    ) -> Result<u8, KError> {
        // The port is reset first, as Linux' hub path does: the reset is what
        // brings the device to a known state and makes the controller report it,
        // and `Address Device` afterwards relies on that state.
        let speed = self.reset_port(port)?;
        let slot_id = self.enable_slot()?;
        let slot = self.allocate_slot(dev, slot_id)?;
        self.slots.push(slot);
        let slot_index = self.slots.len() - 1;
        // The control endpoint's packet size follows the speed: high and Super
        // speed devices always use 64, low speed always 8, full speed is told by
        // the device descriptor -- which cannot be read before addressing, so
        // 64 is the safe default for the speeds this port supports.
        let ep0_max_packet = match speed {
            Speed::Low => 8,
            Speed::Full => 8,
            _ => 64,
        };
        self.address_device(slot_index, port, speed, ep0_max_packet)?;
        Ok(slot_id)
    }
}

const _: () = {
    // The command ring must satisfy xHCI's minimum size.
    assert!(RING_TRBS >= COMMAND_RING_MIN_TRBS);
};
