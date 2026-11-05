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
    /// A ring whose last slot is a Link TRB back to its own base.
    ///
    /// `mem` must hold `trbs + 1` TRBs. xHCI requires every ring segment to end
    /// with a Link TRB — the reference refuses to enqueue otherwise ("Missing
    /// link TRB at end of ring segment") — because the controller follows that
    /// link to wrap; without one it walks off the end of the allocation.
    fn with_link(mem: CoherentMem, trbs: usize) -> Self {
        let ring = Self {
            mem,
            trbs,
            index: 0,
            cycle: true,
        };
        ring.write_link(true);
        ring
    }

    /// The event ring: a consumer-owned ring with no Link TRB.
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

    /// (Re)writes the Link TRB that closes the ring, with the producer's current
    /// cycle bit — the controller follows it only when the bits agree.
    fn write_link(&self, cycle: bool) {
        let base = self.mem.bus_addr();
        let slot = self.mem.as_ptr().cast::<Trb>();
        // SAFETY: the allocation holds `trbs + 1` TRBs and this is the last.
        unsafe {
            let link = slot.add(self.trbs);
            core::ptr::write_volatile(link.cast::<u64>(), base);
            core::ptr::write_volatile(link.cast::<u8>().add(8).cast::<u32>(), 0);
            karch::dsb_ishst();
            let control = (regs::trb::LINK << regs::trb::TYPE_SHIFT)
                | regs::trb::LINK_TOGGLE_CYCLE
                | u32::from(cycle);
            core::ptr::write_volatile(link.cast::<u8>().add(12).cast::<u32>(), control);
        }
    }

    fn trb(&self, index: usize) -> *mut Trb {
        // SAFETY: the ring holds `trbs` 16-byte TRBs and `index < trbs`.
        unsafe { self.mem.as_ptr().cast::<Trb>().add(index % self.trbs) }
    }

    /// Queues one TRB with its cycle bit inverted, so it stays invisible.
    ///
    /// `giveback_first_trb()` writes every TRB of a transfer descriptor first
    /// and only then flips the first TRB's cycle bit, which is what stops the
    /// controller from starting to execute a TD whose later TRBs are still being
    /// written. The inverse bit is used because that is what the reference does:
    /// a held TRB carries the opposite of the ring's cycle, and publishing it
    /// writes the real one.
    fn push_held(&mut self, mut trb: Trb) -> (usize, bool) {
        let index = self.index % self.trbs;
        let cycle = self.cycle;
        if cycle {
            trb.control &= !regs::trb::CYCLE;
        } else {
            trb.control |= regs::trb::CYCLE;
        }
        let slot = self.trb(index);
        let base = slot.cast::<u8>();
        // SAFETY: `base` is the first byte of one TRB inside this ring's own
        // allocation; the offsets below stay inside its 16 bytes.
        unsafe {
            core::ptr::write_volatile(base.cast::<u64>(), trb.parameter);
            core::ptr::write_volatile(base.wrapping_add(8).cast::<u32>(), trb.status);
        }
        karch::dsb_ishst();
        // SAFETY: dword 3 of the same TRB.
        unsafe {
            core::ptr::write_volatile(base.wrapping_add(12).cast::<u32>(), trb.control);
        }
        self.index += 1;
        if self.index == self.trbs {
            self.index = 0;
            self.cycle = !self.cycle;
            self.write_link(self.cycle);
        }
        (index, cycle)
    }

    /// Makes a held TRB visible: the last write of the transfer descriptor.
    fn publish(&self, held: (usize, bool)) {
        let (index, cycle) = held;
        let control = self.trb(index).cast::<u8>().wrapping_add(12).cast::<u32>();
        // SAFETY: dword 3 of TRB `index` in this ring's own allocation.
        let current = unsafe { core::ptr::read_volatile(control) };
        let updated = if cycle {
            current | regs::trb::CYCLE
        } else {
            current & !regs::trb::CYCLE
        };
        // SAFETY: as above. The payload of this TRB was written and barrier-ed
        // by `push_held`, so this store is the one that hands it over.
        unsafe {
            core::ptr::write_volatile(control, updated);
        }
        karch::dsb_ishst();
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
        let slot = self.trb(index);
        // Publish in the order the controller expects (Linux `queue_trb`): the
        // payload words first, then a store barrier, and the control word -- which
        // carries the cycle bit -- last. The cycle bit is what makes the TRB
        // visible, so writing it together with the payload lets the controller
        // fetch a TRB whose parameter/status are still in flight; `wmb()` is
        // `dsb(st)` on arm64, i.e. stronger than the `dmb` a Rust fence emits.
        let base = slot.cast::<u8>();
        // SAFETY: `base` is the first byte of one TRB inside this ring's own
        // allocation, and the offsets below stay inside its 16 bytes.
        unsafe {
            core::ptr::write_volatile(base.cast::<u64>(), trb.parameter);
        }
        let status_ptr = base.wrapping_add(8).cast::<u32>();
        let control_ptr = base.wrapping_add(12).cast::<u32>();
        // SAFETY: dword 2 of the same TRB.
        unsafe {
            core::ptr::write_volatile(status_ptr, trb.status);
        }
        karch::dsb_ishst();
        // SAFETY: dword 3 of the same TRB.
        unsafe {
            core::ptr::write_volatile(control_ptr, trb.control);
        }
        let address = self.mem.bus_addr() + (index * core::mem::size_of::<Trb>()) as u64;
        self.index += 1;
        if self.index == self.trbs {
            // Step over the Link TRB: the producer never writes into it, it
            // re-arms it with the new cycle bit so the controller keeps wrapping.
            self.index = 0;
            self.cycle = !self.cycle;
            self.write_link(self.cycle);
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
    /// Scratchpad buffers required by the controller (`HCSPARAMS2`), kept alive
    /// for its lifetime; `DCBAA[0]` points at the pointer array.
    _scratchpads: Option<Scratchpads>,
    /// One device-visible scratch buffer for control data stages.
    bounce: CoherentMem,
    /// Event ring consumer position, independent of the producer side.
    event_dequeue: usize,
    event_cycle: bool,
    /// One addressed device slot: its input context (reused for every command)
    /// and the EP0 transfer ring the slot context points at.
    slots: Vec<Slot>,
    /// Slot whose contexts a Block-Set-Address-Request `Address Device` installed,
    /// i.e. the device still answering on the default address 0. xHCI keeps such
    /// a device reachable only through its slot.
    address_zero_slot: Option<u8>,
}

/// The scratchpad buffer pool a controller asks for.
struct Scratchpads {
    _array: CoherentMem,
    _buffers: CoherentMem,
}

/// Allocates the scratchpad buffers `HCSPARAMS2` asks for.
///
/// xHCI 4.2: `Max Scratchpad Buffers = HCSPARAMS2[31:27] << 5 | HCSPARAMS2[25:21]`
/// (Linux `HCS_MAX_SCRATCHPAD`). When it is non-zero the DCBAA's first entry must
/// point at an array of pointers to that many buffers *before* the controller is
/// started; without it the first command that allocates a device slot (`Enable
/// Slot`) fails with a TRB error while a No-Op still succeeds.
fn allocate_scratchpads(
    dev: &Device<'_, driver_base::PlatformBus>,
    hcsparams2: u32,
    device_contexts: &CoherentMem,
) -> Result<Option<Scratchpads>, ProbeError> {
    let count = ((((hcsparams2 >> 27) & 0x1f) << 5) | ((hcsparams2 >> 21) & 0x1f)) as usize;
    if count == 0 {
        return Ok(None);
    }
    let array = dev
        .res()
        .alloc_coherent(count * core::mem::size_of::<u64>(), 64)?;
    let buffers = dev.res().alloc_coherent(count * 4096, 4096)?;
    for index in 0..count {
        let entry = array.as_ptr().cast::<u64>().wrapping_add(index);
        // SAFETY: `index < count` inside the array allocation.
        unsafe {
            core::ptr::write_volatile(entry, buffers.bus_addr() + (index * 4096) as u64);
        }
    }
    let slot_zero = device_contexts.as_ptr().cast::<u64>();
    // SAFETY: `slot_zero` is the first entry of the DCBAA allocation.
    unsafe {
        core::ptr::write_volatile(slot_zero, array.bus_addr());
    }
    info!(
        "xhci: {count} scratchpad buffers, array at {:#x}",
        array.bus_addr()
    );
    Ok(Some(Scratchpads {
        _array: array,
        _buffers: buffers,
    }))
}

/// One enabled + addressed device slot.
struct Slot {
    id: u8,
    speed: u8,
    /// Root port this slot was addressed on (`None` until `Address Device`
    /// completes), so the addressed address can be reported per port.
    port: Option<usize>,
    input: CoherentMem,
    ep0: Ring,
    /// Bulk endpoint rings, created when the device's endpoints are configured
    /// (index 0 = EP1-OUT, 1 = EP1-IN); `None` until then.
    bulk: [Option<Ring>; 2],
    /// The hub's status-change (interrupt IN) ring, created with the hub's
    /// endpoint configuration.
    interrupt: Option<Ring>,
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
/// Zeroes interrupter 0 before the rings are programmed.
///
/// RTSOFF points at MFINDEX; the primary interrupter starts 0x20 into the runtime
/// block. Writing ERSTSZ/ERSTBA/ERDP at RTSOFF+0x08.. is writing into a reserved
/// hole, which is why those registers never held anything.
fn clear_interrupter(bar: &IoMap, runtime: usize) {
    bar.write32(runtime + RT_IR0 + 0x08, 0); // ERSTSZ0: no segments yet
    bar.write32(runtime + RT_IR0, 0); // IMAN0: no interrupt pending
    bar.write32(runtime + RT_IR0 + 0x04, 0); // IMOD0
}

/// Logs the register set that was read from a working board at the same offsets,
/// so "what still differs" is a diff instead of a hunt.
fn log_diff_set(bar: &IoMap, operational: usize, runtime: usize) {
    info!(
        "xhci: diff-set gsbuscfg0 {:#010x} gctl {:#010x} gsts {:#010x} gsnpsid {:#010x}",
        bar.read32(0xc100),
        bar.read32(0xc110),
        bar.read32(0xc118),
        bar.read32(0xc120),
    );
    info!(
        "xhci: diff-set usbcmd {:#010x} usbsts {:#010x} dnctrl {:#010x} config {:#010x} crcr \
         {:#010x}",
        bar.read32(operational + op::USBCMD),
        bar.read32(operational + op::USBSTS),
        bar.read32(operational + 0x14),
        bar.read32(operational + op::CONFIG),
        bar.read32(operational + op::CRCR),
    );
    info!(
        "xhci: diff-set guctl {:#010x} guctl1 {:#010x} guctl2 {:#010x} gfladj {:#010x} gbuscfg1 \
         {:#010x} txfifo0 {:#04x} rxfifo0 {:#04x}",
        bar.read32(0xc12c),
        bar.read32(0xc11c),
        bar.read32(0xc19c),
        bar.read32(0xc630),
        bar.read32(0xc104),
        bar.read32(0xc300) & 0xffff,
        bar.read32(0xc380) & 0xffff,
    );
    info!(
        "xhci: diff-set iman {:#x} imod {:#x} erstsz {:#x}",
        bar.read32(runtime + RT_IR0),
        bar.read32(runtime + RT_IR0 + 0x04),
        bar.read32(runtime + RT_IR0 + 0x08),
    );
}

/// Takes the controller over from the firmware's legacy (SMI) interface.
///
/// xHCI 7.1: the USB Legacy Support extended capability carries a BIOS/OS
/// ownership semaphore. While the firmware still owns the controller it keeps
/// running in its legacy mode, where commands such as `Enable Slot` are refused
/// with a TRB error even though the ring is fine and a No-Op still completes.
/// Both working implementations on this board hand it over first (Linux in
/// `quirk_usb_handoff_xhci`, tgoskits in its `legacy_init`).
fn claim_legacy_ownership(bar: &IoMap, hccparams1: u32) {
    let mut offset = (((hccparams1 >> 16) & 0xffff) as usize) * 4;
    if offset == 0 || offset >= 0x1000 {
        info!("xhci: no extended capability list to hand off");
        return;
    }
    loop {
        let header = bar.read32(offset);
        let id = header & 0xff;
        let next = (((header >> 8) & 0xff) as usize) * 4;
        if id == 1 {
            let legsup = bar.read32(offset);
            let bios_owned = legsup & (1 << 16) != 0;
            info!(
                "xhci: legacy support at {offset:#x}: legsup {legsup:#010x} (bios_owned \
                 {bios_owned})",
            );
            if bios_owned {
                bar.write32(offset, legsup | (1 << 24));
                for _ in 0..100_000 {
                    if bar.read32(offset) & (1 << 16) == 0 {
                        break;
                    }
                    khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
                }
                info!("xhci: legacy claimed, legsup {:#010x}", bar.read32(offset));
            }
            bar.write32(offset + 4, 0);
            info!(
                "xhci: legacy SMI control cleared ({:#010x})",
                bar.read32(offset + 4)
            );
            return;
        }
        if next == 0 || offset + next >= 0x1000 {
            break;
        }
        offset += next;
    }
    info!("xhci: no legacy support capability found");
}

/// How a device is reached: through the root port directly, or through a
/// transaction translator of a hub.
#[derive(Debug, Clone, Copy, Default)]
struct Addressing {
    /// xHCI route string: one nibble per hub tier, lowest nibble = first tier.
    route: u32,
    /// Set for a full/low-speed device behind a high-speed hub.
    tt: Option<Translator>,
    /// The parent hub has one transaction translator per port (bDeviceProtocol 2).
    multi_tt: bool,
    /// This device is itself a hub (slot context "Hub" flag, bit 26).
    hub: bool,
}

impl Addressing {
    const fn root() -> Self {
        Self {
            route: 0,
            tt: None,
            multi_tt: false,
            hub: false,
        }
    }
}

/// The transaction translator of the hub a device hangs from.
#[derive(Debug, Clone, Copy)]
struct Translator {
    /// TT think time, copied from the hub descriptor's wHubCharacteristics.
    think_time: u8,
    /// Hub port number the device is on.
    port: u8,
    /// Device slot id of that hub.
    hub_slot: u8,
}

/// `bDeviceClass` of a hub (`bDeviceClass = 09h`, USB 2.0 §11.23).
const HUB_CLASS: u8 = 0x09;

/// Whether a port's link state means it is still negotiating.
///
/// xHCI §4.19.1: 5 RxDetect, 6 Inactive, 4 Disabled and 0/1/2/3 (U0/U1/U2/U3)
/// are settled; 7 Polling, 8 Recovery, 9 Hot Reset, 10 Compliance and 15 Resume
/// are transient. A SuperSpeed port sits in RxDetect until a far-end
/// termination appears and then walks through Polling — that walk is what a
/// single snapshot misses.
fn is_transient_pls(pls: u32) -> bool {
    matches!(pls, 7..=10 | 15)
}

/// 64-bit register write in the order the DWC3 wrapper requires.
///
/// Linux hands its xHCI platform device the `write-64-hi-lo-quirk`, i.e. on this
/// IP the high half goes first -- the opposite of the generic `lo_hi_writeq`.
/// Registers whose upper half is zero (DCBAAP, ERSTBA) hide the difference; the
/// ones that carry flags in the low word do not.
fn write64_hi_lo(bar: &IoMap, offset: usize, value: u64) {
    bar.write32(offset + 4, (value >> 32) as u32);
    bar.write32(offset, value as u32);
}

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
    // Capability registers are only defined once CNR has dropped: reading them
    // earlier returns whatever the block drives while it is still initialising.
    // HCSPARAMS1 in particular carries MaxPorts, so a read during that window
    // reports a port count that has nothing to do with the hardware (observed
    // the same controller report MaxPorts 2 and 1 on different boots), and a
    // controller believed to have one port never scans its SuperSpeed port.
    let hcsparams1_ready = bar.read32(cap::HCSPARAMS1);
    info!(
        "xhci: HCSPARAMS1 after ready {hcsparams1_ready:#010x} (max ports {}, max slots {})",
        (hcsparams1_ready >> 24) & 0xff,
        hcsparams1_ready & 0xff,
    );
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
        claim_legacy_ownership(&bar, hccparams1);
        clear_interrupter(&bar, runtime);

        let erst = dev
            .res()
            .alloc_coherent(core::mem::size_of::<ErstEntry>(), 64)?;
        let event = Ring::new(
            dev.res()
                .alloc_coherent(EVENT_RING_TRBS * core::mem::size_of::<Trb>(), 64)?,
            EVENT_RING_TRBS,
        );
        let command = Ring::with_link(
            dev.res()
                .alloc_coherent((RING_TRBS + 1) * core::mem::size_of::<Trb>(), 64)?,
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

        // After the structures are zeroed: `allocate_scratchpads` writes
        // DCBAA[0], and doing it before the zeroing pass wiped that entry, which
        // left the controller without the scratchpad array it asks for and made
        // `Enable Slot` fail while a No-Op still worked.
        let scratchpads = allocate_scratchpads(dev, hcsparams2, &device_contexts)?;
        log_diff_set(&bar, operational, runtime);
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
            _scratchpads: scratchpads,
            bounce,
            event_dequeue: 0,
            // The controller publishes the first event with the cycle bit set.
            event_cycle: true,
            slots: Vec::new(),
            address_zero_slot: None,
        };
        xhci.program_rings()?;
        // Linux `xhci_run_finished` does these as three separate read-modify-writes
        // and in this order: enable event interrupts (EIE), enable the primary
        // interrupter (IMAN.IE), and only then set RUN. This driver used to write
        // `RS|INTE|EWE` in one go and never touched IMAN at all.
        let usbcmd = xhci.operational + op::USBCMD;
        let iman = xhci.runtime + RT_IR0;
        xhci.bar.write32(iman, xhci.bar.read32(iman) | 0x2); // IMAN.IE
        // A working board's USBCMD reads exactly 0x5: RUN | INTE and nothing else.
        // EWE (bit 10) belongs to device mode; setting it in host mode is what this
        // driver had been doing, and it read back as 0x405 instead of 0x5.
        xhci.bar.write32(
            usbcmd,
            (xhci.bar.read32(usbcmd) & !cmd::EWE) | cmd::RS | cmd::INTE,
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
        xhci.bar.write32(crcr + 4, (cmd_addr >> 32) as u32);
        xhci.bar.write32(crcr, cmd_addr as u32);
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
        // Linux `xhci_add_interrupter` keeps the fields it does not own: ERSTSZ
        // only carries the segment count, ERSTBA only the base address.
        let erstsz = self.runtime + RT_IR0 + 0x08;
        self.bar
            .write32(erstsz, (self.bar.read32(erstsz) & !0xffff) | 1);
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
            // Clear the change bits by writing 1 to them, and nothing else
            // besides port power. Echoing the read value back (which is what
            // this did) also echoes PR, and PR is the port RESET bit: every
            // status read therefore re-asserted a port reset. On the
            // SuperSpeed port that was visible as the link training to U0 at
            // PSIV 4 and being knocked to Disabled one poll later
            // (0x002a1203 -> 0x00020280), i.e. the port worked and our own
            // bookkeeping reset it. PED/PLS/CSC are read-only or write-1-to-
            // clear; PP is the only writable bit worth preserving here.
            self.bar
                .write32(offset, (value & portsc::PP) | (value & portsc::CHANGE_BITS));
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
        let mut trb = self.event.read(self.event_dequeue);
        if trb.control & regs::trb::CYCLE != u32::from(self.event_cycle) {
            return None;
        }
        // The cycle bit is the controller's "this TRB is mine to publish" flag:
        // seeing it does not yet mean the rest of the event has landed, so the
        // read is fenced and repeated (Linux does `rmb()` after reading the TRB).
        // Without this a completion can be consumed with a zero status word, which
        // reads exactly like a completion code of 0.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        trb = self.event.read(self.event_dequeue);
        self.event_dequeue += 1;
        if self.event_dequeue == self.event.trbs {
            self.event_dequeue = 0;
            self.event_cycle = !self.event_cycle;
        }
        // Hand the consumed slot back to the controller (ERDP, clearing EHB).
        // ERDP's low word carries EHB, so it is one of the registers where the
        // DWC3 wrapper's hi-then-lo write order actually matters (the generic
        // 64-bit store can land the halves the other way round).
        let dequeue = self.event.mem.bus_addr() + (self.event_dequeue * TRB_LEN) as u64;
        write64_hi_lo(
            &self.bar,
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
        // A command that produces no completion at all usually means the
        // controller faulted rather than refused: report the error bits.
        let sts = self.bar.read32(self.operational + op::USBSTS);
        log::error!(
            "xhci: {what} no completion; USBSTS {sts:#010x} (hse {}, hce {}, cnr {}, hch {}, eint \
             {})",
            sts & sts::HSE != 0,
            sts & sts::HCE != 0,
            sts & sts::CNR != 0,
            sts & sts::HCH != 0,
            sts & sts::EINT != 0,
        );
        // Is the command ring still running after the timeout? CRCR only reports
        // status bits on this IP, but CRR going clear would mean the controller
        // stopped consuming commands.
        log::error!(
            "xhci: {what} after timeout: crcr {:#x} usbcmd {:#x}",
            self.bar.read32(self.operational + op::CRCR),
            self.bar.read32(self.operational + op::USBCMD),
        );
        // Where did the controller put the event? Dump the first slots so a
        // consumer/pointer mismatch is visible instead of inferred.
        let mut line = alloc::string::String::new();
        for index in 0..8 {
            let trb = self.event.read(index);
            line.push_str(&alloc::format!(
                " [{index}]={:#x}/{:#x}",
                trb.control,
                trb.status
            ));
        }
        log::error!(
            "xhci: {what} event slots:{line} (dequeue {})",
            self.event_dequeue
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
        // Read back the TRB from our own DMA memory: what the controller fetches
        // is memory, and memory is the only place where "what we submitted" and
        // "what it parsed" cannot diverge.
        let index = self.command.index - 1;
        let raw = self.command.read(index);
        info!(
            "xhci: {what} trb[{index}] p {:#x} s {:#010x} c {:#010x} at {published:#x}",
            raw.parameter, raw.status, raw.control
        );
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
        self.ring_command_doorbell();
        // A command that is never fetched looks exactly like a lost notification:
        // the ring is still running (CRR set) and the TRB is on it, but the
        // controller's dequeue never moves. Ring the doorbell again before giving
        // up, and log which attempt produced the completion.
        let event = match self.wait_event(regs::trb::COMMAND_COMPLETION, what) {
            Ok(event) => event,
            Err(first) => {
                warn!("xhci: {what} not fetched, ringing the doorbell again");
                self.ring_command_doorbell();
                match self.wait_event(regs::trb::COMMAND_COMPLETION, what) {
                    Ok(event) => event,
                    Err(_) => return Err(first),
                }
            }
        };
        let code = event.completion_code();
        // The completion event's parameter word is the address of the command TRB
        // that finished, so it identifies which command the code belongs to --
        // "we consumed someone else's completion" and "this command failed" look
        // identical otherwise.
        info!(
            "xhci: {what} completion code {code} for trb {:#x} (slot {}, submitted {:#x})",
            event.parameter,
            event.slot_id(),
            published,
        );
        if code != regs::code::SUCCESS {
            log::error!("xhci: {what} failed, completion code {code}");
            // A command error stops the controller from processing the ring, so
            // every following command would time out. Hand it the dequeue
            // pointer again (with the current cycle state) to restart it.
            self.rearm_command_ring();
            return Err(KError::Io);
        }
        Ok(event)
    }

    /// Rings the command-ring doorbell and flushes the write.
    ///
    /// Both the vendor kernel and mainline follow the store with a read-back
    /// (`writel(DB_VALUE_HOST, &doorbell[0]); readl(&doorbell[0]);`): the doorbell
    /// is a posted write, and without the flush the controller may see the
    /// command ring pointer update before it sees the notification that there is
    /// work on it.
    fn ring_command_doorbell(&self) {
        self.bar.write32(self.doorbell, 0);
        let _ = self.bar.read32(self.doorbell);
    }

    /// Re-publishes the command ring dequeue pointer after a command error.
    fn rearm_command_ring(&self) {
        let dequeue = self.command.dequeue_address();
        let crcr = self.operational + op::CRCR;
        self.bar.write32(
            crcr,
            dequeue as u32 | u32::from(self.command.current_cycle()),
        );
        self.bar.write32(crcr + 4, (dequeue >> 32) as u32);
        // Re-publishing the pointer is only half of a restart: the controller
        // also has to be told there is work, i.e. the command-ring doorbell.
        self.ring_command_doorbell();
        info!("xhci: command ring re-armed at {dequeue:#x}");
    }

    /// `Disable Slot`: gives a slot back to the controller.
    ///
    /// `Address Device` can fail (a Transaction Error on a hub child, for one),
    /// and a slot left in that state is still owned by the controller: the next
    /// device then gets a slot whose predecessor never finished addressing, and
    /// the failure spreads to devices that address fine on their own. Releasing
    /// it is the only way to keep one device's failure local.
    fn disable_slot(&mut self, slot_id: u8) -> Result<(), KError> {
        let trb = Trb::new(
            0,
            0,
            (regs::trb::DISABLE_SLOT << regs::trb::TYPE_SHIFT)
                | (u32::from(slot_id) << regs::trb::SLOT_SHIFT),
        );
        self.command(trb, "disable slot")?;
        self.slots.retain(|slot| slot.id != slot_id);
        info!("xhci: released slot {slot_id}");
        Ok(())
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
        let ep0 = Ring::with_link(
            dev.res()
                .alloc_coherent((RING_TRBS + 1) * TRB_LEN, 64)
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
            port: None,
            input,
            ep0,
            bulk: [None, None],
            interrupt: None,
            device,
        })
    }

    /// Dump the controller-side view when `Address Device` times out: whether
    /// the device context got written (a filled one means the command ran and
    /// only its completion went missing) and the DWC3 globals that decide
    /// whether the core can talk to a device at all.
    fn dump_address_device_failure(&self, slot_index: usize, err: KError) {
        let dwords = self.slots[slot_index].device.as_ptr().cast::<u32>();
        let mut line = alloc::string::String::new();
        for index in 0..10 {
            let cell = dwords.wrapping_add(index);
            // SAFETY: `index` stays inside the device context allocation.
            let value = unsafe { core::ptr::read_volatile(cell) };
            line.push_str(&alloc::format!(" {index}={value:#x}"));
        }
        log::error!("xhci: address device {err:?}; device ctx:{line}");
        let gctl = self.bar.read32(crate::dwc3::regs::GCTL);
        let gusb2 = self.bar.read32(crate::dwc3::regs::GUSB2PHYCFG);
        let gsts = self.bar.read32(crate::dwc3::regs::GSTS);
        // GSTS.BUS_ERR_ADDR_VLD (bit 4) means the core recorded a bus fault;
        // GBUSERRADDR then names the address it faulted on.
        let (err_lo, err_hi) = if gsts & (1 << 4) != 0 {
            (
                self.bar.read32(crate::dwc3::regs::GLOBALS_START + 0x30),
                self.bar.read32(crate::dwc3::regs::GLOBALS_START + 0x34),
            )
        } else {
            (0, 0)
        };
        log::error!(
            "xhci: dwc3 globals at timeout: GCTL {gctl:#010x} (prtcapdir {}) GUSB2PHYCFG \
             {gusb2:#010x} GSTS {gsts:#010x} bus_err_addr {:#x}{:08x}",
            (gctl >> 12) & 0x3,
            err_hi,
            err_lo,
        );
    }

    /// Reads the first 8 descriptor bytes on the default address (0).
    /// `get_bMaxPacketSize0()`: the device descriptor read that precedes the
    /// address assignment.
    ///
    /// The reference asks for `GET_DESCRIPTOR_BUFSIZE` (64) bytes here, not the
    /// 8 that the descriptor's own size field suggests — "this is what Windows
    /// does, so it may help with some non-standards-compliant devices" — and
    /// retries `GET_MAXPACKET0_TRIES` times. It then accepts only a DEVICE
    /// descriptor whose bMaxPacketSize0 is one of the encodings the
    /// specification allows (9 means 512, for SuperSpeed).
    fn read_device_head(&mut self) -> Result<(u16, u8), KError> {
        const TRIES: usize = 3;
        let mut last = Err(KError::Io);
        for _ in 0..TRIES {
            let mut buf = [0u8; 64];
            let setup = usb::SetupPacket::get_descriptor(usb::descriptor::ty::DEVICE, 0, 64);
            match self.control_transfer(0, &setup, &mut buf) {
                Ok(length) => {
                    let max_packet = buf[7];
                    let is_device = buf[1] == usb::descriptor::ty::DEVICE;
                    let is_legal = matches!(max_packet, 8 | 16 | 32 | 64 | 9);
                    if length >= 8 && is_device && is_legal {
                        return Ok((u16::from(max_packet), buf[4]));
                    }
                    log::warn!(
                        "xhci: descriptor head rejected: type {:#04x}, max packet {max_packet}, \
                         length {length}",
                        buf[1]
                    );
                    last = Err(KError::Io);
                }
                Err(err) => last = Err(err),
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100_000));
        }
        last
    }

    /// The old enumeration scheme: assign the address, then read the descriptor.
    ///
    /// `use_new_scheme()` refuses the new scheme for USB 3 devices outright —
    /// "`new scheme` enumeration ... causes USB3 devices to receive control
    /// commands in the default state. This has been seen to cause enumeration
    /// failures" — so a SuperSpeed device is reset once, addressed, and only
    /// then asked for its descriptor. Running the new scheme on one exposes it
    /// to exactly the traffic the reference goes out of its way to avoid.
    fn address_device_direct(
        &mut self,
        dev: &Device<'_, driver_base::PlatformBus>,
        root_port: usize,
        speed: usb::Speed,
        addressing: Addressing,
    ) -> Result<u8, KError> {
        let guess = match speed {
            usb::Speed::Low => 8,
            usb::Speed::Super | usb::Speed::SuperPlus => 512,
            _ => 64,
        };
        let slot_id = self.enable_slot()?;
        let slot = self.allocate_slot(dev, slot_id)?;
        self.slots.push(slot);
        let slot_index = self.slots.len() - 1;
        if let Err(err) =
            self.address_device(slot_index, root_port, speed, guess, addressing, false)
        {
            let _ = self.disable_slot(slot_id);
            return Err(err);
        }
        self.address_zero_slot = None;
        Ok(slot_id)
    }

    /// `xhci_setup_device()` with BSR: install the contexts, no address yet.
    ///
    /// This is the reference's `hub_enable_device()` step, which `hub_port_init`
    /// runs for every new device before it reads `bMaxPacketSize0` on address 0.
    fn slot_bootstrap(
        &mut self,
        dev: &Device<'_, driver_base::PlatformBus>,
        root_port: usize,
        speed: usb::Speed,
        addressing: Addressing,
    ) -> Result<usize, KError> {
        // From xhci_setup_addressable_virt_dev(): 8 low, 64 full and high,
        // 512 SuperSpeed/Plus (the USB core guesses 64 for full speed).
        let guess = match speed {
            usb::Speed::Low => 8,
            usb::Speed::Super | usb::Speed::SuperPlus => 512,
            _ => 64,
        };
        let slot_id = self.enable_slot()?;
        let slot = self.allocate_slot(dev, slot_id)?;
        self.slots.push(slot);
        let slot_index = self.slots.len() - 1;
        if let Err(err) = self.address_device(slot_index, root_port, speed, guess, addressing, true)
        {
            let _ = self.disable_slot(slot_id);
            return Err(err);
        }
        self.address_zero_slot = Some(slot_id);
        // What the controller wrote back decides whether traffic can even be sent:
        // EP0 must be Running and point at our ring.
        self.log_device_context(slot_index);
        match self.read_device_head() {
            Ok((max_packet, class)) => {
                info!("xhci: slot {slot_id} descriptor head: ep0 {max_packet}, class {class:#04x}")
            }
            Err(err) => {
                log::warn!("xhci: slot {slot_id} descriptor head unreadable: {err:?}");
                // Discriminator: an unreachable device fails every SETUP with a
                // transaction error, while a reachable one answers an unsupported
                // request with a stall. Which code comes back says whether the
                // device ever sees this slot's traffic at all.
                if let Err(reset) = self.reset_ep0(slot_index) {
                    log::warn!("xhci: reset endpoint failed: {reset:?}");
                }
                let bogus = usb::SetupPacket::new(0x80, 6, 0x3f00, 0, 8);
                let mut probe = [0u8; 8];
                match self.control_transfer_slot(slot_index, &bogus, &mut probe) {
                    Ok(_) => log::warn!("xhci: bogus-request probe unexpectedly succeeded"),
                    Err(probe_err) => log::warn!("xhci: bogus-request probe -> {probe_err:?}"),
                }
                self.address_zero_slot = None;
                let _ = self.disable_slot(slot_id);
                return Err(err);
            }
        }
        self.address_zero_slot = None;
        Ok(slot_index)
    }

    /// `xhci_setup_device()` without BSR, on a slot the bootstrap prepared.
    ///
    /// The reference's else-branch: the slot context is already set up, so only
    /// the endpoint state the controller owns is carried back into the input
    /// context (`xhci_copy_ep0_dequeue_into_input_ctx`). Rewriting the slot
    /// context instead is what a Context State Error rejected earlier.
    fn slot_assign_address(&mut self, slot_index: usize) -> Result<(), KError> {
        let dwords = self.context_size / 4;
        let slot_id = self.slots[slot_index].id;
        let input = self.slots[slot_index].input.as_ptr().cast::<u32>();
        let device = self.slots[slot_index].device.as_ptr().cast::<u32>();
        for offset in 0..(dwords * 2) {
            // SAFETY: offset stays inside the input context's slot+EP0 region and
            // the device context's own slot+EP0 region, both owned here.
            let value = unsafe { core::ptr::read_volatile(device.wrapping_add(offset)) };
            // SAFETY: as above.
            unsafe {
                core::ptr::write_volatile(input.wrapping_add(dwords + offset), value);
            }
        }
        // Add slot(0) and EP0(1), as `ctrl_ctx->add_flags = SLOT_FLAG | EP0_FLAG`.
        // SAFETY: dword 1 of the input control context, inside this allocation.
        unsafe {
            core::ptr::write_volatile(input.wrapping_add(1), 0x3);
        }
        let trb = Trb::new(
            self.slots[slot_index].input.bus_addr(),
            0,
            (regs::trb::ADDRESS_DEVICE << regs::trb::TYPE_SHIFT)
                | (u32::from(slot_id) << regs::trb::SLOT_SHIFT),
        );
        let event = self.command(trb, "address device (assign)")?;
        info!(
            "xhci: slot {slot_id} addressed (completion code {})",
            event.completion_code()
        );
        Ok(())
    }

    /// `Address Device`: hands the controller the slot and EP0 contexts, which
    /// is what gives the device its USB address.
    fn address_device(
        &mut self,
        slot_index: usize,
        port: usize,
        speed: Speed,
        ep0_max_packet: u16,
        addressing: Addressing,
        bsr: bool,
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
        write(
            slot_ctx,
            context::slot_dword0_routed(
                addressing.route,
                speed_id,
                1,
                addressing.multi_tt,
                addressing.hub,
            ),
        );
        write(slot_ctx + 1, context::slot_dword1((port + 1) as u8));
        if let Some(tt) = addressing.tt {
            write(
                slot_ctx + 2,
                context::slot_dword2_tt(tt.think_time, tt.port, tt.hub_slot),
            );
        }
        let ep0_ctx = dwords_per_context * (context::EP0_CONTEXT + 1);
        write(ep0_ctx + 1, context::endpoint_dword1(4, ep0_max_packet));
        write(ep0_ctx + 2, context::endpoint_dword2(ep0_ring, dcs));
        write(ep0_ctx + 3, (ep0_ring >> 32) as u32);
        write(ep0_ctx + 4, context::endpoint_dword4(8));
        let slot_id = u32::from(self.slots[slot_index].id);
        // What the controller is about to read: the input control context and the
        // contexts this command adds.
        {
            let dwords = self.slots[slot_index].input.as_ptr().cast::<u32>();
            let mut line = alloc::string::String::new();
            for index in 0..(dwords_per_context * 2 + 5) {
                let cell = dwords.wrapping_add(index);
                // SAFETY: `index` stays inside the input context allocation.
                let value = unsafe { core::ptr::read_volatile(cell) };
                if value != 0 {
                    line.push_str(&alloc::format!(" {index}={value:#x}"));
                }
            }
            info!(
                "xhci: input ctx {input_addr:#x} for slot {slot_id}, device ctx {:#x}, dcbaa \
                 {:#x}: {line}",
                self.slots[slot_index].device.bus_addr(),
                self.device_contexts.bus_addr(),
            );
        }
        let mut address_control = (regs::trb::ADDRESS_DEVICE << regs::trb::TYPE_SHIFT)
            | (u32::from(slot_id) << regs::trb::SLOT_SHIFT);
        if bsr {
            // Block Set Address Request: install the slot and EP0 contexts
            // without assigning an address. The parameter existed here but was
            // only ever forwarded to the recursive retry, never turned into this
            // bit — so every "BSR" command this driver issued was a full Address
            // Device that sent SET_ADDRESS, and the two-step sequence the
            // reference relies on had never actually run.
            address_control |= regs::trb::BSR;
        }
        let trb = Trb::new(input_addr, 0, address_control);
        // Port and PHY state at the moment of the transaction, not at reset time.
        let portsc = self
            .bar
            .read32(self.operational + op::PORTSC_BASE + port * op::PORT_STRIDE);
        info!(
            "xhci: before address device: portsc {portsc:#010x} (ccs {}, ped {}, pls {}, psiv {})",
            portsc & portsc::CCS != 0,
            portsc & portsc::PED != 0,
            (portsc & portsc::PLS_MASK) >> portsc::PLS_SHIFT,
            (portsc & portsc::PSIV_MASK) >> portsc::PSIV_SHIFT,
        );
        match self.command(trb, "address device") {
            Ok(_) => {}
            Err(err) => {
                self.dump_address_device_failure(slot_index, err);
                return Err(err);
            }
        }
        self.slots[slot_index].speed = speed_id;
        // The controller now owns this device's address (the slot id).
        self.slots[slot_index].port = Some(port);
        log::info!(
            "xhci: slot {slot_id} addressed on port {}, speed {}, ep0 max packet {ep0_max_packet}",
            port + 1,
            speed.as_str(),
        );
        // What the controller wrote back is the only trustworthy statement
        // about whether `Address Device` took effect: EP0's context must say
        // Running and carry the ring pointer we handed over.
        self.log_device_context(slot_index);
        Ok(())
    }

    /// Dump the device context the controller wrote: slot state and EP0's
    /// state/dequeue. A transfer ring that is never fetched is either "EP0 not
    /// Running" (here) or "doorbell never landed" (checked at the call site).
    fn log_device_context(&self, slot_index: usize) {
        let dwords = self.slots[slot_index].device.as_ptr().cast::<u32>();
        let per = self.context_size / 4;
        let mut line = alloc::string::String::new();
        for index in 0..(per * 2 + 4) {
            let cell = dwords.wrapping_add(index);
            // SAFETY: `index` stays inside the device context allocation.
            let value = unsafe { core::ptr::read_volatile(cell) };
            line.push_str(&alloc::format!(" {index}={value:#x}"));
        }
        log::info!(
            "xhci: device ctx after address: slot dword0 {:#x} (entries {}, speed {}), ep0 dword0 \
             {:#x} (state {}):{line}",
            unsafe { core::ptr::read_volatile(dwords) },
            (unsafe { core::ptr::read_volatile(dwords) } >> 27) & 0x1f,
            (unsafe { core::ptr::read_volatile(dwords) } >> 20) & 0xf,
            unsafe { core::ptr::read_volatile(dwords.wrapping_add(per)) },
            unsafe { core::ptr::read_volatile(dwords.wrapping_add(per)) } & 0x7,
        );
    }

    /// `xhci_handle_halted_endpoint()`: Reset Endpoint, then re-point the ring.
    ///
    /// A failed control transfer (a USB Transaction Error in particular) leaves
    /// EP0 in the Halted state, and a halted endpoint consumes nothing: every
    /// further TRB queued on it is simply never executed, which is why retrying
    /// a transfer without this step can only ever time out. The reference issues
    /// Reset Endpoint followed by Set TR Dequeue Pointer for exactly that case.
    fn reset_ep0(&mut self, slot_index: usize) -> Result<(), KError> {
        let slot_id = u32::from(self.slots[slot_index].id);
        let endpoint = |trb_type: u32| {
            (trb_type << regs::trb::TYPE_SHIFT)
                | (regs::trb::EP0 << regs::trb::EP_SHIFT)
                | (slot_id << regs::trb::SLOT_SHIFT)
        };
        self.command(
            Trb::new(0, 0, endpoint(regs::trb::RESET_ENDPOINT)),
            "reset endpoint",
        )?;
        let slot = &self.slots[slot_index];
        let dequeue = slot.ep0.dequeue_address() | u64::from(slot.ep0.current_cycle());
        self.command(
            Trb::new(dequeue, 0, endpoint(regs::trb::SET_TR_DEQUEUE)),
            "set tr dequeue",
        )?;
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
        // The data stage this setup announces. xHCI 1.0 §6.4.1.2.1 requires it
        // whenever the transfer carries data; without it the controller assumes
        // there is no data stage and never fetches the Data TRB below.
        if length > 0 {
            let trt = if is_in {
                regs::trb::TRT_IN
            } else {
                regs::trb::TRT_OUT
            };
            control |= trt << regs::trb::TRT_SHIFT;
        }
        let held = self.slots[slot_index]
            .ep0
            .push_held(Trb::new(setup_word, 8, control));
        if length > 0 {
            // `xhci_queue_ctrl_tx()` builds the data TRB as
            //   TYPE(DATA) [| DIR_IN] [| ISP if IN]
            // and puts no IOC on it.
            let mut data_control = regs::trb::DATA << regs::trb::TYPE_SHIFT;
            if is_in {
                data_control |= regs::trb::DIR_IN;
                // Interrupt on Short Packet: every descriptor read answers with
                // fewer bytes than requested, and without ISP the controller
                // waits for a full-length packet that never arrives.
                data_control |= regs::trb::ISP;
            }
            // The completion event that carries this transfer's byte count.
            data_control |= regs::trb::IOC;
            // SAFETY: as above.
            let bounce_addr = self.bounce.bus_addr();
            self.slots[slot_index]
                .ep0
                .push(Trb::new(bounce_addr, length as u32, data_control));
        }
        // Status stage: OUT whenever the data stage was IN (and vice versa).
        //
        // IOC goes on the data TRB when there is one, not on the status TRB as
        // `xhci_queue_ctrl_tx` does: the completion event's transfer-length field
        // is how this driver learns how many bytes actually arrived, and only the
        // data TRB carries that number. Linux can put IOC on the status TRB
        // because it tracks the whole transfer descriptor; until this driver does
        // the same, the length has to come from the data stage.
        let mut status_control = regs::trb::STATUS << regs::trb::TYPE_SHIFT;
        if !(length > 0 && is_in) {
            status_control |= regs::trb::DIR_IN;
        }
        if length == 0 {
            status_control |= regs::trb::IOC;
        }
        self.slots[slot_index]
            .ep0
            .push(Trb::new(0, 0, status_control));

        // Only now, with the whole transfer in the ring, does the setup TRB
        // become visible — `giveback_first_trb()`'s ordering.
        self.slots[slot_index].ep0.publish(held);

        // Doorbell for this slot, target = EP0's device slot context index (1).
        let doorbell = self.doorbell + 4 * u32::from(slot_id) as usize;
        self.bar.write32(doorbell, 1);
        // Flush the write, as `xhci_ring_ep_doorbell` does with a read-back: a
        // posted doorbell that is still in flight when the controller looks for
        // work is indistinguishable from a ring that is not being fetched.
        let _ = self.bar.read32(doorbell);
        let event = match self.wait_event(regs::trb::TRANSFER_EVENT, "control transfer") {
            Ok(event) => event,
            Err(err) => {
                // The ring we handed the controller vs. what is actually in it:
                // "TRBs never written" and "written but never fetched" have
                // completely different causes, and both look like a timeout.
                let mut line = alloc::string::String::new();
                for index in 0..3 {
                    let trb = self.slots[slot_index].ep0.read(index);
                    line.push_str(&alloc::format!(
                        " [{index}] p={:#x} s={:#x} c={:#x}",
                        trb.parameter,
                        trb.status,
                        trb.control
                    ));
                }
                log::error!(
                    "xhci: control transfer ep0 ring at {:#x} (cycle {}):{line}",
                    self.slots[slot_index].ep0.mem.bus_addr(),
                    self.slots[slot_index].ep0.current_cycle(),
                );
                return Err(err);
            }
        };
        let code = event.completion_code();
        if code != regs::code::SUCCESS && code != regs::code::SHORT_PACKET {
            // The event says which slot and endpoint the controller blames, and
            // which TRB it stopped at. Without that, "the device did not answer"
            // and "the controller never routed it" look identical.
            log::error!(
                "xhci: control transfer completion code {code} (slot {}, ep {}, trb {:#x}, \
                 residual {})",
                (event.control >> regs::trb::SLOT_SHIFT) & 0xff,
                (event.control >> 16) & 0x1f,
                event.parameter,
                event.transfer_length(),
            );
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
        // Idempotent on purpose. Two layers ask for "the port's speed": this
        // driver's own bring-up (reset → Enable Slot → Address Device) and
        // `usb::enumerate::enumerate_port`, which calls `port_reset` as its
        // first step. Re-asserting PR on a port that is already enabled and
        // addressed resets it again and invalidates the slot's state, so the
        // very next control transfer fails (`enumeration failed: NotFound`
        // right after a successful Address Device). Report the current speed
        // instead when the port is already enabled and not in reset.
        if current & portsc::PED != 0 && current & portsc::PR == 0 {
            let speed = regs::speed_from_psiv((current & portsc::PSIV_MASK) >> portsc::PSIV_SHIFT);
            log::info!(
                "xhci: port {} already enabled, speed {}, portsc {current:#010x}",
                port + 1,
                speed.as_str(),
            );
            return Ok(speed);
        }
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
                log::info!(
                    "xhci: port {} reset, speed {}, portsc {:#010x} (ped {}, pls {})",
                    port + 1,
                    speed.as_str(),
                    value,
                    value & portsc::PED != 0,
                    (value & portsc::PLS_MASK) >> portsc::PLS_SHIFT,
                );
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

    /// xHCI's device addresses are slot ids assigned by `Address Device`, so a
    /// port whose slot is already addressed reports that id and the bus core
    /// skips the default-address phase.
    fn port_address(&self, port: usize) -> Option<u8> {
        self.slots
            .iter()
            .find(|slot| slot.port == Some(port))
            .map(|slot| slot.id)
    }

    fn control_transfer(
        &mut self,
        address: u8,
        setup: &SetupPacket,
        data: &mut [u8],
    ) -> KResult<usize> {
        // xHCI uses the slot id as the device address, so the address the bus
        // core passes back is the slot whose EP0 ring we transfer on.
        let wanted = if address == 0 {
            self.address_zero_slot.ok_or(KError::NotFound)?
        } else {
            address
        };
        let slot_index = self
            .slots
            .iter()
            .position(|slot| slot.id == wanted)
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
    /// Watches every root port until none is mid-training, logging each change.
    ///
    /// Bounded (6 s). A port whose link state is one of the transient ones —
    /// Polling, Recovery, Hot Reset, Compliance or Resume — is still negotiating,
    /// so cutting the wait short there would report a connected device as absent.
    /// Everything else (U0, U3, RxDetect, Disabled, Inactive) is a settled state
    /// and worth a line the first time it appears.
    fn poll_ports_until_settled(&mut self) {
        const POLL_MS: usize = 100;
        const MAX_POLLS: usize = 60;
        let mut last: alloc::vec::Vec<u32> = alloc::vec![u32::MAX; self.ports];
        for attempt in 0..MAX_POLLS {
            let mut transient = false;
            for port in 0..self.ports {
                let raw = self.port_status_raw(port);
                if raw != last[port] {
                    let pls = (raw & portsc::PLS_MASK) >> portsc::PLS_SHIFT;
                    log::info!(
                        "xhci: port {} portsc {raw:#010x} -> ccs {}, ped {}, pls {}{}",
                        port + 1,
                        raw & portsc::CCS != 0,
                        raw & portsc::PED != 0,
                        pls,
                        if is_transient_pls(pls) {
                            " (training)"
                        } else {
                            ""
                        },
                    );
                    last[port] = raw;
                }
                let pls = (raw & portsc::PLS_MASK) >> portsc::PLS_SHIFT;
                transient |= is_transient_pls(pls);
            }
            // Settled and at least one full window has been observed: the
            // summary is printed by the caller's scan.
            if !transient && attempt >= 2 {
                return;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(POLL_MS as u64 * 1_000));
        }
        log::warn!("xhci: ports still training after {MAX_POLLS} polls, scanning anyway");
    }

    pub fn enumerate(&mut self, dev: &Device<'_, driver_base::PlatformBus>) -> (usize, usize) {
        let mut found = 0usize;
        let mut ready = 0usize;
        // VBUS has just been switched on: a device needs time to see it and
        // pull up, and this port has no connect interrupt to wait on. Poll the
        // ports for a while so a device that attaches "late" is not reported as
        // absent -- that is exactly the confusion this port already paid for.
        // Poll until the ports settle rather than until the first connect: a
        // SuperSpeed port reports Connect Status only after it has trained
        // (RxDetect -> Polling -> U0), which takes far longer than the
        // High-Speed path that comes up in microseconds. Stopping at "some port
        // connected" therefore always reports the USB2 side and never gives the
        // SS port its chance — and a single snapshot cannot tell "nothing is
        // attached" from "still training", which are the two states this port
        // has to distinguish.
        self.poll_ports_until_settled();
        for port in 0..self.ports {
            match self.port_status(port) {
                Ok(status) if status.connected => found += 1,
                _ => continue,
            }
            log::info!("xhci: port {} has a device, bringing it up", port + 1);
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
                            // A hub is not a leaf: everything the user plugs in
                            // sits one tier below it. Walk its ports now, while
                            // the device model is being built, so the tree is
                            // complete before any class driver looks for one.
                            if device.device.class == HUB_CLASS {
                                self.scan_hub_ports(dev, &device, port);
                            }
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

    /// Walks a hub's downstream ports and enumerates what is attached.
    ///
    /// A device behind a hub is reached differently from a root-port device:
    /// the hub's port is reset through the hub class (the controller knows
    /// nothing about it), the slot carries the route string of the port path,
    /// and a full/low-speed device behind a high-speed hub also carries that
    /// hub's transaction translator — without it the device is addressed but
    /// never answers, because every transaction has to be split by the hub.
    fn scan_hub_ports(
        &mut self,
        dev: &Device<'_, driver_base::PlatformBus>,
        hub: &usb::UsbDevice,
        root_port: usize,
    ) {
        // The controller has to be told this slot is a hub before it will route
        // anything to the ports below it.
        if let Some(info) = usb::hub::info(self, hub.address, hub.device.protocol) {
            if let Some(slot_index) = self.slots.iter().position(|slot| slot.id == hub.address) {
                let interrupt = hub.configurations.first().and_then(|config| {
                    config.interfaces.first().and_then(|interface| {
                        interface.endpoints.iter().find_map(|endpoint| {
                            (endpoint.is_in() && endpoint.attributes & 0x03 == 3).then_some((
                                endpoint.number(),
                                endpoint.max_packet_size,
                                endpoint.interval,
                            ))
                        })
                    })
                });
                if let Err(err) = self.configure_hub(dev, slot_index, info.ports, interrupt) {
                    log::error!("xhci: hub {} configure failed: {err:?}", hub.address);
                }
            }
        }
        let ports = usb::hub::scan(self, hub.address, hub.device.protocol);
        // `bDeviceProtocol` (from the *device* descriptor): 0 FS hub, 1 HS
        // single-TT, 2 HS multi-TT, 3 SuperSpeed.
        let protocol = hub.device.protocol;
        let port_count = ports.len();
        let _ = port_count;
        for entry in ports.iter().filter(|port| port.connected) {
            log::info!(
                "usb: hub {} port {}: device attached at {} speed",
                hub.address,
                entry.port,
                entry.speed.as_str(),
            );
            let Some(speed) = self.reset_hub_port(hub, entry.port, protocol) else {
                continue;
            };
            // USB 2.0 §11.5.1.5 reset recovery: the device must not be sent a
            // SETUP until it has settled after the hub's reset. Flash drives in
            // particular take noticeably longer than the minimum, and the only
            // symptom of being early is a Transaction Error from `Address
            // Device` — indistinguishable from a wrong route or speed.
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100_000));
            // A full/low-speed device behind a high-speed hub needs the hub's
            // transaction translator; a SuperSpeed device needs neither.
            // The slot context's MTT bit and its TT fields go together: MTT only
            // says the parent hub has one translator per port, and a high-speed
            // device behind the hub is not translated at all, so setting MTT
            // without the TT fields describes a topology that does not exist.
            let tt = match (protocol, speed) {
                (1 | 2, usb::Speed::Low | usb::Speed::Full) => Some(Translator {
                    think_time: 0,
                    port: entry.port,
                    hub_slot: hub.address,
                }),
                _ => None,
            };
            let addressing = Addressing {
                route: u32::from(entry.port) & 0xf,
                tt,
                multi_tt: tt.is_some() && protocol == 2,
                hub: false,
            };
            // `hub_port_init()` retries a new device `GET_DESCRIPTOR_TRIES` (3)
            // times, 100 ms apart, and `xhci_setup_device()` treats a
            // Transaction Error from Address Device as retryable: it frees the
            // slot and lets the caller try again. Devices routinely need that
            // second or third attempt (they answer before their firmware has
            // finished settling), so a single failure is not a verdict.
            const ATTEMPTS: usize = 3;
            // The reference's new scheme, in its order: enable the slot contexts,
            // read bMaxPacketSize0 on address 0, reset the port AGAIN, then assign
            // the address on the same slot. The middle reset is the one the device
            // is in when it is finally addressed.
            // `use_new_scheme()`: the new scheme is not used for USB 3 devices.
            let is_usb3 = matches!(speed, usb::Speed::Super | usb::Speed::SuperPlus);
            let mut attempt: Result<u8, KError> = if is_usb3 {
                self.address_device_direct(dev, root_port, speed, addressing)
            } else {
                Err(KError::Io)
            };
            for _ in 0..usize::from(is_usb3) {
                break;
            }
            for _ in 0..usize::from(!is_usb3) {
                let slot_index = match self.slot_bootstrap(dev, root_port, speed, addressing) {
                    Ok(index) => index,
                    Err(err) => {
                        attempt = Err(err);
                        break;
                    }
                };
                if self.reset_hub_port(hub, entry.port, protocol).is_none() {
                    attempt = Err(KError::Io);
                    break;
                }
                khal::time::busy_wait(ktime_types::TimeSpan::from_micros(20_000));
                match self.slot_assign_address(slot_index) {
                    Ok(()) => attempt = Ok(self.slots[slot_index].id),
                    Err(err) => attempt = Err(err),
                }
            }
            let _ = &mut attempt;

            for retry in 1..ATTEMPTS {
                if attempt.is_ok() {
                    break;
                }
                log::info!(
                    "xhci: hub {} port {}: attempt {retry} failed, retrying as the reference does",
                    hub.address,
                    entry.port
                );
                // `hub_port_init()`'s caller re-runs the whole function on
                // retry, which begins with `hub_port_reset()`: the port reset
                // is what puts the device back into the state where it accepts
                // SET_ADDRESS. Retrying the command against a device that just
                // refused one only produced the same Transaction Error again.
                if self.reset_hub_port(hub, entry.port, protocol).is_none() {
                    attempt = Err(KError::Io);
                    continue;
                }
                khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100_000));
                attempt = if is_usb3 {
                    self.address_device_direct(dev, root_port, speed, addressing)
                } else {
                    self.address_downstream(dev, root_port, speed, addressing)
                };
            }
            match attempt {
                Ok(address) => match usb::enumerate::enumerate_addressed(
                    self,
                    address,
                    usize::from(entry.port),
                    speed,
                ) {
                    Ok(device) => log::info!(
                        "xhci: hub {} port {} -> {}",
                        hub.address,
                        entry.port,
                        device.summary()
                    ),
                    Err(err) => log::error!(
                        "xhci: hub {} port {} enumeration failed: {err:?}",
                        hub.address,
                        entry.port
                    ),
                },
                Err(err) => log::error!(
                    "xhci: hub {} port {} address device failed: {err:?}",
                    hub.address,
                    entry.port
                ),
            }
        }
    }

    /// `hcd->driver->reset_device()` (`xhci_discover_or_reset_device()`): after
    /// a port reset the controller is told the device was reset, so it drops
    /// whatever context state it kept for the slot. The reference runs this
    /// from `hub_port_reset()`'s success path (`update_devnum(udev, 0)` and the
    /// `reset_device` call that follows) -- without it the controller carries
    /// stale per-device state into the next `Address Device`.
    fn reset_device(&mut self, slot_id: u8) -> Result<(), KError> {
        let trb = Trb::new(
            0,
            0,
            (regs::trb::RESET_DEVICE << regs::trb::TYPE_SHIFT)
                | (u32::from(slot_id) << regs::trb::SLOT_SHIFT),
        );
        let event = self.command(trb, "reset device")?;
        let code = event.completion_code();
        // The command "can't fail" per the 0.95/0.96 spec unless the slot is
        // not enabled or not in the addressed/configured state -- both treated
        // as success by the reference.
        log::info!("xhci: reset device for slot {slot_id}: completion code {code}");
        Ok(())
    }

    /// Resets one downstream port through the hub class and returns its speed.
    fn reset_hub_port(
        &mut self,
        hub: &usb::UsbDevice,
        port: u8,
        protocol: u8,
    ) -> Option<usb::Speed> {
        let info = usb::hub::HubInfo {
            ports: 0,
            protocol,
            power_on_2ms: 0,
        };
        let speed = usb::hub::reset_port(self, hub.address, port, info)?;
        Some(match speed {
            usb::hub::Speed::Low => usb::Speed::Low,
            usb::hub::Speed::Full => usb::Speed::Full,
            usb::hub::Speed::High => usb::Speed::High,
            usb::hub::Speed::Super => usb::Speed::Super,
            usb::hub::Speed::Unknown => usb::Speed::High,
        })
    }

    /// Allocates a slot for a device behind `hub` and addresses it there.
    fn address_downstream(
        &mut self,
        dev: &Device<'_, driver_base::PlatformBus>,
        root_port: usize,
        speed: usb::Speed,
        addressing: Addressing,
    ) -> Result<u8, KError> {
        let slot_id = self.enable_slot()?;
        let slot = self.allocate_slot(dev, slot_id)?;
        self.slots.push(slot);
        let slot_index = self.slots.len() - 1;
        // From the reference's xhci_setup_addressable_virt_dev(): 8 for low
        // speed, 64 for full AND high speed (the USB core guesses 64 for a full
        // speed device until the descriptor arrives), 512 for SuperSpeed/Plus.
        // Full speed used to be given 8 here, which is the low-speed value.
        let ep0_max_packet = match speed {
            usb::Speed::Low => 8,
            usb::Speed::Super | usb::Speed::SuperPlus => 512,
            _ => 64,
        };
        self.address_device(
            slot_index,
            root_port,
            speed,
            ep0_max_packet,
            addressing,
            false,
        )?;
        Ok(slot_id)
    }

    /// Tells the controller that a slot is a hub (`xhci_update_hub_device`).
    ///
    /// The Hub flag is NOT set at `Address Device` — doing so makes the command
    /// fail — but through a **Configure Endpoint** afterwards, together with the
    /// slot context's Number of Ports. Those two are what let the controller
    /// route transactions into the hub's downstream port paths; without them any
    /// device behind the hub fails `Address Device` with a Transaction Error
    /// while the hub itself addresses fine.
    fn configure_hub(
        &mut self,
        dev: &driver_base::Device<'_, driver_base::PlatformBus>,
        slot_index: usize,
        num_ports: u8,
        interrupt: Option<(u8, u16, u8)>,
    ) -> Result<(), KError> {
        let dwords = self.context_size / 4;
        let input = self.slots[slot_index].input.as_ptr().cast::<u32>();
        let device = self.slots[slot_index].device.as_ptr().cast::<u32>();
        let slot_id = self.slots[slot_index].id;
        // The hub's status-change (interrupt IN) endpoint is configured here so
        // the slot reaches the Configured state: xHCI 4.3.3 forbids addressing
        // devices behind a hub whose slot is only Addressed, and an SS hub's
        // child `Address Device` fails with a Transaction Error until this
        // endpoint exists. Linux reaches the same state by claiming the hub's
        // interface, which adds the endpoint through `xhci_add_endpoint()`.
        //
        // The reference splits this into two Configure Endpoint commands:
        // `xhci_update_hub_device()` writes DEV_HUB + MAX_PORTS with add_flags
        // = SLOT, and `xhci_add_endpoint()` adds the endpoint with
        // add_flags = SLOT | ep_flag and a raised `context entries`. One
        // merged command answered Context State Error (17).
        //
        // The slot context the controller built is in its OWN device context:
        // Configure Endpoint must present the current state, not a guess.
        for offset in 0..dwords {
            // SAFETY: `offset` stays inside both contexts (input slot context at
            // dwords..2*dwords, device slot context at 0..dwords).
            let value = unsafe { core::ptr::read_volatile(device.wrapping_add(offset)) };
            // SAFETY: as above, on the input context's slot-context slot.
            unsafe {
                core::ptr::write_volatile(input.wrapping_add(dwords + offset), value);
            }
        }
        let slot_ctx = dwords;
        // dword0 bit26 = Hub; dword1 bits 31:24 = Number of Ports.
        let info0 = unsafe { core::ptr::read_volatile(input.wrapping_add(slot_ctx)) };
        unsafe {
            core::ptr::write_volatile(input.wrapping_add(slot_ctx), info0 | (1 << 26));
        }
        let info1 = unsafe { core::ptr::read_volatile(input.wrapping_add(slot_ctx + 1)) };
        unsafe {
            core::ptr::write_volatile(
                input.wrapping_add(slot_ctx + 1),
                (info1 & 0x00ff_ffff) | (u32::from(num_ports) << 24),
            );
        }
        // dword3 carries the Device State; a Configure Endpoint input context
        // must present it as 0 (the controller owns that field).
        unsafe {
            core::ptr::write_volatile(input.wrapping_add(slot_ctx + 3), 0);
        }
        // Command 1 (`xhci_update_hub_device()`): add_flags = SLOT only.
        unsafe {
            core::ptr::write_volatile(input.wrapping_add(1), 1);
        }
        let trb = Trb::new(
            self.slots[slot_index].input.bus_addr(),
            0,
            (regs::trb::CONFIGURE_ENDPOINT << regs::trb::TYPE_SHIFT)
                | (u32::from(slot_id) << regs::trb::SLOT_SHIFT),
        );
        self.command(trb, "configure hub")?;
        info!("xhci: slot {slot_id} configured as a hub with {num_ports} port(s)");

        // Command 2 (`xhci_add_endpoint()`): add_flags = SLOT | ep_flag, the
        // endpoint context, and the slot's `context entries` raised to cover
        // the endpoint's context index (endpoint * 2 + direction + 1). This is
        // what transitions the slot to Configured, which xHCI 4.3.3 requires
        // before devices behind the hub are addressed.
        if let Some((ep_number, max_packet, interval)) = interrupt {
            let context_index = (u32::from(ep_number) * 2) + 1;
            let entries = context_index + 1;
            let ring = Ring::with_link(
                dev.res()
                    .alloc_coherent((RING_TRBS + 1) * TRB_LEN, 64)
                    .map_err(|_| KError::NoMemory)?,
                RING_TRBS,
            );
            let ring_addr = ring.mem.bus_addr();
            let dcs = ring.cycle;
            {
                let slot = &mut self.slots[slot_index];
                slot.interrupt = Some(ring);
            }
            let ctx = dwords * (context_index + 1) as usize;
            let write = |offset: usize, value: u32| {
                // SAFETY: inside the input context allocation (33 contexts).
                unsafe {
                    core::ptr::write_volatile(input.wrapping_add(offset), value);
                }
            };
            // dword0: interval lives at bits 23:16 (the state bits are
            // controller-owned).
            write(ctx, (u32::from(interval) & 0xff) << 16);
            write(
                ctx + 1,
                context::endpoint_dword1_num(7, max_packet, ep_number),
            );
            write(ctx + 2, context::endpoint_dword2(ring_addr, dcs));
            write(ctx + 3, (ring_addr >> 32) as u32);
            write(
                ctx + 4,
                context::endpoint_dword4(u32::from(max_packet).max(8)),
            );
            // Input control context: add SLOT and the new endpoint; entries
            // raised to cover it.
            write(1, 0x1 | (1 << context_index));
            let entries_dword = unsafe { core::ptr::read_volatile(input.wrapping_add(slot_ctx)) };
            write(
                slot_ctx,
                (entries_dword & !0xf800_0000) | (entries << 27) | (1 << 26),
            );
            let trb = Trb::new(
                self.slots[slot_index].input.bus_addr(),
                0,
                (regs::trb::CONFIGURE_ENDPOINT << regs::trb::TYPE_SHIFT)
                    | (u32::from(slot_id) << regs::trb::SLOT_SHIFT),
            );
            self.command(trb, "configure hub endpoints")?;
            info!(
                "xhci: slot {slot_id} hub interrupt endpoint added at context {context_index} \
                 (entries {entries})"
            );
        }
        // The controller must report this slot as Configured before it will route
        // anything to the ports below the hub; the state lives in the slot
        // context the controller owns, so read it back instead of assuming.
        self.log_device_context(slot_index);
        Ok(())
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
        // USB 2.0 7.1.7.5 reset recovery: the host must not send a SETUP packet
        // until 10 ms after the reset ends. Address Device without BSR makes the
        // controller issue SET_ADDRESS to the device, which is the first such
        // transaction -- issuing it immediately is what left the command hanging.
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(20_000));
        let slot_id = self.enable_slot()?;
        // Discriminator: a No-Op as the third TRB. If this also stalls, the ring
        // stops being consumed after two commands and the fault is in the ring;
        // if it completes, only Address Device is affected.
        let second_noop = Trb::new(0, 0, regs::trb::NO_OP << regs::trb::TYPE_SHIFT);
        match self.command(second_noop, "second no-op") {
            Ok(event) => info!(
                "xhci: second no-op (third TRB) -> code {}",
                event.completion_code()
            ),
            Err(err) => log::error!("xhci: second no-op (third TRB) -> {err:?}"),
        }
        let slot = self.allocate_slot(dev, slot_id)?;
        self.slots.push(slot);
        let slot_index = self.slots.len() - 1;
        // The control endpoint's packet size follows the speed: high and Super
        // speed devices always use 64, low speed always 8, full speed is told by
        // the device descriptor -- which cannot be read before addressing, so
        // 64 is the safe default for the speeds this port supports.
        // SuperSpeed's EP0 always uses a 512-byte max packet. Announcing 64 for
        // a SuperSpeed slot makes `Address Device` fail with completion code 17
        // (Parameter Error) and leaves the device unaddressable -- measured on
        // the USB3.2 companion hub, whose port had already trained to U0 at
        // PSIV 4.
        let ep0_max_packet = match speed {
            Speed::Low => 8,
            Speed::Full => 8,
            Speed::Super => 512,
            _ => 64,
        };
        let ep0_max_packet = match speed {
            Speed::Low | Speed::Full => 8,
            Speed::Super => 512,
            _ => 64,
        };
        self.address_device(
            slot_index,
            port,
            speed,
            ep0_max_packet,
            Addressing::root(),
            false,
        )?;
        // Behaviour probe, not a register read: a No-Op Transfer TRB on the
        // just-addressed slot's EP0 ring. It completes with a Transfer Event
        // and never touches the device, so it separates "the transfer ring /
        // doorbell / event delivery is not working" from "the device does not
        // answer the first SETUP". Both look identical from a control-transfer
        // timeout, and this port already spent rounds on the difference.
        self.probe_transfer_ring(slot_index);
        Ok(slot_id)
    }

    /// Pushes a No-Op Transfer TRB on the slot's EP0 ring and reports whether a
    /// Transfer Event comes back.
    fn probe_transfer_ring(&mut self, slot_index: usize) {
        let slot_id = self.slots[slot_index].id;
        let trb = Trb::new(
            0,
            0,
            (regs::trb::NO_OP_TRANSFER << regs::trb::TYPE_SHIFT) | regs::trb::IOC,
        );
        self.slots[slot_index].ep0.push(trb);
        let doorbell = self.doorbell + 4 * u32::from(slot_id) as usize;
        self.bar.write32(doorbell, 1);
        // Flush the write, as `xhci_ring_ep_doorbell` does with a read-back: a
        // posted doorbell that is still in flight when the controller looks for
        // work is indistinguishable from a ring that is not being fetched.
        let _ = self.bar.read32(doorbell);
        match self.wait_event(regs::trb::TRANSFER_EVENT, "no-op transfer") {
            Ok(event) => info!(
                "xhci: no-op transfer completed (code {}) — EP0 ring, doorbell and event delivery \
                 work; a later control-transfer timeout is the device not answering",
                event.completion_code(),
            ),
            Err(err) => log::error!(
                "xhci: no-op transfer did NOT complete ({err:?}) — the fault is in the EP0 ring / \
                 doorbell / event path, not in the device"
            ),
        }
    }
}

const _: () = {
    // The command ring must satisfy xHCI's minimum size.
    assert!(RING_TRBS >= COMMAND_RING_MIN_TRBS);
};
