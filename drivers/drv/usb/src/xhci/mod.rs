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
    /// Slot id handed to us by `Enable Slot`, one per addressed device.
    slots: Vec<u8>,
}

/// Brings the controller to the post-reset state (§4.2).
///
/// Halt, reset, then program the number of enabled device slots. The
/// self-clearing bits are polled: a reset that does not complete is reported,
/// because every later step assumes the controller is idle.
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
        let max_ports = (hcsparams1 & 0xff) as usize;
        let max_slots = (hcsparams1 >> 8) & 0xff;
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
        // Interrupter 0 sits at RTSOFF; its own registers start 0x20 in.
        bar.write32(runtime + 0x08, 0); // ERSTSZ: no segments programmed yet
        bar.write32(runtime + 0x20 + 0x04, 0); // IMAN: no interrupt pending
        bar.write32(runtime + 0x20, 1); // IMOD: one event's worth of delay

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
            ports: max_ports,
            max_slots,
            context_size,
            command,
            event,
            erst,
            device_contexts,
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
        self.bar.write32(self.runtime + 0x08, 1); // ERSTSZ
        self.bar.write64(self.runtime + 0x10, self.erst.bus_addr()); // ERSTBA
        self.bar
            .write64(self.runtime + 0x18, self.event.mem.bus_addr()); // ERDP
        self.bar.write64(
            self.operational + op::DCBAAP,
            self.device_contexts.bus_addr(),
        );
        // CRCR: the ring's base with the initial cycle state (RCS) set.
        self.bar
            .write64(self.operational + op::CRCR, self.command.mem.bus_addr() | 1);
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

    fn port_reset(&mut self, _port: usize) -> KResult<Speed> {
        Err(KError::Unsupported)
    }

    fn control_transfer(
        &mut self,
        _address: u8,
        _setup: &SetupPacket,
        _data: &mut [u8],
    ) -> KResult<usize> {
        Err(KError::Unsupported)
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

const _: () = {
    // The command ring must satisfy xHCI's minimum size.
    assert!(RING_TRBS >= COMMAND_RING_MIN_TRBS);
};
