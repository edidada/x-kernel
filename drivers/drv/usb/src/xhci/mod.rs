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

/// Where the event ring segment size goes in the ERST entry's dword 2.
///
/// 16 is what xHCI 6.5.3.3 describes; 0 is what Linux's `xhci_alloc_erst()` writes.
/// Kept as one named value because it is an A/B on this silicon, not a preference.
const ERST_SEG_SIZE_SHIFT: u32 = 16;
/// The controller's scratch buffer for control data stages.
/// `HUB_DEBOUNCE_STEP` / `HUB_DEBOUNCE_STABLE` (`drivers/usb/core/hub.c:112-113`).
const HUB_DEBOUNCE_STEP_US: u64 = 25_000;
const HUB_DEBOUNCE_STABLE_US: u64 = 100_000;

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
    /// TRBs the producer has written since the last wrap. `inc_enq()`'s
    /// `num_trbs_free` counterpart: the controller owns everything up to its own
    /// dequeue, and the ring may only be reused after it has been consumed.
    fn used_since_wrap(&self) -> usize {
        self.index % self.trbs
    }

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

    /// `td_to_noop()` (`xhci-ring.c:148`): overwrites a cancelled transfer
    /// descriptor's TRBs with No-Op TRBs, preserving only each TRB's cycle bit.
    ///
    /// Moving the ring dequeue alone is not enough after a halt: the controller
    /// **caches** fetched TRBs, and a cached stalled TD keeps being retried even
    /// though the dequeue has moved past it. Overwriting the TD is what flushes
    /// that cache (`xhci_invalidate_cancelled_tds()` does it for every cancelled
    /// TD before queueing Set TR Dequeue).
    fn no_op_range(&self, first_trb: u64, last_trb: u64) {
        let base = self.mem.bus_addr();
        let mut address = first_trb;
        while address <= last_trb {
            let offset = (address - base) as usize;
            if offset % TRB_LEN != 0 || offset / TRB_LEN >= self.trbs {
                break;
            }
            let slot = self.trb(offset / TRB_LEN);
            // SAFETY: `slot` is one TRB inside this ring's own allocation; the
            // cycle bit is the only field of the old TRB that is kept.
            unsafe {
                let base = slot.cast::<u8>();
                core::ptr::write_volatile(base.cast::<u64>(), 0);
                core::ptr::write_volatile(base.wrapping_add(8).cast::<u32>(), 0);
                let control = base.wrapping_add(12).cast::<u32>();
                let cycle = core::ptr::read_volatile(control) & regs::trb::CYCLE;
                core::ptr::write_volatile(
                    control,
                    cycle | (regs::trb::NO_OP_TRANSFER << regs::trb::TYPE_SHIFT),
                );
            }
            address += TRB_LEN as u64;
        }
        karch::dsb_ishst();
    }

    /// The cycle bit of the TRB at this address (the Set TR Dequeue Pointer
    /// command's DCS field is exactly this, `xhci_move_dequeue_past_td()`).
    fn cycle_at(&self, address: u64) -> bool {
        let offset = (address - self.mem.bus_addr()) as usize;
        // SAFETY: the caller passes an address inside this ring.
        unsafe {
            let control = self.trb(offset / TRB_LEN).cast::<u8>().wrapping_add(12);
            core::ptr::read_volatile(control.cast::<u32>()) & regs::trb::CYCLE != 0
        }
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
    /// Outstanding commands in submission order — the reference's
    /// `xhci->cmd_list`. Commands complete in the order they were issued, so a
    /// completion always belongs to the oldest entry; matching by TRB address
    /// instead (what this driver used to do) turns a lost completion into a
    /// misattributed one.
    commands: alloc::collections::VecDeque<PendingCmd>,
    /// The devices on this controller and the class bindings published for them.
    /// Created by the probe (which is where a `DeviceHandle` exists) and used
    /// afterwards, so a device can be withdrawn or published at runtime.
    bus: Option<alloc::sync::Arc<ksync::Mutex<usb::bus::UsbBus>>>,
    /// Root ports that reported a connection but did not answer when brought up.
    ///
    /// A stacked connector exposes the same physical port in both protocol sets, and
    /// the USB2 side of a SuperSpeed device accepts a port reset but then ignores
    /// every control transfer. `hub_port_init()` gives up on such a device; this
    /// driver used to re-try the whole enumeration for each pass, spending a control
    /// timeout (5 s) each time. One port, one attempt, and it is named in the log.
    dead_ports: alloc::vec::Vec<usize>,
    /// Last `(PORTSC, PORTPMSC)` sampled per root port, and when the last transfer
    /// entry sampled them (see `sample_ports_before_transfer()`).
    last_port_sample: alloc::vec::Vec<(u32, u32)>,
    last_transfer_ns: u64,
    /// A weak handle to this controller, so a runtime path can hand the block
    /// backend the same `Arc` probe did. Weak, not strong: the controller must not
    /// keep itself alive.
    self_weak: Option<alloc::sync::Weak<ksync::Mutex<Xhci>>>,
    /// A Port Status Change event arrived since the last port scan. Polled
    /// drivers still need it: it is the only signal that a port changed while
    /// nobody was looking at it.
    port_change: bool,
    /// A Host Controller Error event arrived: the controller needs a reset.
    hc_error: bool,
    /// Set by the event dispatcher when a Command Ring Stopped event (TRB type
    /// 37) arrives. `abort_command_ring()` alternates on it across its two
    /// steps: abort (expect the event) and restart (expect nothing).
    ring_stopped: bool,
    /// The completion of the oldest command, held for the caller that issued it
    /// (Linux hands this back through the per-command `completion` object; this
    /// driver has one command in flight at a time, so one slot is the same
    /// thing). A command completed by the dispatcher with no waiter is dropped
    /// here — the reference's `cmd_list` head is removed at dispatch for the
    /// same reason: a dispatched command must not stay at the head, or every
    /// later completion is compared against it and desynchronizes the queue.
    completed: Option<PendingCmd>,
    /// Endpoint-recovery commands the event dispatcher asked for. They are
    /// issued right after the event that requested them, never from inside a
    /// handler: the dispatcher must stay non-reentrant so every completion is
    /// drained by the same in-order loop.
    deferred: alloc::collections::VecDeque<Trb>,
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
/// The count's bit layout is ambiguous on this silicon. The reference's macro
/// reads `HCSPARAMS2[25:21] << 5 | HCSPARAMS2[31:27]`; the spec-shaped reading
/// of `hcsparams2 = 0x0c0000f1` gives `bits31:27 << 5` = 32. Both boots where
/// the controller died with `HSE + GBUSERRADDR=0xC` (the shape of a
/// NULL-scratchpad-pointer DMA) postdate the array shrinking to one entry, so
/// the ambiguity is resolved the expensive-but-certain way: allocate the
/// LARGER count and point every entry at its own valid page. Whatever count
/// the controller really uses, every pointer it can read is live. When the
/// count is non-zero the DCBAA's first entry must point at the array *before*
/// the controller is started; without it the first command that allocates a
/// device slot (`Enable Slot`) fails with a TRB error while a No-Op still
/// succeeds.
/// A/B bisect for the HSE death at the mouse's Address Device: pin the
/// count to the reference macro's decode (1 on this silicon) and keep
/// everything else at the current state. If the death persists, the
/// scratchpad size is exonerated and the trigger is the event-handling
/// change's timing.
fn allocate_scratchpads(
    dev: &Device<'_, driver_base::PlatformBus>,
    hcsparams2: u32,
    device_contexts: &CoherentMem,
) -> Result<Option<Scratchpads>, ProbeError> {
    let count = ((((hcsparams2 >> 21) & 0x1f) << 5) | ((hcsparams2 >> 27) & 0x1f)) as usize;
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
/// One transfer descriptor, as the reference tracks it (`struct xhci_td`).
///
/// A control transfer is one TD spanning its setup, data and status TRBs, and
/// the completion event names the TRB it stopped at — `xhci_trb_in_td()` uses
/// exactly that to decide which transfer an event finishes.
#[derive(Clone, Copy)]
struct Td {
    /// Bus address of the first TRB (the setup TRB for a control transfer).
    first_trb: u64,
    /// Bus address of the last TRB (the status TRB for a control transfer).
    last_trb: u64,
    /// What was queued, for the timeout and out-of-range reports.
    what: &'static str,
}

/// One bulk endpoint's recovery state (`struct xhci_virt_ep`'s `ep_state`).
#[derive(Clone, Copy, Default)]
struct BulkEndpointState {
    /// `EP_HALTED`: a Reset Endpoint is queued or in flight.
    halted: bool,
    /// `SET_DEQ_PENDING`: a Set TR Dequeue is in flight, and the ring must not be
    /// doorbelled until it completes (`xhci_ring_ep_doorbell()` returns early).
    set_deq_pending: bool,
    /// A doorbell was withheld while `set_deq_pending`; ring it afterwards.
    doorbell_pending: bool,
    /// The descriptor whose completion halted the endpoint, to be turned into
    /// No-Ops when the reset takes effect (`cancelled_td_list`).
    halted_td: Option<Td>,
}

/// A command TRB waiting for its completion: one entry of `xhci->cmd_list`.
struct PendingCmd {
    /// Bus address of the command TRB itself, which is what a completion
    /// event's parameter word carries.
    trb_addr: u64,
    trb_type: u32,
    slot_id: u8,
    ep_id: u8,
    /// The completion event, once the dispatcher routed it here.
    done: Option<Trb>,
}

struct Slot {
    id: u8,
    speed: u8,
    /// Root port this slot was addressed on (`None` until `Address Device`
    /// completes), so the addressed address can be reported per port.
    port: Option<usize>,
    /// The bus the device hangs off: `None` for a root port, `Some((hub address,
    /// hub port))` for a child. A device that stops answering is brought back by
    /// resetting *that* port and re-addressing it -- the same path
    /// `usb_reset_and_verify_device()` takes, minus the class-driver rebind.
    parent: Option<(u8, u8)>,
    /// Root port and route the slot was addressed with, so a re-address repeats
    /// them.
    root_port: usize,
    route: u32,
    input: CoherentMem,
    ep0: Ring,
    /// Bulk endpoint rings, created when the device's endpoints are configured
    /// (index 0 = OUT, 1 = IN); `None` until then.
    bulk: [Option<Ring>; 2],
    /// Device Context Index of each bulk endpoint (`2 * ep_number + direction`),
    /// which is what the doorbell and every transfer event carry.
    bulk_dci: [u8; 2],
    /// Each bulk endpoint's `wMaxPacketSize` as the descriptor stated it (512 for
    /// high speed, 1024 for SuperSpeed). `xhci_queue_bulk_tx()` reads it from the
    /// endpoint for `xhci_td_remainder()`, so a SuperSpeed transfer computed with
    /// a high-speed value is a lie about the TD size.
    bulk_max_packet: [u16; 2],
    /// Per-direction bulk endpoint state: the halt/SET_DEQ bookkeeping the
    /// reference keeps in `xhci_virt_ep`, which this driver only had for EP0.
    bulk_state: [BulkEndpointState; 2],
    /// Outstanding bulk descriptors per direction, in submission order.
    bulk_tds: [alloc::collections::VecDeque<Td>; 2],
    /// Completed bulk transfers per direction, in giveback order.
    bulk_completions: [alloc::collections::VecDeque<Trb>; 2],
    /// The hub's status-change (interrupt IN) ring, created with the hub's
    /// endpoint configuration.
    interrupt: Option<Ring>,
    /// Device context array entry handed to the controller by `Address Device`.
    device: CoherentMem,
    /// `ep->td_list`: outstanding transfer descriptors in submission order.
    tds: alloc::collections::VecDeque<Td>,
    /// Completed transfers, in the order the controller finished them
    /// (`usb_hcd_giveback_urb()` order). The waiting transfer takes the front.
    ep0_completions: alloc::collections::VecDeque<Trb>,
    /// `ep->ep_state & EP_HALTED`: a Reset Endpoint is queued or in flight.
    ep0_halted: bool,
    /// The descriptor whose completion halted the endpoint; its TRBs are turned
    /// into No-Ops when the Reset Endpoint completes (`cancelled_td_list`).
    halted_td: Option<Td>,
    /// `ep->ep_state & EP_STOP_CMD_PENDING`: a Stop Endpoint is in flight.
    stop_pending: bool,
    /// `ep->ep_state & SET_DEQ_PENDING`: a Set TR Dequeue is in flight, and the
    /// ring must not be doorbelled until it completes — doorbelling first starts
    /// the ring with the old dequeue (`xhci_move_dequeue_past_td()`).
    set_deq_pending: bool,
    /// A doorbell was withheld while `set_deq_pending`; ring it afterwards.
    doorbell_pending: bool,
    /// `ep->err_count`: transferless errors are retried softly a bounded number
    /// of times before the hard reset (`handle_transferless_tx_event()`).
    ep0_err_count: u32,
    /// The endpoint stopped answering once (a wire/forwarding death): later
    /// transfers to this slot fast-fail instead of waiting out the full 5 s
    /// each, so a dead child costs the boot milliseconds, not minutes.
    wire_dead: bool,
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
/// Writes the input control context's `add` flags (dword 1).
fn write_input_control(input: *mut u32, add_flags: u32) {
    // SAFETY: dword 1 of the input context, which the caller owns.
    unsafe {
        core::ptr::write_volatile(input.wrapping_add(1), add_flags);
    }
}

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

/// The generic 64-bit register write: low word first, then high
/// (`xhci_write_64` = `lo_hi_writeq`). DCBAAP, CRCR and ERDP all take this
/// order; ERSTBA is the one register the `WRITE_64_HI_LO` quirk reverses.
fn write64_lo_hi(bar: &IoMap, offset: usize, value: u64) {
    bar.write32(offset, value as u32);
    bar.write32(offset + 4, (value >> 32) as u32);
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
    // `xhci_halt()`: XHCI_MAX_HALT_USEC (32 ms) for the halt to stick.
    while bar.read32(operational + op::USBSTS) & sts::HCH == 0 && waited < HALT_POLL_US {
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
    // `xhci_reset(XHCI_RESET_SHORT_USEC)`: 250 ms for HCRST to self-clear and
    // the same for CNR to drop; both were once given 20 ms, which only ever
    // worked because warm boots reset faster than that.
    while bar.read32(operational + op::USBCMD) & cmd::HCRST != 0 && waited < RESET_POLL_US {
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
    for _ in 0..RESET_POLL_US {
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
            commands: alloc::collections::VecDeque::new(),
            bus: None,
            dead_ports: alloc::vec::Vec::new(),
            last_port_sample: alloc::vec::Vec::new(),
            last_transfer_ns: 0,
            self_weak: None,
            port_change: false,
            hc_error: false,
            ring_stopped: false,
            completed: None,
            deferred: alloc::collections::VecDeque::new(),
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
        // Generic 64-bit order (`xhci_write_64` = `lo_hi_writeq`): low half
        // first. The hi-first order here was an artefact of generalising the
        // ERSTBA quirk to every register; ERSTBA is the only hi-first one.
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
                    // xHCI 6.5.3.3: the segment size sits in dword 2 bits 31:16 and
                    // bits 15:0 are RsvdZ. Linux writes `TRBS_PER_SEGMENT` raw
                    // (`xhci_alloc_erst()`, xhci-mem.c:1897), which puts the count in
                    // the reserved half; on a controller that reads the field as the
                    // spec describes, the segment size is then zero and its idea of
                    // the ring's length -- and so of when the cycle bit flips --
                    // diverges from ours. That divergence is what a decoder's lap
                    // parity cannot survive, and it fits every symptom seen here:
                    // events stop being visible while the controller still asserts
                    // EINT, and later the core faults.
                    ring_segment_size: (EVENT_RING_TRBS as u32) << ERST_SEG_SIZE_SHIFT,
                    reserved: 0,
                },
            );
        }
        info!(
            "xhci: erst entry base {:#x} size {:#x} (shift {ERST_SEG_SIZE_SHIFT})",
            self.event.mem.bus_addr(),
            (EVENT_RING_TRBS as u32) << ERST_SEG_SIZE_SHIFT,
        );
        self.dump_erst("programmed");
        self.log_allocations("init");
        // Linux `xhci_add_interrupter` keeps the fields it does not own: ERSTSZ
        // only carries the segment count, ERSTBA only the base address.
        let erstsz = self.runtime + RT_IR0 + 0x08;
        self.bar
            .write32(erstsz, (self.bar.read32(erstsz) & !0xffff) | 1);
        // ERSTBA is the one 64-bit register the reference writes HIGH word first
        // when the `WRITE_64_HI_LO` quirk is set (`xhci_add_interrupter`); every
        // other 64-bit register — DCBAAP, CRCR, ERDP — goes through
        // `xhci_write_64` = `lo_hi_writeq`, low word first. This controller runs
        // with the quirk (the working board's xHCI quirks word has bit 47 set),
        // so both orders below are the reference's, not one order for all.
        write64_hi_lo(
            &self.bar,
            self.runtime + RT_IR0 + 0x10,
            self.erst.bus_addr(),
        ); // ERSTBA0
        write64_lo_hi(&self.bar, self.runtime + 0x18, self.event.mem.bus_addr()); // ERDP
        write64_lo_hi(
            &self.bar,
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
            "xhci: command ring {:#x}, event ring {:#x}, dcbaa {:#x}, erst {:#x}, bounce {:#x}",
            self.command.mem.bus_addr(),
            self.event.mem.bus_addr(),
            self.device_contexts.bus_addr(),
            self.erst.bus_addr(),
            self.bounce.bus_addr(),
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
    /// Hands the controller the device table it publishes through.
    ///
    /// The table is created by the probe, which is the only place a
    /// `driver_base::DeviceHandle` can be obtained; from then on it belongs to the
    /// controller's state, so a port scan happens later without needing a `Device`.
    pub fn attach_bus(&mut self, bus: alloc::sync::Arc<ksync::Mutex<usb::bus::UsbBus>>) {
        self.bus = Some(bus);
    }

    /// Records a weak handle to this controller for runtime publication.
    pub fn attach_self(&mut self, this: &alloc::sync::Arc<ksync::Mutex<Xhci>>) {
        self.self_weak = Some(alloc::sync::Arc::downgrade(this));
    }

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

/// `xhci_halt()`'s `XHCI_MAX_HALT_USEC`: 32 ms for Run/Stop to take effect.
const HALT_POLL_US: usize = 32_000;

/// `xhci_reset(XHCI_RESET_SHORT_USEC)`: 250 ms for HCRST to self-clear and for
/// CNR to drop (the long variant is 10 s on the resume path).
const RESET_POLL_US: usize = 250_000;

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

/// How long to wait for a command or transfer completion: the reference's
/// command timeout is `XHCI_CMD_DEFAULT_TIMEOUT` = 5 s and the USB core's
/// default control-urb timeout is also 5 s, so a slow device is given the same
/// window here instead of failing earlier than Linux would. 1 ms polling
/// steps; a completion that arrives early is caught on the next poll.
const EVENT_POLL_STEPS: usize = 5_000;

/// Fast-fail for a slot whose wire already timed out once: a dead wire
/// answers nothing no matter how long software waits.
const WIRE_DEAD_POLL_STEPS: usize = 250;

/// Root port reset: how long to poll PORTSC for PRC / the enabled state, in
/// 1 µs steps (the hub layer allows `HUB_RESET_TIMEOUT`, 1 s, for a reset to
/// finish; this controller completes one in single-digit milliseconds).
const PORT_RESET_POLL_STEPS: usize = 200_000;

/// How many events one `pump_events()` pass will dispatch before returning.
///
/// A pass must terminate even if the controller publishes a flood: a port
/// status change on every poll would otherwise keep the loop from ever
/// consulting the waiter again. The value also caps the unclaimed completions
/// kept per endpoint.
const EVENT_DRAIN_LIMIT: usize = 64;

/// `MAX_SOFT_RETRY` (`xhci-ring.c:2588`): how many times a transferless error is
/// retried with a soft Reset Endpoint before the endpoint is hard-reset.
const MAX_SOFT_RETRY: u32 = 3;

/// `xhci_halted_host_endpoint()` (`xhci-ring.c:2191`): which completion codes
/// leave the endpoint Halted.
///
/// On xHCI a STALL always halts, *including* a control pipe protocol stall, and
/// a USB/Split Transaction Error halts every non-isochronous endpoint. This is
/// why one failed control transfer makes EP0 consume nothing at all: the ring
/// stops at the failed TRB, and without a Reset Endpoint there is no recovery.
///
/// The reference exempts isochronous endpoints from the transaction-error rule
/// and treats a babbling control endpoint as halted only on controllers newer
/// than xHCI 0.95 — which this one is. This driver configures no isochronous
/// endpoint, so every endpoint it tracks follows the full rule.
fn halts_host_endpoint(code: u8, ep_state: u32) -> bool {
    // `finish_td()` (`xhci-ring.c:2204`, board kernel 5.10): a STALL always
    // halts, but the transaction/babble/split errors only halt when the endpoint
    // context actually says Halted -- a controller that reports one of them
    // without halting keeps running, and resetting it then would throw away the
    // endpoint's state. The newer kernels factor this rule out into
    // `xhci_halted_host_endpoint()`; 5.10 has it inline, which is what this
    // follows.
    const EP_STATE_HALTED: u32 = 1;
    match code {
        regs::code::STALL => true,
        regs::code::BABBLE
        | regs::code::TRANSACTION_ERROR
        | regs::code::SPLIT_TRANSACTION_ERROR => ep_state == EP_STATE_HALTED,
        _ => false,
    }
}

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

/// `fls` — index of the highest set bit (1-based), as the reference's helper
/// of the same name; 0 for 0.
fn fls(value: u32) -> u32 {
    32 - u32::from(value.leading_zeros())
}

/// Endpoint context parameters for one endpoint descriptor, transcribed from
/// the reference's `xhci_endpoint_init` fill sequence: returns `(interval,
/// max_burst, max_esit_payload)`.
///
/// - interval — `xhci_get_endpoint_interval`: control/bulk endpoints are
///   serviced on demand (0; high-speed NAK rates are not used here),
///   SuperSpeed and high-speed interrupt endpoints take `bInterval - 1` as
///   the exponent of 125 µs units (`xhci_parse_exponent_interval`), and
///   full/low-speed interrupt endpoints convert 1 ms frames with
///   `fls(8·bInterval) - 1` clamped to 3..=10 (`xhci_parse_frame_interval`).
///   Full-speed isochronous counts frames, not microframes, so its exponent
///   is shifted by 3.
/// - max_burst — `xhci_get_endpoint_max_burst`: the companion descriptor's
///   `bMaxBurst` for SuperSpeed, the extra packets per microframe
///   (`wMaxPacketSize >> 11`) for high-speed periodic endpoints, else 0.
/// - max_esit_payload — `usb_endpoint_max_periodic_payload`: the companion's
///   `wBytesPerInterval` for SuperSpeed, `max_packet · maxp_mult` otherwise,
///   and 0 for control/bulk.
fn endpoint_context_params(
    speed: usb::Speed,
    ep: &usb::descriptor::EndpointDescriptor,
) -> (u32, u32, u32) {
    let transfer = ep.transfer_type();
    let periodic = transfer == 1 || transfer == 3; // isochronous | interrupt
    let max_packet = u32::from(ep.max_packet_size) & 0x7ff; // usb_endpoint_maxp
    let extra_per_uframe = (u32::from(ep.max_packet_size) >> 11) & 0x3; // maxp_mult - 1
    let exponent_interval = |b: u8| u32::from(b.clamp(1, 16)) - 1;
    let frame_interval = |b: u8| fls(u32::from(b) * 8).saturating_sub(1).clamp(3, 10);
    let interval = if !periodic {
        0
    } else {
        match speed {
            usb::Speed::High | usb::Speed::Super | usb::Speed::SuperPlus => {
                exponent_interval(ep.interval)
            }
            usb::Speed::Full => {
                if transfer == 1 {
                    exponent_interval(ep.interval) + 3
                } else {
                    frame_interval(ep.interval)
                }
            }
            usb::Speed::Low => frame_interval(ep.interval),
            usb::Speed::Unknown => 0,
        }
    };
    let max_burst = match speed {
        usb::Speed::Super | usb::Speed::SuperPlus => u32::from(ep.max_burst),
        usb::Speed::High if periodic => extra_per_uframe,
        _ => 0,
    };
    let max_esit = if !periodic {
        0
    } else if matches!(speed, usb::Speed::Super | usb::Speed::SuperPlus) {
        u32::from(ep.bytes_per_interval)
    } else {
        max_packet * (extra_per_uframe + 1)
    };
    (interval, max_burst, max_esit)
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
            // Nothing published at this slot. Note that after a wrap the slot may
            // still hold the *previous* lap's TRB, which is indistinguishable from
            // "not yet written" by the cycle bit alone -- so a mismatch here is not
            // evidence of anything and must not be "resynchronized": adopting the
            // old parity would dispatch last lap's completions as if they were new.
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
        // ERDP takes the generic low-word-first order (`xhci_write_64`); the
        // low word carries EHB, so it lands last and clears the flag only after
        // the new pointer is in place.
        let dequeue = self.event.mem.bus_addr() + (self.event_dequeue * TRB_LEN) as u64;
        write64_lo_hi(
            &self.bar,
            self.runtime + RT_IR0 + RT_ERDP,
            dequeue | u64::from(ERDP_EHB),
        );
        Some(trb)
    }

    /// One bulk transfer over an already configured endpoint.
    ///
    /// The descriptor chain follows `xhci_queue_bulk_tx()` (5.10
    /// drivers/usb/host/xhci-ring.c:3580):
    ///
    /// - a buffer must not cross a 64 KiB boundary, so one transfer becomes one
    ///   TRB per such boundary (`TRB_BUFF_LEN_UP_TO_BOUNDARY`);
    /// - every TRB but the last carries `CHAIN`; the last clears it and sets
    ///   `IOC`, which is the completion this caller waits for;
    /// - IN transfers set `ISP` so a short packet ends the TD instead of waiting
    ///   for a full-length one;
    /// - the TRB length / TD size fields: `TD_SIZE` is the number of packets
    ///   *remaining* after this TRB, in packets of the endpoint's max packet
    ///   size (`xhci_td_remainder`), and a wrong value makes the controller
    ///   treat a multi-TRB transfer as complete after the first one.
    ///
    /// The whole chain is published only after it is built (`push_held` +
    /// `publish`), which is `xhci_queue_bulk_tx()`'s "don't give the first TRB to
    /// the hardware until the others exist".
    fn bulk_transfer_slot(
        &mut self,
        slot_index: usize,
        endpoint: u8,
        data: &mut [u8],
        is_in: bool,
    ) -> KResult<usize> {
        self.bulk_transfer_on(slot_index, usize::from(is_in), endpoint, data, is_in)
    }

    /// One bulk transfer on a chosen endpoint ring.
    ///
    /// `ring_index` selects which of the two configured endpoints carries it (0 =
    /// the OUT one, 1 = the IN one) and `is_in` sets the TRB's direction bits.
    /// They are separate arguments because they *are* separate things: the ring
    /// belongs to an endpoint, the direction belongs to the transfer. A bring-up
    /// probe needs to address one endpoint's ring with the other direction to see
    /// whether the controller runs the TRB at all.
    fn bulk_transfer_on(
        &mut self,
        slot_index: usize,
        direction: usize,
        endpoint: u8,
        data: &mut [u8],
        is_in: bool,
    ) -> KResult<usize> {
        let _ = endpoint;
        let slot_id = self.slots[slot_index].id;
        if self.slots[slot_index].wire_dead {
            return Err(KError::Io);
        }
        if self.slots[slot_index].bulk[direction].is_none() {
            return Err(KError::Unsupported);
        }
        let length = data.len();
        if length > BOUNCE_LEN {
            return Err(KError::InvalidInput);
        }
        let max_packet = usize::from(self.slots[slot_index].bulk_max_packet[direction]);
        if !is_in && length > 0 {
            // The controller reads the buffer, so it has to be device-visible.
            // SAFETY: the bounce buffer is BOUNCE_LEN bytes, `length` was checked
            // against it, and the caller's slice holds `length` bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(data.as_ptr(), self.bounce().as_ptr(), length);
            }
        }
        let buffer = self.bounce.bus_addr();
        // Build the chain back to front is not possible here (each TRB needs the
        // address of its own forward slot), so it is built forward with the first
        // TRB held unpublished until the rest exist.
        let mut addresses: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
        let mut offset = 0usize;
        let mut held: Option<(usize, bool)> = None;
        loop {
            let address = buffer + offset as u64;
            // `TRB_BUFF_LEN_UP_TO_BOUNDARY`: stop at the 64 KiB line.
            let to_boundary = 0x1_0000 - (address & 0xffff) as usize;
            let mut trb_len = to_boundary.min(length - offset);
            let last = offset + trb_len >= length;
            let mut control = regs::trb::NORMAL << regs::trb::TYPE_SHIFT;
            if is_in {
                // `xhci_queue_bulk_tx()` sets ISP for IN and nothing else: DIR_IN
                // (bit 16) belongs to the control transfer's DATA and STATUS
                // TRBs, and it is reserved in a Normal TRB. Setting it there made
                // the controller skip the TRB entirely -- the CBW completed, the
                // data stage's doorbell was rung, and the endpoint's dequeue never
                // moved.
                control |= regs::trb::ISP;
            }
            if last {
                control |= regs::trb::IOC;
            } else {
                control |= regs::trb::CHAIN;
            }
            // `xhci_td_remainder()`: packets left after this TRB, that is
            // ceil(remaining / max_packet) - (bytes of this TRB) / max_packet.
            let remaining = length - offset - trb_len;
            let packets_left = remaining.div_ceil(max_packet);
            let td_size = packets_left.min(31) as u32;
            let status = (trb_len as u32 & 0x1ffff) | (td_size << regs::trb::TD_SIZE_SHIFT);
            let trb = Trb::new(address, status, control);
            if held.is_none() {
                let pushed = self.slots[slot_index].bulk[direction]
                    .as_mut()
                    .expect("bulk ring")
                    .push_held(trb);
                held = Some((pushed.0, pushed.1));
                addresses.push(
                    self.slots[slot_index].bulk[direction]
                        .as_ref()
                        .expect("bulk ring")
                        .mem
                        .bus_addr()
                        + (pushed.0 * TRB_LEN) as u64,
                );
            } else {
                let pushed = self.slots[slot_index].bulk[direction]
                    .as_mut()
                    .expect("bulk ring")
                    .push(trb);
                addresses.push(pushed);
            }
            offset += trb_len;
            if last {
                break;
            }
            // A zero-length remainder would loop forever; `trb_len` is derived
            // from a non-zero `length` and a boundary that is at least 1 byte.
            if trb_len == 0 {
                trb_len = 1;
            }
        }
        let first_trb = *addresses.first().ok_or(KError::InvalidInput)?;
        let last_trb = *addresses.last().ok_or(KError::InvalidInput)?;
        self.slots[slot_index].bulk_tds[direction].push_back(Td {
            first_trb,
            last_trb,
            what: "bulk transfer",
        });
        let dci = self.slots[slot_index].bulk_dci[direction];
        if let Some(held) = held {
            // `giveback_first_trb()`: the first TRB of the chain becomes visible
            // only now, with the cycle bit that was current when it was written.
            self.slots[slot_index].bulk[direction]
                .as_mut()
                .expect("bulk ring")
                .publish(held);
        }
        // Where the controller is reading versus where we just wrote: the two must
        // agree, or every later TRB is behind its read pointer (see
        // `log_bulk_position()`).
        self.log_bulk_position(slot_index, direction, "submitting");
        // `xhci_ring_ep_doorbell()` skips the doorbell while a cancellation is
        // pending *or* the endpoint is halted: ringing it then restarts the ring at
        // the dequeue the recovery is moving away from.
        let state = self.slots[slot_index].bulk_state[direction];
        if state.set_deq_pending || state.halted {
            info!(
                "xhci: bulk doorbell withheld (halted {}, set deq pending {})",
                state.halted, state.set_deq_pending
            );
            self.slots[slot_index].bulk_state[direction].doorbell_pending = true;
        } else {
            self.ring_doorbell(slot_id, dci);
        }
        let event = self.wait_for_bulk(slot_index, direction)?;
        let code = event.completion_code();
        if code != regs::code::SUCCESS && code != regs::code::SHORT_PACKET {
            log::error!(
                "xhci: bulk {} on slot {slot_id} ep {endpoint:#04x} failed, code {code}",
                if is_in { "in" } else { "out" }
            );
            return Err(KError::Io);
        }
        // The completion event carries what the controller did *not* transfer.
        let residual = (event.status & regs::trb::STATUS_LEN_MASK) as usize;
        let transferred = length.saturating_sub(residual);
        if is_in && transferred > 0 {
            // SAFETY: the controller wrote `transferred` bytes into the bounce
            // buffer, which is at least that long, and the caller's slice
            // matches `length >= transferred`.
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

    /// `xhci_trb_in_td()`: may `trb` belong to the descriptor `td`?
    ///
    /// A TD that crosses the ring's end has `first_trb > last_trb`, and a plain
    /// `first..=last` range test answers "no" for exactly the TRBs it does own --
    /// which is what a log line reading `outside head TD 0x1010bff0..0x1010b010`
    /// was saying. The reference walks the ring's segments for this reason; with a
    /// single-segment ring the same result is the wrapped interval below.
    ///
    /// Treating a wrapped TD as "outside" was not merely noisy: the handler returns
    /// without consuming the head TD, so one such event owns the head permanently and
    /// every later completion is discarded as "not the head TD".
    fn trb_in_td(td: &Td, trb: u64) -> bool {
        if td.first_trb <= td.last_trb {
            trb >= td.first_trb && trb <= td.last_trb
        } else {
            trb >= td.first_trb || trb <= td.last_trb
        }
    }

    /// `xhci_handle_halted_endpoint()` for a bulk endpoint.
    ///
    /// A bulk endpoint that returns a completion the controller considers fatal
    /// (Stall / Transaction Error / Babble) is left in the Halted state and
    /// consumes nothing more: every later transfer on it, including the CBW of
    /// the next BOT command, simply never completes. The reference resets the
    /// endpoint (soft reset, `TRB_TSP`) and then moves the ring's dequeue past the
    /// descriptor that stalled, turning its TRBs into No-Ops first so the
    /// controller's cached copies cannot be fetched again.
    fn recover_bulk_endpoint(&mut self, slot_index: usize, direction: usize) {
        let state = self.slots[slot_index].bulk_state[direction];
        if state.halted {
            info!(
                "xhci: reset endpoint for slot {} dci {} already pending",
                self.slots[slot_index].id, self.slots[slot_index].bulk_dci[direction]
            );
            return;
        }
        let dci = self.slots[slot_index].bulk_dci[direction];
        let mut control = self.endpoint_command_field(slot_index, dci, regs::trb::RESET_ENDPOINT);
        // `EP_SOFT_RESET`: preserve the endpoint's transfer state.
        control |= regs::trb::TSP;
        self.deferred.push_back(Trb::new(0, 0, control));
        self.slots[slot_index].bulk_state[direction].halted = true;
        info!(
            "xhci: queued reset endpoint for slot {} dci {dci}",
            self.slots[slot_index].id
        );
    }

    /// `xhci_invalidate_cancelled_tds()` for a bulk endpoint: No-Op the
    /// descriptor that stalled, then re-point the ring past it.
    fn invalidate_bulk_endpoint(&mut self, slot_index: usize, direction: usize) {
        let mut last = None;
        while let Some(td) = self.slots[slot_index].bulk_tds[direction].pop_front() {
            if let Some(ring) = self.slots[slot_index].bulk[direction].as_mut() {
                ring.no_op_range(td.first_trb, td.last_trb);
            }
            last = Some(td.last_trb);
        }
        if let Some(td) = self.slots[slot_index].bulk_state[direction]
            .halted_td
            .take()
        {
            if let Some(ring) = self.slots[slot_index].bulk[direction].as_mut() {
                ring.no_op_range(td.first_trb, td.last_trb);
            }
            last = Some(last.map_or(td.last_trb, |last: u64| last.max(td.last_trb)));
        }
        let Some(ring) = self.slots[slot_index].bulk[direction].as_ref() else {
            return;
        };
        // Nothing to skip: the ring's own enqueue position is already past
        // everything that was handed to the controller.
        let next = last.map_or_else(
            || ring.mem.bus_addr() + (ring.index % ring.trbs) as u64 * TRB_LEN as u64,
            |last| last + TRB_LEN as u64,
        );
        let dcs = ring.cycle_at(next);
        let dci = self.slots[slot_index].bulk_dci[direction];
        let control = self.endpoint_command_field(slot_index, dci, regs::trb::SET_TR_DEQUEUE);
        self.deferred
            .push_back(Trb::new(next | u64::from(dcs), 0, control));
        // Withhold the doorbell until this completes: ringing it first restarts
        // the ring at the dequeue the recovery is moving away from.
        self.slots[slot_index].bulk_state[direction].set_deq_pending = true;
        self.slots[slot_index].bulk_state[direction].doorbell_pending = true;
        info!(
            "xhci: re-pointed slot {} dci {dci} past the halted descriptor to {next:#x}",
            self.slots[slot_index].id
        );
    }

    /// Waits for the oldest outstanding bulk descriptor of one direction.
    fn wait_for_bulk(&mut self, slot_index: usize, direction: usize) -> Result<Trb, KError> {
        let steps = if self.slots[slot_index].wire_dead {
            WIRE_DEAD_POLL_STEPS
        } else {
            EVENT_POLL_STEPS
        };
        for _ in 0..steps {
            self.pump_events();
            let state = self.slots[slot_index].bulk_state[direction];
            if state.doorbell_pending && !state.set_deq_pending && !state.halted {
                self.slots[slot_index].bulk_state[direction].doorbell_pending = false;
                let dci = self.slots[slot_index].bulk_dci[direction];
                let slot_id = self.slots[slot_index].id;
                self.ring_doorbell(slot_id, dci);
            }
            if let Some(event) = self.slots[slot_index].bulk_completions[direction].pop_front() {
                self.slots[slot_index].wire_dead = false;
                return Ok(event);
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_000));
        }
        self.log_event_timeout("bulk transfer");
        // A descriptor that never completed leaves the endpoint in a state the
        // controller will not advance: reset it and move past the descriptor, so
        // the caller's retry starts from a clean ring. (The slot is *not* marked
        // wire-dead: unlike a device that stopped answering it entirely, this is
        // recoverable, and the BOT layer retries once.)
        if let Some(td) = self.slots[slot_index].bulk_tds[direction].front().copied() {
            self.slots[slot_index].bulk_state[direction].halted_td = Some(td);
        }
        // Deliberately *no* endpoint reset here.
        //
        // A timeout is not a halt: the device never told us anything, so resetting
        // the host-side pipe (`Reset Endpoint`) alone leaves the two sides holding
        // different pipe state -- for a SuperSpeed endpoint that is a link-level
        // sequence mismatch, and the device then drops every packet on that pipe. The
        // reference never does this: a timed-out URB just returns, and the host-side
        // reset only happens inside `usb_clear_halt()` *after* the device's
        // `CLEAR_FEATURE(ENDPOINT_HALT)` succeeded (`usb_stor_reset_common()` sends
        // exactly that as its recovery). That ordering is what this driver now
        // follows: the class reset drives `clear_endpoint_halt()`, which resets and
        // invalidates in the right order (see `Hcd::clear_endpoint_halt`).
        //
        // The descriptor itself is still No-Op'd and the ring's dequeue moved past it
        // (`xhci_invalidate_cancelled_tds()`), because that part only touches the ring:
        // leaving a dead TRB at the head would make every later transfer wait behind a
        // descriptor the controller never advances past.
        self.invalidate_bulk_endpoint(slot_index, direction);
        // Did the controller ever start this descriptor? The endpoint context's
        // dequeue is the answer: unchanged means the doorbell/DCS never reached
        // it, advanced means it fetched the TRB and stopped somewhere after it.
        let dci = self.slots[slot_index].bulk_dci[direction];
        let offset = self.context_size * usize::from(dci);
        let base = self.slots[slot_index].device.as_ptr().cast::<u8>();
        // SAFETY: `dci` is a configured bulk endpoint, so its context is inside
        // this slot's device context; dwords 0 and 2 are state and dequeue.
        let (state, dequeue) = unsafe {
            let dword0 = core::ptr::read_volatile(base.wrapping_add(offset).cast::<u32>());
            let low = core::ptr::read_volatile(base.wrapping_add(offset + 8).cast::<u32>());
            let high = core::ptr::read_volatile(base.wrapping_add(offset + 12).cast::<u32>());
            (dword0 & 0x7, (u64::from(high) << 32) | u64::from(low))
        };
        if let Some(ring) = self.slots[slot_index].bulk[direction].as_ref() {
            // The TRB at the *enqueue* position: what the controller should be
            // looking at next. Index 0 is a leftover and says nothing about the
            // transfer that failed.
            // The last TRB enqueued, i.e. the one the controller's dequeue is
            // sitting on: what it was actually given.
            let enqueue_index = (ring.index + ring.trbs - 1) % ring.trbs;
            let trb = ring.read(enqueue_index);
            // The link state at the moment of the timeout, and the port's power
            // management word: a SuperSpeed link that fell into U1/U2/U3 stops
            // answering IN tokens, which looks exactly like a device that took the
            // CBW and then went silent.
            let port = self.slots[slot_index].port.unwrap_or(0);
            let base = self.operational + op::PORTSC_BASE + port * op::PORT_STRIDE;
            let portsc = self.bar.read32(base);
            let pmsc = self.bar.read32(base + 4);
            log::error!(
                "xhci: bulk timeout: port {} portsc {portsc:#010x} (ccs {}, ped {}, pls {}) pmsc \
                 {pmsc:#010x} (u1 timeout {}, u2 timeout {}, fie {})",
                port + 1,
                portsc & portsc::CCS != 0,
                portsc & portsc::PED != 0,
                (portsc >> 5) & 0xf,
                pmsc & 0xff,
                (pmsc >> 8) & 0xff,
                pmsc & (1 << 16) != 0,
            );
            // The controller's own health at the same instant, and how far its
            // event consumer has got. A frozen MFINDEX means nothing is being
            // scheduled at all; an ERDP that never moved since the transfer
            // started means the event never existed rather than was lost.
            let gsts = self.bar.read32(0xc118);
            let usbsts = self.bar.read32(self.operational + op::USBSTS);
            let mfindex = self.bar.read32(self.runtime);
            let erdp = self.bar.read64(self.runtime + RT_IR0 + 0x18);
            log::error!(
                "xhci: bulk timeout: gsts {gsts:#010x} usbsts {usbsts:#010x} mfindex \
                 {mfindex:#010x} erdp {erdp:#x}"
            );
            // Consumer against producer: our dequeue, the cycle bit we expect
            // there, and the next few slots as the controller left them. "An event
            // exists that we never look at" and "the controller posted nothing"
            // look identical from the outside; this separates them.
            {
                let mut line = alloc::string::String::new();
                for ahead in 0..4usize {
                    let index = (self.event_dequeue + ahead) % self.event.trbs;
                    let slot = self.event.read(index);
                    line.push_str(&alloc::format!(
                        " [{index}] p={:#x} s={:#x} c={:#x} type {} cycle {}",
                        slot.parameter,
                        slot.status,
                        slot.control,
                        slot.trb_type(),
                        u32::from(slot.control & regs::trb::CYCLE),
                    ));
                }
                log::error!(
                    "xhci: bulk timeout: event dequeue {} expect cycle {}{line}",
                    self.event_dequeue,
                    u32::from(self.event_cycle),
                );
            }
            log::error!(
                "xhci: bulk timeout: ep dci {dci} state {state} dequeue {dequeue:#x} ring {:#x} \
                 enqueue[{enqueue_index}] p {:#x} s {:#010x} c {:#010x} (ring cycle {})",
                ring.mem.bus_addr(),
                trb.parameter,
                trb.status,
                trb.control,
                ring.current_cycle(),
            );
        }
        Err(KError::TimedOut)
    }

    /// Polls the ports once and reconciles the device table with what the
    /// controller reports.
    ///
    /// This is the polled counterpart of `hub_wq`: XK has no interrupt path for
    /// this controller, so the Port Status Change event only sets a flag (see the
    /// event dispatcher) and a caller drains it here.
    ///
    /// Returns the class bindings of every device that went away. The caller drops
    /// them **after** releasing this controller's lock -- a publication's withdrawal
    /// may wait for a consumer that needs the same lock.
    pub fn rescan(&mut self) -> alloc::vec::Vec<usb::bus::ClassBinding> {
        let mut gone = alloc::vec::Vec::new();
        let Some(bus) = self.bus.clone() else {
            return gone;
        };
        // A controller error invalidates every slot: `xhci_hc_died()`.
        if self.hc_error {
            self.hc_error = false;
            log::error!("xhci: rescan after a host controller error: dropping all devices");
            let ids: alloc::vec::Vec<u8> = self.slots.iter().map(|slot| slot.id).collect();
            for slot in ids {
                let _ = self.disable_slot(slot);
            }
            self.slots.clear();
            gone.extend(bus.lock().detach_all());
            return gone;
        }
        if !self.port_change {
            return gone;
        }
        self.port_change = false;
        // Which addresses still have a connected port. A device behind a hub is not
        // visible in the root ports, so it is only removed when its own port scan
        // says so; that path re-attaches through `bind_and_publish()`.
        //
        // `handle_port_status()` does not decide removal: it acknowledges the change
        // bits and hands the port to the hub layer, which confirms a disconnect with
        // `hub_port_debounce_be_stable()` (100 ms stable, sampled every 25 ms --
        // hub.c:111-113) before `usb_disconnect()`, and resuscitates a device whose
        // port still reports a connection. Removing on one `CCS == 0` sample is what
        // would delete a working device's table entry -- and with it its block
        // device -- on a link that merely wobbles.
        let mut connected = alloc::vec::Vec::new();
        for port in 0..self.ports {
            let value = self.port_status_raw(port);
            if value & portsc::CCS != 0 || !self.port_disconnect_confirmed(port) {
                connected.push(port);
            }
        }
        let stale: alloc::vec::Vec<u8> = {
            let bus = bus.lock();
            bus.devices()
                // `port` is 0-based for a root-port device (the index the scan
                // above used) and a hub port number for everything else, so the
                // two are only comparable for root-port devices.
                .filter(|entry| entry.device().parent.is_none())
                .filter(|entry| !connected.contains(&entry.device().port))
                .map(|entry| entry.device().address)
                .collect()
        };
        for address in stale {
            log::warn!("xhci: address {address} is gone from its port");
            let _ = self.disable_slot(address);
            self.slots.retain(|slot| slot.id != address);
            gone.extend(bus.lock().detach(address));
        }
        gone
    }

    /// Logs the event-ring segment table entry as the memory holds it.
    ///
    /// The stall dump showed the entry holding `0x1` instead of the event ring base,
    /// which makes the controller's idea of its own event ring garbage. This is the
    /// same read, placed where it can catch the change: once right after programming
    /// and once after the first command completes.
    fn dump_erst(&self, when: &str) {
        let entry = self.erst.as_ptr().cast::<u32>();
        // SAFETY: `self.erst` owns one `ErstEntry` = four dwords.
        let words = unsafe {
            [
                core::ptr::read_volatile(entry),
                core::ptr::read_volatile(entry.wrapping_add(1)),
                core::ptr::read_volatile(entry.wrapping_add(2)),
                core::ptr::read_volatile(entry.wrapping_add(3)),
            ]
        };
        info!(
            "xhci: erst [{when}] virt {:#x} bus {:#x}: [{:#x}, {:#x}, {:#x}, {:#x}]",
            self.erst.as_ptr() as usize,
            self.erst.bus_addr(),
            words[0],
            words[1],
            words[2],
            words[3],
        );
    }

    /// Logs every coherent allocation this controller handed to the hardware.
    ///
    /// The point is aliasing: if two of these share a page, the write that lands in
    /// one is a corruption of the other, and a single table makes that visible
    /// without another guessing round.
    fn log_allocations(&self, when: &str) {
        info!(
            "xhci: allocations [{when}]: erst {:#x} (virt {:#x}), event {:#x}, command {:#x}, \
             dcbaa {:#x} ({} slots), bounce {:#x}",
            self.erst.bus_addr(),
            self.erst.as_ptr() as usize,
            self.event.mem.bus_addr(),
            self.command.mem.bus_addr(),
            self.device_contexts.bus_addr(),
            self.max_slots,
            self.bounce.bus_addr(),
        );
        for slot in &self.slots {
            info!(
                "xhci: allocations [{when}]: slot {} input {:#x} device {:#x} ep0 {:#x} bulk_out \
                 {:#x} bulk_in {:#x} interrupt {:#x}",
                slot.id,
                slot.input.bus_addr(),
                slot.device.bus_addr(),
                slot.ep0.mem.bus_addr(),
                slot.bulk[0].as_ref().map_or(0, |ring| ring.mem.bus_addr()),
                slot.bulk[1].as_ref().map_or(0, |ring| ring.mem.bus_addr()),
                slot.interrupt
                    .as_ref()
                    .map_or(0, |ring| ring.mem.bus_addr()),
            );
        }
    }

    /// Samples every root port and reports what changed while nothing was running.
    ///
    /// The symptom being chased is "the device stops answering after roughly ten
    /// seconds of no traffic", and the two candidate mechanisms -- the link going
    /// to U1/U2/U3 without us noticing, or the device NAKing in U0 -- are
    /// indistinguishable from the failure log alone, because the timeout is what
    /// produces that log and by then the link may already be back. Sampling on the
    /// way *into* every transfer instead turns it into a time series: a changed
    /// `PORTSC`/`PORTPMSC` with the idle gap attached says the link moved, and an
    /// unchanged one says it did not.
    fn sample_ports_before_transfer(&mut self) {
        let now = khal::time::monotonic_time_nanos();
        for port in 0..self.ports {
            let offset = self.operational + op::PORTSC_BASE + port * op::PORT_STRIDE;
            let portsc = self.bar.read32(offset);
            let pmsc = self.bar.read32(offset + op::PORTPMSC);
            let sample = (portsc, pmsc);
            match self.last_port_sample.get(port).copied() {
                Some(previous) if previous == sample => {}
                Some((previous_portsc, previous_pmsc)) => {
                    let idle_us = now.saturating_sub(self.last_transfer_ns) / 1_000;
                    log::info!(
                        "xhci: port {} changed while idle {idle_us} us: portsc \
                         {previous_portsc:#010x} -> {portsc:#010x} (pls {} -> {}, ped {} -> {}), \
                         pmsc {previous_pmsc:#010x} -> {pmsc:#010x}",
                        port + 1,
                        (previous_portsc & portsc::PLS_MASK) >> portsc::PLS_SHIFT,
                        (portsc & portsc::PLS_MASK) >> portsc::PLS_SHIFT,
                        previous_portsc & portsc::PED != 0,
                        portsc & portsc::PED != 0,
                    );
                }
                None => log::info!(
                    "xhci: port {} first sample portsc {portsc:#010x} (pls {}, ped {}, ccs {}), \
                     pmsc {pmsc:#010x}",
                    port + 1,
                    (portsc & portsc::PLS_MASK) >> portsc::PLS_SHIFT,
                    portsc & portsc::PED != 0,
                    portsc & portsc::CCS != 0,
                ),
            }
            if port >= self.last_port_sample.len() {
                self.last_port_sample.push(sample);
            } else {
                self.last_port_sample[port] = sample;
            }
        }
        self.last_transfer_ns = now;
    }

    /// Acknowledges the interrupter's pending flag and `USBSTS.EINT`
    /// (`xhci_irq()`'s two write-backs).
    fn acknowledge_events(&self) {
        let ir = self.runtime + RT_IR0;
        let pending = self.bar.read32(ir);
        if pending & 0x1 != 0 {
            // IMAN.IP is write-one-to-clear; writing the read value back clears it
            // without disturbing IE.
            self.bar.write32(ir, pending);
        }
        let status = self.bar.read32(self.operational + op::USBSTS);
        if status & sts::EINT != 0 {
            self.bar.write32(self.operational + op::USBSTS, status);
        }
    }

    /// `hub_port_debounce_be_stable()`: whether `port` has reported a disconnect
    /// continuously for `HUB_DEBOUNCE_STABLE` ms, sampled every
    /// `HUB_DEBOUNCE_STEP` ms (`hub.c:111-113`).
    ///
    /// The change bits are deliberately not acknowledged here: this is the removal
    /// question, asked after the port change has already been consumed.
    fn port_disconnect_confirmed(&self, port: usize) -> bool {
        let offset = self.operational + op::PORTSC_BASE + port * op::PORT_STRIDE;
        let mut stable = 0u64;
        while stable < HUB_DEBOUNCE_STABLE_US {
            if self.bar.read32(offset) & portsc::CCS != 0 {
                return false;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(HUB_DEBOUNCE_STEP_US));
            stable += HUB_DEBOUNCE_STEP_US;
        }
        true
    }

    /// Publishes a class binding for an already enumerated device.
    ///
    /// Uses the device table's `DeviceHandle`, so this works after probe returned --
    /// which is what a re-enumerated device needs.
    pub fn bind_and_publish(&mut self, address: u8) -> KResult<()> {
        let Some(bus) = self.bus.clone() else {
            return Err(KError::BadState);
        };
        let device = {
            let bus = bus.lock();
            bus.device(address).cloned().ok_or(KError::NotFound)?
        };
        let Some(pipe) = mass_storage_endpoints(&device) else {
            return Ok(());
        };
        let slot_index = self
            .slots
            .iter()
            .position(|slot| slot.id == address)
            .ok_or(KError::NotFound)?;
        // The endpoint rings were allocated in probe, where a `Device` (and so the
        // DMA allocator) exists; re-installing the contexts is enough here.
        self.reinstall_bulk_endpoints(slot_index)?;
        let handle = bus.lock().handle().clone();
        let xhci = self
            .self_weak
            .as_ref()
            .and_then(alloc::sync::Weak::upgrade)
            .ok_or(KError::BadState)?;
        let publication = publish_mass_storage(&handle, &xhci, &device)?;
        let binding = usb::bus::ClassBinding::new(
            "usb-storage",
            pipe.endpoints.interface,
            alloc::vec![alloc::boxed::Box::new(publication)],
        );
        // A refused bind hands the owners back; the publication is dropped here,
        // after the table's lock is released.
        match bus.lock().bind(address, binding) {
            Ok(()) => Ok(()),
            Err(rejected) => {
                let error = rejected.error;
                usb::bus::drop_bindings(alloc::vec![rejected.binding]);
                Err(error)
            }
        }
    }

    /// `usb_reset_and_verify_device()`, minus the class-driver rebind.
    ///
    /// A device that stops answering is brought back by resetting the port it
    /// hangs off and then re-running the address/configure sequence with the slot
    /// and the address it already had. Everything the class driver holds --
    /// endpoint addresses, the block backend, the mounted filesystem -- stays
    /// valid, which is why this is the useful half of a device reset here.
    ///
    /// What it deliberately does *not* do: read the descriptors again, rebind a
    /// class driver or republish a block device. Those need an interface/class
    /// layer that owns the publications; without it a reset can only repair a
    /// device whose identity is unchanged, and that is exactly the case this
    /// recovers from.
    pub(crate) fn reset_and_reverify(&mut self, slot_index: usize) -> KResult<()> {
        let slot_id = self.slots[slot_index].id;
        let speed = regs::speed_from_slot_id(self.slots[slot_index].speed);
        let root_port = self.slots[slot_index].root_port;
        let route = self.slots[slot_index].route;
        let parent = self.slots[slot_index].parent;
        log::warn!(
            "xhci: resetting address {slot_id} to bring it back (parent {:?})",
            parent
        );
        match parent {
            // Behind a hub: reset *that* port. The hub itself is a device on this
            // controller, so this is a control transfer to it.
            Some((hub_address, port)) => {
                if let Some(info) = usb::hub::info(self, hub_address, 2)
                    && usb::hub::reset_port_retries(self, hub_address, port, info).is_none()
                {
                    log::error!("xhci: hub {hub_address} port {port} did not reset");
                    return Err(KError::Io);
                }
            }
            None => {
                self.reset_port(root_port)?;
            }
        }
        // The device is back in the Default state at address 0; `Address Device`
        // gives it the same address the slot already had, and the endpoints the
        // class driver knows about are re-installed below.
        let max_packet = match speed {
            usb::Speed::Low => 8,
            usb::Speed::Super | usb::Speed::SuperPlus => 512,
            _ => 64,
        };
        let addressing = Addressing {
            route,
            tt: None,
            multi_tt: false,
            hub: false,
        };
        self.address_device(slot_index, root_port, speed, max_packet, addressing, false)?;
        // Endpoint contexts were dropped with the device's state: install the
        // bulk pair again (the ring contents are still there, and the dequeue is
        // re-published by Configure Endpoint).
        for direction in 0..2 {
            let dci = self.slots[slot_index].bulk_dci[direction];
            if dci != 0 {
                self.slots[slot_index].bulk_state[direction] = BulkEndpointState::default();
            }
        }
        self.reinstall_bulk_endpoints(slot_index)?;
        log::warn!("xhci: address {slot_id} is back");
        Ok(())
    }

    /// Re-installs the bulk endpoint contexts of a slot after its device was reset.
    fn reinstall_bulk_endpoints(&mut self, slot_index: usize) -> KResult<()> {
        // The configured endpoints are kept in the slot; re-running Configure
        // Endpoint with the same contexts is what the reference's
        // `xhci_discover_or_reset_device()` + `xhci_configure_endpoint()` do.
        let slot_id = self.slots[slot_index].id;
        let dci_out = self.slots[slot_index].bulk_dci[0];
        let dci_in = self.slots[slot_index].bulk_dci[1];
        if dci_out == 0 && dci_in == 0 {
            return Ok(());
        }
        let dwords = self.context_size / 4;
        let input = self.slots[slot_index].input.as_ptr().cast::<u32>();
        let device_ctx = self.slots[slot_index].device.as_ptr().cast::<u32>();
        for offset in 0..dwords {
            // SAFETY: one slot context inside each allocation.
            let value = unsafe { core::ptr::read_volatile(device_ctx.wrapping_add(offset)) };
            // SAFETY: the input context's own slot-context slot.
            unsafe {
                core::ptr::write_volatile(input.wrapping_add(dwords + offset), value);
            }
        }
        let mut add_flags = 1u32;
        let mut highest = 1u32;
        for direction in 0..2 {
            let dci = self.slots[slot_index].bulk_dci[direction];
            if dci == 0 {
                continue;
            }
            let Some(ring) = self.slots[slot_index].bulk[direction].as_ref() else {
                continue;
            };
            // The ring keeps its position and cycle; the context is re-pointed at
            // where the producer is now.
            let next = ring.mem.bus_addr() + (ring.used_since_wrap() as u64) * TRB_LEN as u64;
            let dcs = ring.cycle_at(next);
            let ctx = dwords * (dci as usize + 1);
            let write = |offset: usize, value: u32| {
                // SAFETY: inside the input context allocation.
                unsafe {
                    core::ptr::write_volatile(input.wrapping_add(offset), value);
                }
            };
            write(ctx, context::endpoint_dword0(0));
            let ep0_packet = 1024u16;
            write(
                ctx + 1,
                context::endpoint_dword1(if direction == 0 { 2 } else { 6 }, ep0_packet, 0),
            );
            write(ctx + 2, context::endpoint_dword2(next, dcs));
            write(ctx + 3, (next >> 32) as u32);
            write(ctx + 4, context::endpoint_tx_info(0, 0));
            add_flags |= 1 << dci;
            highest = highest.max(u32::from(dci));
        }
        for dci in 2..=highest {
            if add_flags & (1 << dci) != 0 {
                continue;
            }
            let gap = dwords * (dci as usize + 1);
            for dword in 0..5usize {
                // SAFETY: inside the input context allocation.
                unsafe {
                    core::ptr::write_volatile(input.wrapping_add(gap + dword), 0);
                }
            }
        }
        write_input_control(input, add_flags);
        let slot_ctx = dwords;
        let before = unsafe { core::ptr::read_volatile(input.wrapping_add(slot_ctx)) };
        // SAFETY: dword0 of the input slot context.
        unsafe {
            core::ptr::write_volatile(
                input.wrapping_add(slot_ctx),
                (before & !0xf800_0000) | ((highest + 1) << 27),
            );
        }
        let trb = Trb::new(
            self.slots[slot_index].input.bus_addr(),
            0,
            (regs::trb::CONFIGURE_ENDPOINT << regs::trb::TYPE_SHIFT)
                | (u32::from(slot_id) << regs::trb::SLOT_SHIFT),
        );
        self.command(trb, "reinstall bulk endpoints")?;
        info!("xhci: slot {slot_id} bulk endpoints reinstalled");
        Ok(())
    }

    /// `xhci_check_maxpacket()`: tell the controller the real EP0 max packet.
    ///
    /// `Address Device` can only carry a *guess* (8 for low speed, 64 for full and
    /// high, 512 for SuperSpeed) because the descriptor has not been read yet. When
    /// the descriptor then says something else -- a full-speed device with 8, 16 or
    /// 32 -- the context has to be updated with Evaluate Context, or every later
    /// control transfer is split into packets the device cannot receive.
    pub(crate) fn evaluate_ep0_max_packet(&mut self, address: u8, max_packet: u16) -> KResult<()> {
        let slot_index = self
            .slots
            .iter()
            .position(|slot| slot.id == address)
            .ok_or(KError::NotFound)?;
        let slot_id = self.slots[slot_index].id;
        let dwords = self.context_size / 4;
        let input = self.slots[slot_index].input.as_ptr().cast::<u32>();
        let device_ctx = self.slots[slot_index].device.as_ptr().cast::<u32>();
        // `xhci_endpoint_copy()` of the EP0 context, then the two fields
        // `xhci_check_maxpacket()` rewrites: the EP state must be cleared and the
        // packet size replaced.
        let ep0 = dwords * (context::EP0_CONTEXT + 1);
        for dword in 0..5usize {
            // SAFETY: the EP0 context of both contexts is inside its allocation.
            let value = unsafe { core::ptr::read_volatile(device_ctx.wrapping_add(ep0 + dword)) };
            // SAFETY: as above, on the input context.
            unsafe {
                core::ptr::write_volatile(input.wrapping_add(ep0 + dword), value);
            }
        }
        let info0 = unsafe { core::ptr::read_volatile(input.wrapping_add(ep0)) };
        // SAFETY: dword0 of the input EP0 context: EP state must read as 0.
        unsafe {
            core::ptr::write_volatile(input.wrapping_add(ep0), info0 & !0x7);
        }
        let info1 = unsafe { core::ptr::read_volatile(input.wrapping_add(ep0 + 1)) };
        // SAFETY: dword1 of the same context: Max Packet Size is bits 31:16.
        unsafe {
            core::ptr::write_volatile(
                input.wrapping_add(ep0 + 1),
                (info1 & !0xffff_0000) | (u32::from(max_packet) << 16),
            );
        }
        // `ctrl_ctx->add_flags = EP0_FLAG; ctrl_ctx->drop_flags = 0;`
        write_input_control(input, 1 << (context::EP0_CONTEXT as u32));
        let trb = Trb::new(
            self.slots[slot_index].input.bus_addr(),
            0,
            (regs::trb::EVALUATE_CONTEXT << regs::trb::TYPE_SHIFT)
                | (u32::from(slot_id) << regs::trb::SLOT_SHIFT),
        );
        self.command(trb, "evaluate context")?;
        info!("xhci: slot {slot_id} ep0 max packet updated to {max_packet}");
        Ok(())
    }

    /// Configures a device's two bulk endpoints and installs them.
    ///
    /// This is `xhci_endpoint_init()` (5.10 drivers/usb/host/xhci-mem.c:1462) for
    /// a bulk pipe plus the `Configure Endpoint` that `xhci_add_endpoint()` issues:
    ///
    /// - Endpoint type = Bulk (2), C_Err = 3, Max Packet Size from the endpoint
    ///   descriptor (512 for a high-speed or SuperSpeed bulk endpoint);
    /// - Dequeue = ring base | DCS(1);
    /// - the input control context adds SLOT and both endpoint contexts, and the
    ///   slot's `context entries` is raised to cover the highest DCI used, with
    ///   the contexts in between present and disabled (xHCI 4.4.23).
    ///
    /// Without this the endpoints simply do not exist: `Doorbell[slot] = DCI`
    /// has nothing to start, and every transfer event names an endpoint the
    /// controller never configured.
    pub(crate) fn configure_bulk(
        &mut self,
        dev: &Device<'_, driver_base::PlatformBus>,
        address: u8,
        ep_out: u8,
        ep_in: u8,
        max_packet_out: u16,
        max_packet_in: u16,
        max_burst_out: u8,
        max_burst_in: u8,
    ) -> KResult<()> {
        let slot_index = self
            .slots
            .iter()
            .position(|slot| slot.id == address)
            .ok_or(KError::NotFound)?;
        if self.slots[slot_index].bulk[0].is_some() && self.slots[slot_index].bulk[1].is_some() {
            return Ok(());
        }
        let slot_id = self.slots[slot_index].id;
        let dwords = self.context_size / 4;
        let input = self.slots[slot_index].input.as_ptr().cast::<u32>();
        let device_ctx = self.slots[slot_index].device.as_ptr().cast::<u32>();
        // The slot context Configure Endpoint presents must be the one the
        // controller already has, not a guess: copy the device's own.
        for offset in 0..dwords {
            // SAFETY: both ranges are one context inside their allocations.
            let value = unsafe { core::ptr::read_volatile(device_ctx.wrapping_add(offset)) };
            // SAFETY: as above, on the input context's slot-context slot.
            unsafe {
                core::ptr::write_volatile(input.wrapping_add(dwords + offset), value);
            }
        }
        let mut highest_dci = 1u32;
        let mut add_flags = 1u32; // SLOT
        for (direction, endpoint, max_packet, max_burst) in [
            (0usize, ep_out, max_packet_out, max_burst_out),
            (1usize, ep_in, max_packet_in, max_burst_in),
        ] {
            let ep_number = u32::from(endpoint & 0x0f);
            // `xhci_get_endpoint_index()`: DCI = 2 * ep_number + direction.
            let dci = (ep_number * 2) + direction as u32;
            let ring = Ring::with_link(
                dev.res()
                    .alloc_coherent((RING_TRBS + 1) * TRB_LEN, 64)
                    .map_err(|_| KError::NoMemory)?,
                RING_TRBS,
            );
            let ring_addr = ring.mem.bus_addr();
            let dcs = ring.cycle;
            self.slots[slot_index].bulk_dci[direction] = dci as u8;
            self.slots[slot_index].bulk_max_packet[direction] = max_packet;
            self.slots[slot_index].bulk[direction] = Some(ring);
            let ctx = dwords * (dci as usize + 1);
            let write = |offset: usize, value: u32| {
                // SAFETY: inside the input context allocation (33 contexts).
                unsafe {
                    core::ptr::write_volatile(input.wrapping_add(offset), value);
                }
            };
            // dword0: Interval/Mult are 0 for bulk (`xhci_endpoint_init` only
            // fills them for periodic pipes).
            write(ctx, context::endpoint_dword0(0));
            // dword1: EP Type is **per direction** — `xhci_get_endpoint_type()`
            // returns BULK_OUT_EP (2) or BULK_IN_EP (6), never 2 for both. An IN
            // endpoint declared as "Bulk Out" is accepted by Configure Endpoint
            // (the value is legal) and then never completes a single transfer:
            // the controller has no reason to issue IN tokens for it, and it
            // reports nothing at all -- no event, no error, just silence.
            let ep_type = if direction == 0 { 2 } else { 6 };
            // Max Burst Size comes from the endpoint's SuperSpeed companion
            // descriptor (`xhci_get_endpoint_max_burst()`); 0 means "one packet
            // per burst", which is only correct for a device that declares 0.
            write(
                ctx + 1,
                context::endpoint_dword1(ep_type, max_packet, max_burst),
            );
            write(ctx + 2, context::endpoint_dword2(ring_addr, dcs));
            write(ctx + 3, (ring_addr >> 32) as u32);
            // dword4: no ESIT for bulk; Linux leaves Average TRB Length 0 here
            // (`avg_trb_len = max_esit_payload`, and bulk has none).
            write(ctx + 4, context::endpoint_tx_info(0, 0));
            add_flags |= 1 << dci;
            highest_dci = highest_dci.max(dci);
        }
        // Every context up to the highest DCI has to be present, and the ones
        // that are not being added have to read as "disabled" (EP state 0). The
        // set is decided by `add_flags`, not by the numeric range: zeroing
        // `2..highest_dci` wiped the IN endpoint this very call configured, and
        // the controller answered Parameter Error (17) with one endpoint context
        // all zero and no endpoint in the state it had just accepted.
        for dci in 2..=highest_dci {
            if add_flags & (1 << dci) != 0 {
                continue;
            }
            let gap_ctx = dwords * (dci as usize + 1);
            for dword in 0..5usize {
                // SAFETY: inside the input context allocation.
                unsafe {
                    core::ptr::write_volatile(input.wrapping_add(gap_ctx + dword), 0);
                }
            }
        }
        write_input_control(input, add_flags);
        // Raise `context entries` to cover the highest DCI (bits 31:27).
        let slot_ctx = dwords;
        let entries = highest_dci + 1;
        let before = unsafe { core::ptr::read_volatile(input.wrapping_add(slot_ctx)) };
        // SAFETY: dword0 of the input slot context.
        unsafe {
            core::ptr::write_volatile(
                input.wrapping_add(slot_ctx),
                (before & !0xf800_0000) | (entries << 27),
            );
        }
        // What the controller is about to read, context by context: with the
        // command answered by Parameter Error (17), the invalid field is in
        // here and nowhere else.
        {
            let mut line = alloc::string::String::new();
            for context in 0..=(highest_dci as usize + 1) {
                line.push_str(&alloc::format!(" [{context}]"));
                for dword in 0..5usize {
                    let offset = if context == 0 {
                        dword
                    } else {
                        dwords * context + dword
                    };
                    // SAFETY: inside the input context allocation.
                    let value = unsafe { core::ptr::read_volatile(input.wrapping_add(offset)) };
                    line.push_str(&alloc::format!(" {value:#x}"));
                }
            }
            info!("xhci: configure bulk input ctx{line}");
        }
        let trb = Trb::new(
            self.slots[slot_index].input.bus_addr(),
            0,
            (regs::trb::CONFIGURE_ENDPOINT << regs::trb::TYPE_SHIFT)
                | (u32::from(slot_id) << regs::trb::SLOT_SHIFT),
        );
        self.command(trb, "configure bulk endpoints")?;
        info!(
            "xhci: slot {slot_id} bulk endpoints configured (out {ep_out:#04x} dci {}, in \
             {ep_in:#04x} dci {})",
            self.slots[slot_index].bulk_dci[0], self.slots[slot_index].bulk_dci[1]
        );
        // What the controller accepted: the input context above is our intent,
        // this is its answer. A DCI whose EP state is not Running (1), or whose
        // dequeue lost its DCS bit, never became usable, and no transfer queued
        // on it will be fetched no matter how correct the TRB is.
        let mut line = alloc::string::String::new();
        {
            let base = self.slots[slot_index].device.as_ptr().cast::<u8>();
            for dci in 1..=highest_dci as usize {
                line.push_str(&alloc::format!(" dci{dci}:"));
                for dword in 0..5usize {
                    let offset = self.context_size * dci + dword * 4;
                    // SAFETY: `dci` does not exceed the highest configured DCI,
                    // so this stays inside the slot's device context.
                    let value = unsafe {
                        core::ptr::read_volatile(base.wrapping_add(offset).cast::<u32>())
                    };
                    line.push_str(&alloc::format!(" {value:#x}"));
                }
            }
        }
        info!("xhci: slot {slot_id} device ctx after configure{line}");
        Ok(())
    }

    /// Drains every event the controller has published and routes it to the
    /// state it belongs to — the reference's `xhci_handle_event()`.
    ///
    /// Events are processed **in ring order** and are never "waited for" by a
    /// particular caller. That structure is the point: an event is consumed
    /// exactly once, and what it completes is decided by its TRB type, the
    /// slot/endpoint it names, and the head of that endpoint's descriptor
    /// queue. Deferring events for a different waiter (the earlier design) made
    /// a late completion from a dead transfer indistinguishable from the next
    /// transfer's own completion — and left the event ring consumed in an order
    /// the controller's own bookkeeping did not share.
    fn pump_events(&mut self) {
        // `xhci_irq()` acknowledges what it drained: the interrupter's pending flag
        // and `USBSTS.EINT`. Both are RW1C, so leaving them set does not block
        // events, but it makes them useless as evidence -- a set EINT then reads as
        // "an event is waiting" even when the ring is empty and nothing arrived,
        // which is exactly the wrong conclusion to draw while chasing a stall.
        self.acknowledge_events();
        for _ in 0..EVENT_DRAIN_LIMIT {
            // ldp:step irq.dequeue-event
            let Some(event) = self.next_event() else {
                break;
            };
            // ldp:step irq.dispatch-type
            match event.trb_type() {
                regs::trb::TRANSFER_EVENT => self.handle_tx_event(&event),
                regs::trb::COMMAND_COMPLETION => self.handle_cmd_completion(&event),
                // Port Status Change: the hotplug and link-state signal. This
                // driver polls the ports instead of taking an interrupt, so the
                // event only needs to record that a rescan is due -- dropping it
                // silently is what makes "the device was replugged" invisible.
                regs::trb::PORT_STATUS_CHANGE => {
                    let port = ((event.parameter >> 24) & 0xff) as usize;
                    info!("xhci: port {} status change event", port);
                    self.port_change = true;
                }
                // `xhci_handle_event()`'s HOST_CONTROLLER_ERROR case leads to
                // `xhci_hc_died()`: the controller has faulted and everything it
                // owns is suspect.
                regs::trb::HOST_CONTROLLER_ERROR => {
                    log::error!("xhci: host controller error event");
                    self.hc_error = true;
                }
                regs::trb::DEVICE_NOTIFICATION | regs::trb::MFINDEX_WRAP => {}
                // `xhci_handle_event()`'s `TRB_COMMAND_RING_STOPPED` case: the
                // acknowledgement of a Command Ring Abort. Nothing is queued for
                // it; `abort_command_ring()` is the waiter.
                regs::trb::COMMAND_RING_STOPPED => {
                    info!(
                        "xhci: command ring stopped event (code {})",
                        event.completion_code()
                    );
                    self.ring_stopped = true;
                }
                // Port status changes need no queue: the port scan reads PORTSC
                // directly, and PORTSC is the authoritative copy.
                _ => {}
            }
            self.issue_deferred();
        }
    }

    /// Issues the endpoint-recovery commands the dispatcher queued.
    ///
    /// Queuing instead of calling `self.command()` from inside a handler keeps
    /// the dispatcher non-reentrant: the recovery command's own completion is
    /// drained by the same loop, in ring order, like every other event.
    fn issue_deferred(&mut self) {
        while let Some(trb) = self.deferred.pop_front() {
            let control = trb.control;
            if self.command.index >= self.command.trbs {
                log::error!("xhci: command ring exhausted issuing a deferred command");
                continue;
            }
            let trb_addr = self.command.push(trb);
            let trb_type = (control & regs::trb::TYPE_MASK) >> regs::trb::TYPE_SHIFT;
            let slot_id = ((control & regs::trb::SLOT_MASK) >> regs::trb::SLOT_SHIFT) as u8;
            let ep_id = ((control >> regs::trb::EP_SHIFT) & 0x1f) as u8;
            self.commands.push_back(PendingCmd {
                trb_addr,
                trb_type,
                slot_id,
                ep_id,
                done: None,
            });
            info!("xhci: queued recovery command {trb_type} for slot {slot_id}");
            self.ring_command_doorbell();
        }
    }

    /// `handle_tx_event()`: route one transfer event to the descriptor it
    /// completes, and start the endpoint recovery the completion asks for.
    fn handle_tx_event(&mut self, event: &Trb) {
        // ldp:step irq.tx-find-ep
        let slot_id = event.slot_id();
        let ep_id = (event.control >> regs::trb::EP_SHIFT) & 0x1f;
        let code = event.completion_code();
        let Some(slot_index) = self.slots.iter().position(|slot| slot.id == slot_id) else {
            log::error!("xhci: transfer event for unknown slot {slot_id} (code {code})");
            return;
        };
        if ep_id != regs::trb::EP0 {
            self.handle_bulk_event(slot_index, ep_id, event);
            return;
        }
        // ldp:step irq.tx-td-front
        let Some(td) = self.slots[slot_index].tds.front().copied() else {
            // `handle_transferless_tx_event()`: an error with no outstanding TD
            // is a wire problem, not a data problem.
            self.handle_transferless(slot_index, code);
            return;
        };
        // `xhci_trb_in_td()`: the event must point inside the head TD's range. An
        // event pointing anywhere else belongs to a transfer that was already
        // reaped, and acting on it would finish the wrong transfer.
        if !Self::trb_in_td(&td, event.parameter) {
            log::error!(
                "xhci: event trb {:#x} outside head TD {:#x}..{:#x} (slot {slot_id}, {})",
                event.parameter,
                td.first_trb,
                td.last_trb,
                td.what,
            );
            return;
        }
        let what = td.what;
        self.slots[slot_index].tds.pop_front();
        self.slots[slot_index].wire_dead = false;
        let completions = &mut self.slots[slot_index].ep0_completions;
        if completions.len() >= EVENT_DRAIN_LIMIT {
            // Nobody is waiting for the oldest one (its waiter timed out); the
            // ring is still drained in order, so dropping it loses nothing but
            // the report.
            log::warn!("xhci: dropping an unclaimed completion for slot {slot_id}");
            completions.pop_front();
        }
        completions.push_back(*event);
        // ldp:step irq.tx-halt
        if halts_host_endpoint(code, self.ep0_state(slot_index)) {
            // `finish_td()`: this completion halts the host endpoint, so it will
            // consume nothing more until a Reset Endpoint clears the halt. The
            // completion still goes back to the waiter, which turns it into the
            // USB error the class driver expects.
            info!("xhci: {what} halted EP0 of slot {slot_id} (code {code})");
            // Keep the descriptor: its TRBs have to be turned into No-Ops once
            // the endpoint is reset (the controller caches them).
            self.slots[slot_index].halted_td = Some(td);
            self.halt_ep0(slot_index, false);
        }
    }

    /// Routes a transfer event for a bulk endpoint to that endpoint's queue.
    ///
    /// Bulk endpoints are identified by their Device Context Index (the same
    /// number the doorbell takes), not by the endpoint address in the
    /// descriptor, so the slot remembers the two DCIs it configured.
    fn handle_bulk_event(&mut self, slot_index: usize, ep_id: u32, event: &Trb) {
        let slot_id = self.slots[slot_index].id;
        let Some(direction) = (0..2).find(|dir| {
            self.slots[slot_index].bulk_dci[*dir] != 0
                && u32::from(self.slots[slot_index].bulk_dci[*dir]) == ep_id
        }) else {
            log::warn!("xhci: transfer event for slot {slot_id} ep {ep_id} with no bulk endpoint");
            return;
        };
        let code = event.completion_code();
        let Some(td) = self.slots[slot_index].bulk_tds[direction].front().copied() else {
            log::warn!(
                "xhci: bulk event slot {slot_id} ep {ep_id} with no descriptor (code {code})"
            );
            return;
        };
        if !Self::trb_in_td(&td, event.parameter) {
            log::error!(
                "xhci: bulk event trb {:#x} outside head TD {:#x}..{:#x} (slot {slot_id}, ep \
                 {ep_id})",
                event.parameter,
                td.first_trb,
                td.last_trb,
            );
            return;
        }
        self.slots[slot_index].bulk_tds[direction].pop_front();
        let completions = &mut self.slots[slot_index].bulk_completions[direction];
        if completions.len() >= EVENT_DRAIN_LIMIT {
            completions.pop_front();
        }
        completions.push_back(*event);
        if halts_host_endpoint(
            code,
            self.ep_state(slot_index, self.slots[slot_index].bulk_dci[direction]),
        ) {
            // `finish_td()`: the endpoint is halted, so it consumes nothing more
            // until a Reset Endpoint clears it. The caller still gets the
            // completion and turns it into the USB error its transfer expects;
            // the BOT layer then runs the specification's reset recovery.
            log::info!("xhci: bulk ep {ep_id} of slot {slot_id} halted (code {code})");
        }
    }

    /// `handle_transferless_tx_event()`: errors that arrive with no TD.
    ///
    /// Retry softly (Transfer State Preserve) a bounded number of times, then
    /// hard-reset. The counter is the reference's per-endpoint `err_count`.
    fn handle_transferless(&mut self, slot_index: usize, code: u8) {
        match code {
            regs::code::STALL
            | regs::code::TRANSACTION_ERROR
            | regs::code::SPLIT_TRANSACTION_ERROR => {
                let slot_id = self.slots[slot_index].id;
                let count = self.slots[slot_index].ep0_err_count + 1;
                self.slots[slot_index].ep0_err_count = count;
                log::warn!("xhci: transferless event slot {slot_id} code {code} (err {count})");
                self.halt_ep0(slot_index, count <= MAX_SOFT_RETRY);
            }
            _ => log::warn!(
                "xhci: transferless event slot {} ignored code {code}",
                self.slots[slot_index].id
            ),
        }
    }

    /// `xhci_handle_halted_endpoint()`: queue a Reset Endpoint, at most once.
    ///
    /// The `EP_HALTED` guard is load-bearing: a second Reset Endpoint queued
    /// before the first completes is answered with a Context State Error, and
    /// the endpoint then stays halted with its recovery silently spent.
    ///
    /// `soft` selects the reference's `EP_SOFT_RESET`, which sets `TRB_TSP` so
    /// the endpoint's transfer state is preserved.
    // ldp:step irq.halt-reset-ep
    fn halt_ep0(&mut self, slot_index: usize, soft: bool) {
        if self.slots[slot_index].ep0_halted {
            info!(
                "xhci: reset endpoint for slot {} already pending",
                self.slots[slot_index].id
            );
            return;
        }
        self.slots[slot_index].ep0_halted = true;
        let mut control = self.ep0_command_field(slot_index, regs::trb::RESET_ENDPOINT);
        if soft {
            control |= regs::trb::TSP;
        }
        self.deferred.push_back(Trb::new(0, 0, control));
    }

    /// The slot/endpoint field block of a command TRB addressed at this EP0.
    fn ep0_command_field(&self, slot_index: usize, trb_type: u32) -> u32 {
        self.endpoint_command_field(slot_index, regs::trb::EP0 as u8, trb_type)
    }

    /// The same block for any endpoint: Reset Endpoint and Set TR Dequeue are
    /// addressed by Device Context Index, not by "EP0", and a bulk endpoint
    /// needs exactly the same recovery a control endpoint gets.
    fn endpoint_command_field(&self, slot_index: usize, dci: u8, trb_type: u32) -> u32 {
        (trb_type << regs::trb::TYPE_SHIFT)
            | (u32::from(dci) << regs::trb::EP_SHIFT)
            | (u32::from(self.slots[slot_index].id) << regs::trb::SLOT_SHIFT)
    }

    /// `handle_cmd_completion()`: a completion belongs to the oldest command.
    fn handle_cmd_completion(&mut self, event: &Trb) {
        // ldp:step irq.cmd-completion
        let Some(cmd) = self.commands.front() else {
            log::error!(
                "xhci: command completion for trb {:#x} with no command outstanding",
                event.parameter,
            );
            return;
        };
        // The parameter word normally carries the address of the command TRB
        // that completed (the reference compares the same word against the
        // command ring's dequeue pointer). A mismatch means the controller
        // completed something other than the oldest outstanding command, and
        // advancing the queue on it would desynchronize every later completion.
        //
        // `Set TR Dequeue Pointer` is the documented exception: its event
        // reports the command's *parameter* (the new endpoint dequeue pointer),
        // as `xhci_handle_cmd_set_deq()` reads it back from the TRB.
        if event.parameter != cmd.trb_addr && cmd.trb_type != regs::trb::SET_TR_DEQUEUE {
            log::error!(
                "xhci: mismatched command completion event: trb {:#x}, expected {:#x}",
                event.parameter,
                cmd.trb_addr,
            );
            return;
        }
        // `list_del_init(&cmd->cmd_list)` at the top of the reference's handler:
        // the command leaves the queue when it is dispatched, not when its
        // waiter looks at it. A recovery command has no waiter at all, and
        // leaving it at the head would compare every later completion against
        // it -- the desynchronization that made every command time out.
        let mut cmd = self.commands.pop_front().expect("front command");
        cmd.done = Some(*event);
        let (trb_type, slot_id, cmd_ep) = (cmd.trb_type, cmd.slot_id, cmd.ep_id);
        info!(
            "xhci: command {trb_type} for slot {slot_id} completed, code {}",
            event.completion_code()
        );
        self.completed = Some(cmd);
        match trb_type {
            // ldp:step irq.cmd-stop-ep-done
            regs::trb::STOP_ENDPOINT => {
                if let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == slot_id) {
                    slot.stop_pending = false;
                }
                if event.completion_code() == regs::code::CONTEXT_STATE_ERROR {
                    // `xhci_handle_cmd_stop_ep()`: a Halted endpoint refuses Stop
                    // Endpoint with Context State Error, and the recovery is
                    // Reset Endpoint (then the same dequeue move).
                    info!("xhci: stop endpoint for slot {slot_id} refused (halted), resetting");
                    if let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == slot_id) {
                        if slot.halted_td.is_none() {
                            slot.halted_td = Some(Td {
                                first_trb: 0,
                                last_trb: 0,
                                what: "cancelled EP0 transfer",
                            });
                        }
                    }
                    let slot_index = self
                        .slots
                        .iter()
                        .position(|slot| slot.id == slot_id)
                        .unwrap_or(0);
                    // A placeholder descriptor (0..0) only marks "something was
                    // cancelled"; no-op'ing it is a no-op, so drop it first.
                    if let Some(td) = self.slots[slot_index].halted_td {
                        if td.first_trb == 0 && td.last_trb == 0 {
                            self.slots[slot_index].halted_td = None;
                        }
                    }
                    self.halt_ep0(slot_index, false);
                } else {
                    self.invalidate_ep0(slot_id);
                }
            }
            // ldp:step irq.cmd-set-deq-done
            regs::trb::SET_TR_DEQUEUE => {
                // Fact, not hypothesis: the endpoint state and the hardware
                // dequeue the controller holds. A Context State Error on Set TR
                // Dequeue means the endpoint is not Stopped/Error, and only the
                // context says which state it did end up in.
                self.log_ep0_state(slot_id, "set tr dequeue completion");
                // `xhci_handle_cmd_set_deq()`: the endpoint is re-pointed past
                // the halted TD. That TD was already handed back when its
                // transfer event arrived, so only the report is left.
                if let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == slot_id) {
                    slot.set_deq_pending = false;
                    for direction in 0..2 {
                        if slot.bulk_dci[direction] == cmd_ep {
                            slot.bulk_state[direction].set_deq_pending = false;
                        }
                    }
                }
                info!("xhci: set tr dequeue for slot {slot_id} ep {cmd_ep} complete");
            }
            regs::trb::RESET_ENDPOINT => {
                // "This command will only fail if the endpoint wasn't halted,
                // but we don't care" (xhci_handle_cmd_reset_ep). The halt is
                // cleared either way, and the ring is then re-pointed past the
                // TD that stalled: the reference's reset handler calls
                // xhci_invalidate_cancelled_tds(), which queues Set TR Dequeue.
                self.clear_halt(slot_id);
                if cmd_ep == regs::trb::EP0 as u8 {
                    // `xhci_invalidate_cancelled_tds()`: flush the controller's
                    // cached copy of the halted TD and move the dequeue past it.
                    self.invalidate_ep0(slot_id);
                } else if let Some(slot_index) =
                    self.slots.iter().position(|slot| slot.id == slot_id)
                {
                    for direction in 0..2 {
                        if self.slots[slot_index].bulk_dci[direction] == cmd_ep {
                            self.slots[slot_index].bulk_state[direction].halted = false;
                            self.invalidate_bulk_endpoint(slot_index, direction);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Reports one bulk endpoint's controller-side dequeue against our producer.
    ///
    /// Two states produce the same host-side symptom ("no Transfer Event, device
    /// NAKing"): the device never accepted the packet, or the controller is
    /// dequeuing from somewhere other than where we enqueued. The second one is a
    /// bookkeeping bug in this driver -- `Reset Endpoint` moves the controller's
    /// dequeue back to the ring base, and if the producer is not moved with it
    /// (`xhci_move_dequeue_past_td()`'s Set TR Dequeue) every later TRB sits behind
    /// the controller's read pointer and is never fetched. Looking at both numbers
    /// before each command separates them, which no timeout log can do afterwards.
    fn log_bulk_position(&self, slot_index: usize, direction: usize, when: &str) {
        let slot = &self.slots[slot_index];
        let dci = slot.bulk_dci[direction];
        if dci == 0 {
            return;
        }
        let Some(ring) = slot.bulk[direction].as_ref() else {
            return;
        };
        let offset = self.context_size * usize::from(dci);
        let base = slot.device.as_ptr().cast::<u8>();
        // SAFETY: one endpoint context per DCI inside this device context.
        let dequeue =
            unsafe { core::ptr::read_volatile(base.wrapping_add(offset + 8).cast::<u32>()) };
        log::info!(
            "xhci: {when} slot {} dci {dci}: controller dequeue {dequeue:#x}, ring base {:#x}, \
             producer index {} (cycle {}), tds {}",
            slot.id,
            ring.mem.bus_addr(),
            ring.index,
            ring.current_cycle(),
            slot.bulk_tds[direction].len(),
        );
    }

    /// EP0's state, as the controller recorded it (`GET_EP_CTX_STATE`).
    fn ep0_state(&self, slot_index: usize) -> u32 {
        let ep0 = self.context_size * context::EP0_CONTEXT;
        let base = self.slots[slot_index].device.as_ptr().cast::<u8>();
        // SAFETY: the output device context holds a slot context and one
        // endpoint context per DCI; `ep0` is inside this allocation.
        unsafe { core::ptr::read_volatile(base.wrapping_add(ep0).cast::<u32>()) & 0x7 }
    }

    /// One endpoint's state, as the controller recorded it.
    fn ep_state(&self, slot_index: usize, dci: u8) -> u32 {
        let offset = self.context_size * usize::from(dci);
        let base = self.slots[slot_index].device.as_ptr().cast::<u8>();
        // SAFETY: the output device context holds one endpoint context per DCI;
        // `dci` came from a configured bulk endpoint, so `offset` is inside.
        unsafe { core::ptr::read_volatile(base.wrapping_add(offset).cast::<u32>()) & 0x7 }
    }

    /// Reports EP0's state and dequeue from the slot's output device context.
    fn log_ep0_state(&self, slot_id: u8, when: &str) {
        let Some(slot) = self.slots.iter().find(|slot| slot.id == slot_id) else {
            return;
        };
        let ep0 = self.context_size * context::EP0_CONTEXT;
        let base = slot.device.as_ptr().cast::<u8>();
        // SAFETY: the output device context holds a slot context and one
        // endpoint context per DCI; `ep0` is inside this allocation, and the
        // dwords read are the endpoint context's state and dequeue fields.
        let (state, dequeue) = unsafe {
            let dword0 = core::ptr::read_volatile(base.wrapping_add(ep0).cast::<u32>());
            let dword2 = core::ptr::read_volatile(base.wrapping_add(ep0 + 8).cast::<u32>());
            let dword3 = core::ptr::read_volatile(base.wrapping_add(ep0 + 12).cast::<u32>());
            let dequeue = (u64::from(dword3) << 32) | u64::from(dword2 & !0xf);
            (dword0 & 0x7, dequeue)
        };
        info!("xhci: ep0 state {state} deq {dequeue:#x} at {when} (slot {slot_id})");
    }

    /// Clears the `EP_HALTED` state for the slot with this id.
    fn clear_halt(&mut self, slot_id: u8) {
        if let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == slot_id) {
            slot.ep0_halted = false;
        }
    }

    /// `xhci_urb_dequeue()`: stop the endpoint so the ring can be reconciled.
    ///
    /// A timed-out control transfer leaves its descriptor Active on the ring.
    /// Simply abandoning it and queueing the retry behind it puts two
    /// descriptors in flight while only one completion is expected — and the
    /// abandoned one's late event then points at a TRB that no longer belongs to
    /// the head descriptor, which is exactly how this driver lost a mouse's EP0
    /// for good. The reference cancels the URB instead (`usb_kill_urb()` →
    /// `xhci_urb_dequeue()`): Stop Endpoint, then move the dequeue past the
    /// abandoned descriptors, and only then return to the caller.
    fn cancel_ep0(&mut self, slot_index: usize) {
        if self.slots[slot_index].stop_pending {
            // `EP_STOP_CMD_PENDING`: one stop already covers this endpoint.
            return;
        }
        self.slots[slot_index].stop_pending = true;
        let control = self.ep0_command_field(slot_index, regs::trb::STOP_ENDPOINT);
        self.deferred.push_back(Trb::new(0, 0, control));
    }

    /// `xhci_invalidate_cancelled_tds()` + `xhci_move_dequeue_past_td()`.
    ///
    /// Two things have to happen for a stopped endpoint to run again, and only
    /// doing the second is not enough:
    ///
    /// 1. every cancelled descriptor's TRBs are rewritten as No-Ops, which
    ///    flushes the controller's cached copy of them;
    /// 2. `Set TR Dequeue Pointer` moves the ring past them, with the DCS bit
    ///    taken from the last rewritten TRB.
    ///
    /// (The reference walks the ring to reconcile the hardware dequeue with its
    /// own before choosing the target TRB; this ring is a single segment whose
    /// descriptors are contiguous, so "one past the last cancelled TRB" is the
    /// TRB that walk computes.)
    // ldp:step irq.cmd-reset-ep-done
    fn invalidate_ep0(&mut self, slot_id: u8) {
        let Some(slot_index) = self.slots.iter().position(|slot| slot.id == slot_id) else {
            return;
        };
        let mut last = None;
        while let Some(td) = self.slots[slot_index].tds.pop_front() {
            self.slots[slot_index]
                .ep0
                .no_op_range(td.first_trb, td.last_trb);
            last = Some(td.last_trb);
        }
        if let Some(td) = self.slots[slot_index].halted_td.take() {
            self.slots[slot_index]
                .ep0
                .no_op_range(td.first_trb, td.last_trb);
            last = Some(last.map_or(td.last_trb, |last: u64| last.max(td.last_trb)));
        }
        // Nothing was cancelled (e.g. a transferless error): the endpoint is
        // reset and the ring keeps its dequeue.
        let Some(last) = last else {
            info!("xhci: no cancelled descriptor to skip for slot {slot_id}");
            return;
        };
        let next = last + TRB_LEN as u64;
        let dcs = self.slots[slot_index].ep0.cycle_at(next);
        let control = self.ep0_command_field(slot_index, regs::trb::SET_TR_DEQUEUE);
        self.deferred
            .push_back(Trb::new(next | u64::from(dcs), 0, control));
        // Withhold the doorbell until this command completes: ringing it first
        // would restart the ring at the old dequeue (`xhci_move_dequeue_past_td`).
        self.slots[slot_index].set_deq_pending = true;
        self.slots[slot_index].doorbell_pending = true;
        info!("xhci: skipping cancelled EP0 descriptors of slot {slot_id} up to {next:#x}");
    }

    /// Waits for the next completion the dispatcher hands back for this slot.
    ///
    /// This is the only place a caller blocks: the loop drives the same
    /// in-order dispatcher every waiter uses, then takes the front completion
    /// whose turn it is. A timed-out transfer leaves its descriptor queued, and
    /// its late completion is still routed in order — the dispatcher does not
    /// care whether anyone is waiting.
    fn wait_for_completion(&mut self, slot_index: usize, what: &str) -> Result<Trb, KError> {
        let steps = if self.slots[slot_index].wire_dead {
            WIRE_DEAD_POLL_STEPS
        } else {
            EVENT_POLL_STEPS
        };
        for _ in 0..steps {
            self.pump_events();
            // The withheld doorbell (see control_transfer_slot) goes out as soon
            // as the Set TR Dequeue that blocked it has completed.
            if self.slots[slot_index].doorbell_pending && !self.slots[slot_index].set_deq_pending {
                self.slots[slot_index].doorbell_pending = false;
                self.ring_slot_doorbell(self.slots[slot_index].id);
            }
            if let Some(event) = self.slots[slot_index].ep0_completions.pop_front() {
                self.slots[slot_index].wire_dead = false;
                return Ok(event);
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_000));
        }
        self.slots[slot_index].wire_dead = true;
        self.log_event_timeout(what);
        // Leave the ring and the descriptor queue reconciled before the caller
        // retries: the reference's `usb_kill_urb()` does not return until the
        // cancelled URB has been given back.
        self.cancel_ep0(slot_index);
        for _ in 0..WIRE_DEAD_POLL_STEPS {
            self.pump_events();
            if !self.slots[slot_index].stop_pending && !self.slots[slot_index].set_deq_pending {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_000));
        }
        Err(KError::TimedOut)
    }

    /// The timeout diagnostics shared by both waiters.
    fn log_event_timeout(&mut self, what: &str) {
        // Nothing arrived: report what the event slot actually holds and where
        // the consumer thinks it is, so "controller never wrote" and "we are
        // looking at the wrong address" cannot be confused.
        let first = self.event.read(self.event_dequeue);
        log::error!(
            "xhci: event ring[dequeue] p {:#018x} s {:#010x} c {:#010x}",
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
        log::error!(
            "xhci: {what} timed out after {} ms (dequeue {} cycle {}, outstanding commands {})",
            EVENT_POLL_STEPS,
            self.event_dequeue,
            self.event_cycle,
            self.commands.len(),
        );
        // `GSTS.BUS_ERR_ADDR_VLD` (bit 4) means the core recorded a bus fault, and
        // `GBUSERRADDR` then holds the address it could not reach. That address is
        // the difference between "a device did not answer" and "the controller
        // faulted on one of our buffers", which look identical from the host side --
        // and it names which buffer, because every address this driver hands over is
        // one of a handful of allocations.
        let gsts = self.bar.read32(crate::dwc3::regs::GSTS);
        let bus_err = gsts & crate::dwc3::regs::GSTS_BUS_ERR_ADDR_VLD != 0;
        let (err_lo, err_hi) = if bus_err {
            (
                self.bar.read32(crate::dwc3::regs::GBUSERRADDR0),
                self.bar.read32(crate::dwc3::regs::GBUSERRADDR1),
            )
        } else {
            (0, 0)
        };
        // What the controller sees of the event ring: the ERST entry, its size
        // register, the dequeue register and the interrupter's mask/status. A stall
        // where EINT is set but `next_event()` sees nothing is a disagreement about
        // one of these, and each is one register read away.
        let erst = self.erst.as_ptr().cast::<u32>();
        // SAFETY: `erst` owns one `ErstEntry` = four dwords.
        let entry = unsafe {
            [
                core::ptr::read_volatile(erst),
                core::ptr::read_volatile(erst.wrapping_add(1)),
                core::ptr::read_volatile(erst.wrapping_add(2)),
                core::ptr::read_volatile(erst.wrapping_add(3)),
            ]
        };
        let ir = self.runtime + RT_IR0;
        log::error!(
            "xhci: {what} event ring state: erst entry [{:#x}, {:#x}, {:#x}, {:#x}] erstsz {:#x} \
             erdp {:#018x} iman {:#x} consumer index {} cycle {}",
            entry[0],
            entry[1],
            entry[2],
            entry[3],
            self.bar.read32(ir + 0x08),
            self.bar.read64(ir + RT_ERDP),
            self.bar.read32(ir),
            self.event_dequeue,
            self.event_cycle,
        );
        log::error!(
            "xhci: {what} bus error recorded: {bus_err} at {err_hi:#010x}{err_lo:08x}; \
             allocations: command ring {:#x}, event ring {:#x}, dcbaa {:#x}, bounce {:#x}",
            self.command.mem.bus_addr(),
            self.event.mem.bus_addr(),
            self.device_contexts.bus_addr(),
            self.bounce.bus_addr(),
        );
    }

    /// Issues a command on the command ring and waits for its completion.
    ///
    /// The command joins `commands` (the reference's `xhci->cmd_list`) before
    /// the doorbell, because completions are routed by position in that queue —
    /// not by matching an address in whatever event happens to be in the ring.
    fn command(&mut self, trb: Trb, what: &str) -> Result<Trb, KError> {
        if self.command.index >= self.command.trbs {
            // A command ring that wrapped needs a Link TRB in a production
            // driver; enumeration issues a handful of commands, so the ring is
            // simply not allowed to wrap before that is implemented.
            log::error!("xhci: command ring exhausted during {what}");
            return Err(KError::NoMemory);
        }
        let control = trb.control;
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
        self.commands.push_back(PendingCmd {
            trb_addr: published,
            trb_type: (control & regs::trb::TYPE_MASK) >> regs::trb::TYPE_SHIFT,
            slot_id: ((control & regs::trb::SLOT_MASK) >> regs::trb::SLOT_SHIFT) as u8,
            ep_id: ((control >> regs::trb::EP_SHIFT) & 0x1f) as u8,
            done: None,
        });
        self.ring_command_doorbell();
        // A command that is never fetched looks exactly like a lost notification:
        // the ring is still running (CRR set) and the TRB is on it, but the
        // controller's dequeue never moves. Ring the doorbell again before giving
        // up, and log which attempt produced the completion.
        let mut completion = None;
        'attempts: for attempt in 0..2 {
            for _ in 0..EVENT_POLL_STEPS {
                self.pump_events();
                if let Some(done) = self.completed.take() {
                    if done.trb_addr == published {
                        completion = done.done;
                        break 'attempts;
                    }
                    // A recovery command finished; it has no waiter.
                    info!(
                        "xhci: recovery command {} for slot {} done, code {}",
                        done.trb_type,
                        done.slot_id,
                        done.done.map_or(0, |event| event.completion_code()),
                    );
                }
                khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_000));
            }
            if attempt == 0 {
                warn!("xhci: {what} not fetched, ringing the doorbell again");
                self.ring_command_doorbell();
            }
        }
        let Some(event) = completion else {
            // `xhci_handle_command_timeout()`: retire the oldest command (a stale
            // head would be compared against every later completion) and abort
            // the ring. Aborting rather than re-publishing CRCR is what actually
            // restarts it -- see `abort_command_ring()`.
            self.commands.pop_front();
            if let Err(err) = self.abort_command_ring() {
                log::error!("xhci: command ring abort failed after {what}: {err:?}");
            }
            self.log_event_timeout(what);
            return Err(KError::TimedOut);
        };
        // The handler already dispatched this completion by TRB type; the
        // waiter only reports the status. The head is this command because
        // completions are only recorded on the front of the queue.
        self.commands.pop_front();
        let code = event.completion_code();
        info!(
            "xhci: {what} completion code {code} for trb {:#x} (slot {}, submitted {:#x})",
            event.parameter,
            event.slot_id(),
            published,
        );
        if code != regs::code::SUCCESS {
            log::error!("xhci: {what} failed, completion code {code}");
            // A command that *completes* with an error leaves the ring running:
            // the reference does not touch CRCR here (`xhci_setup_device()` just
            // releases the slot), and writing CRCR while CRR is set was how this
            // driver turned one failed Address Device into a controller that
            // completed no command at all afterwards (the following Disable Slot
            // timed out after 5 s).
            if code == regs::code::COMMAND_RING_STOPPED {
                self.rearm_command_ring();
            }
            return Err(KError::Io);
        }
        Ok(event)
    }

    /// Rings an endpoint doorbell (target 1 is EP0) and flushes the write.
    ///
    /// `xhci_ring_ep_doorbell()` follows its store with a read-back for the same
    /// reason the command doorbell does: a posted write that is still in flight
    /// when the controller looks for work is indistinguishable from a ring that
    /// is not being fetched at all.
    fn ring_slot_doorbell(&self, slot_id: u8) {
        self.ring_doorbell(slot_id, regs::trb::EP0 as u8);
    }

    /// Rings a slot's doorbell for one endpoint.
    ///
    /// The value written is the Device Context Index of the endpoint to notify
    /// (`2 * ep_number + direction`, EP0 = 1); the controller starts on whatever
    /// that endpoint's ring holds.
    fn ring_doorbell(&self, slot_id: u8, dci: u8) {
        let doorbell = self.doorbell + 4 * u32::from(slot_id) as usize;
        self.bar.write32(doorbell, u32::from(dci));
        let _ = self.bar.read32(doorbell);
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

    /// `xhci_abort_cmd_ring()`: stop a command ring the controller has stopped
    /// fetching, then restart it at the next TRB.
    ///
    /// xHCI 1.1 §4.6.1.2. Re-publishing CRCR alone is not a restart: while the
    /// ring is running (CRR=1) with a command the controller is stuck on, the
    /// write does not displace that command, and while the ring is stopped
    /// (CRR=0) the controller needs the abort/stop handshake before it will fetch
    /// again. The reference's order is:
    ///
    /// 1. write CRCR = next dequeue | CS | CA, which aborts the command in
    ///    progress;
    /// 2. wait for CRR to clear (5 s, then the host is treated as dead);
    /// 3. wait for the Command Ring Stopped event (2 s) -- some controllers clear
    ///    CRR without sending it;
    /// 4. restart by writing CRCR = dequeue | CS and ringing the doorbell.
    ///
    /// The caller has already retired the timed-out command from the software
    /// queue and skipped its TRB, so the ring restarts past it.
    fn abort_command_ring(&mut self) -> Result<(), KError> {
        let crcr_reg = self.operational + op::CRCR;
        let dequeue = self.command.dequeue_address();
        let cycle = u32::from(self.command.current_cycle());
        info!("xhci: aborting the command ring at {dequeue:#x}");
        self.write_crcr(dequeue, cycle | regs::crcr::CA);
        // Step 2: CRR must clear.
        let mut stopped = false;
        for _ in 0..5000 {
            if self.bar.read32(crcr_reg) & regs::crcr::CRR == 0 {
                stopped = true;
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_000));
        }
        if !stopped {
            log::error!(
                "xhci: abort did not stop the command ring (CRCR {:#x})",
                self.bar.read32(crcr_reg)
            );
            return Err(KError::TimedOut);
        }
        // Step 3: consume the Command Ring Stopped event, if the controller sends
        // one. `pump_events()` sets `ring_stopped` when it dispatches type 37.
        self.ring_stopped = false;
        for _ in 0..2000 {
            self.pump_events();
            if self.ring_stopped {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_000));
        }
        if !self.ring_stopped {
            info!("xhci: no Command Ring Stopped event; restarting anyway");
        }
        // Step 4: restart.
        self.ring_stopped = false;
        self.write_crcr(dequeue, cycle);
        self.ring_command_doorbell();
        info!("xhci: command ring restarted at {dequeue:#x}");
        Ok(())
    }

    /// Writes the 64-bit CRCR as the reference does (`lo_hi_writeq`, low dword
    /// first), with `extra` OR-ed into the control half.
    fn write_crcr(&self, dequeue: u64, extra: u32) {
        let crcr = self.operational + op::CRCR;
        let low = (dequeue as u32 & !0x1f) | extra;
        self.bar.write32(crcr, low);
        self.bar.write32(crcr + 4, (dequeue >> 32) as u32);
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
            Ok(event) => {
                // The first command is the first time the controller reads the ERST
                // to publish an event: if the entry changes anywhere, it changes
                // here.
                self.dump_erst("after first command");
                info!(
                    "xhci: no-op completed (code {}), command ring works",
                    event.completion_code()
                )
            }
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
            parent: None,
            root_port: 0,
            route: 0,
            input,
            ep0,
            bulk: [None, None],
            bulk_dci: [0, 0],
            bulk_max_packet: [0, 0],
            bulk_state: [BulkEndpointState::default(), BulkEndpointState::default()],
            bulk_tds: [
                alloc::collections::VecDeque::new(),
                alloc::collections::VecDeque::new(),
            ],
            bulk_completions: [
                alloc::collections::VecDeque::new(),
                alloc::collections::VecDeque::new(),
            ],
            interrupt: None,
            device,
            tds: alloc::collections::VecDeque::new(),
            ep0_completions: alloc::collections::VecDeque::new(),
            ep0_halted: false,
            halted_td: None,
            stop_pending: false,
            set_deq_pending: false,
            doorbell_pending: false,
            ep0_err_count: 0,
            wire_dead: false,
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
        let (err_lo, err_hi) = if gsts & crate::dwc3::regs::GSTS_BUS_ERR_ADDR_VLD != 0 {
            (
                self.bar.read32(crate::dwc3::regs::GBUSERRADDR0),
                self.bar.read32(crate::dwc3::regs::GBUSERRADDR1),
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

    /// Names the failing layer for a SuperSpeed hub child: BSR bootstrap, then
    /// a wire-free No-Op on the slot's EP0 ring, then the address-0 descriptor
    /// read. Diagnostics only — every acquired slot is released again, and the
    /// result is whatever the logs say.
    fn diagnose_super_child(
        &mut self,
        dev: &Device<'_, driver_base::PlatformBus>,
        root_port: usize,
        speed: usb::Speed,
        addressing: Addressing,
    ) {
        let slot_index = match self.slot_bootstrap(dev, root_port, speed, addressing) {
            Ok(index) => index,
            Err(err) => {
                log::warn!(
                    "xhci: super-speed child diagnostic: BSR bootstrap already fails ({err:?})"
                );
                return;
            }
        };
        let slot_id = self.slots[slot_index].id;
        info!(
            "xhci: super-speed child diagnostic: slot {slot_id} bootstrapped (BSR), probing EP0 \
             ring"
        );
        self.probe_transfer_ring(slot_index);
        let mut buf = [0u8; 8];
        let setup = usb::SetupPacket::get_descriptor(usb::descriptor::ty::DEVICE, 0, 8);
        match self.control_transfer(0, &setup, &mut buf) {
            Ok(_) => info!(
                "xhci: super-speed child diagnostic: address-0 descriptor read SUCCEEDED \
                 ({:02x?}) — the wire path through the hub is alive; the failure is \
                 SET_ADDRESS-specific",
                &buf[..2],
            ),
            Err(err) => log::warn!(
                "xhci: super-speed child diagnostic: address-0 descriptor read fails ({err:?}) — \
                 with the hub Configured, the hub is not forwarding transactions downstream"
            ),
        }
        self.address_zero_slot = None;
        let _ = self.disable_slot(slot_id);
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
        // What the controller wrote back matters as much as the completion code:
        // the slot context carries the device address it assigned and the state
        // it put EP0 in, which is the difference between "the device ignored us"
        // and "we are talking to the wrong address".
        self.log_device_context(slot_index);
        // `hub_port_init()`, right after Set Address and before the first
        // descriptor read:
        //
        // 	/* cope with hardware quirkiness:
        // 	 *  - let SET_ADDRESS settle, some device hardware wants it
        // 	 *  - read ep0 maxpacket even for high and low speed,
        // 	 */
        // 	msleep(10);
        //
        // Without it the first control transfer on the new address can come back
        // as a USB Transaction Error with the whole setup packet untransferred
        // (`residual 8`) even though Address Device succeeded.
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10_000));
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
                // device ever sees this slot's traffic at all. No explicit reset
                // is needed: the failed transfer's completion already routed a
                // Reset Endpoint through the dispatcher.
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
        write(ep0_ctx + 1, context::endpoint_dword1(4, ep0_max_packet, 0));
        write(ep0_ctx + 2, context::endpoint_dword2(ep0_ring, dcs));
        write(ep0_ctx + 3, (ep0_ring >> 32) as u32);
        write(ep0_ctx + 4, context::endpoint_tx_info(0, 8));
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
        self.slots[slot_index].root_port = port;
        self.slots[slot_index].route = addressing.route;
        log::info!(
            "xhci: slot {slot_id} addressed on port {}, speed {}, ep0 max packet {ep0_max_packet}",
            port + 1,
            speed.as_str(),
        );
        // `hub_port_init()`: "cope with hardware quirkiness: let SET_ADDRESS
        // settle" — 10 ms before the next control request, for device firmware
        // that is not ready to answer right after the status stage. BSR = 1
        // issued no SET_ADDRESS, so it needs no settle.
        if !bsr {
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10_000));
        }
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
        let setup_index = held.0;
        let setup_trb = self.slots[slot_index].ep0.mem.bus_addr() + (setup_index * TRB_LEN) as u64;
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
        let last_trb = self.slots[slot_index]
            .ep0
            .push(Trb::new(0, 0, status_control));

        // The descriptor this transfer forms, registered before the setup TRB
        // becomes visible so the dispatcher can always route the completion
        // back to it (`xhci_queue_ctrl_tx()` ends with the same bookkeeping).
        self.slots[slot_index].tds.push_back(Td {
            first_trb: setup_trb,
            last_trb,
            what: "control transfer",
        });

        // Only now, with the whole transfer in the ring, does the setup TRB
        // become visible — `giveback_first_trb()`'s ordering.
        self.slots[slot_index].ep0.publish(held);

        // Doorbell for this slot, target = EP0's device slot context index (1).
        // Withheld while a Set TR Dequeue is in flight: doorbelling now would
        // restart the ring at the dequeue the recovery is moving away from
        // (`xhci_move_dequeue_past_td` sets SET_DEQ_PENDING for this reason).
        if self.slots[slot_index].set_deq_pending {
            info!("xhci: doorbell withheld, set tr dequeue pending");
        } else {
            self.ring_slot_doorbell(slot_id);
        }
        let event = match self.wait_for_completion(slot_index, "control transfer") {
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
                // No Stop/Reset here: the descriptor stays queued, and whatever
                // the controller eventually reports about it is still processed
                // in ring order by the dispatcher, which owns endpoint
                // recovery (`finish_td()` → `xhci_handle_halted_endpoint()`).
                return Err(err);
            }
        };
        let code = event.completion_code();
        if code == regs::code::STALL {
            // The dispatcher already queued the Reset Endpoint + Set TR Dequeue
            // pair for this halt (a stalled EP0 consumes nothing more until
            // then); the stall itself propagates to the caller.
            return Err(KError::InvalidInput);
        }
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
        for _ in 0..PORT_RESET_POLL_STEPS {
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
        address: u8,
        endpoint: u8,
        data: &mut [u8],
        is_in: bool,
    ) -> KResult<usize> {
        let slot_index = self
            .slots
            .iter()
            .position(|slot| slot.id == address)
            .ok_or(KError::NotFound)?;
        self.bulk_transfer_slot(slot_index, endpoint, data, is_in)
    }

    /// `xhci_endpoint_reset()`: return a halted endpoint to Running and re-point
    /// its ring past whatever stalled.
    ///
    /// The class driver has just cleared the device's halt with
    /// `CLEAR_FEATURE(ENDPOINT_HALT)`; the *host* side needs its own reset, or the
    /// endpoint stays halted and the next transfer is never fetched. Both halves
    /// are already here for EP0 and for bulk -- this is the mapping from an
    /// endpoint address to the DCI they take.
    fn clear_endpoint_halt(&mut self, address: u8, endpoint: u8, is_out: bool) {
        let Some(slot_index) = self.slots.iter().position(|slot| slot.id == address) else {
            return;
        };
        if endpoint == 0 {
            let slot_id = self.slots[slot_index].id;
            self.clear_halt(slot_id);
            self.invalidate_ep0(slot_id);
            return;
        }
        // Bulk endpoints are found by direction, which is also how their DCI was
        // recorded when they were configured.
        let direction = usize::from(!is_out);
        if self.slots[slot_index].bulk[direction].is_none() {
            return;
        }
        self.slots[slot_index].bulk_state[direction].halted = false;
        self.recover_bulk_endpoint(slot_index, direction);
    }

    fn set_ep0_max_packet(&mut self, address: u8, max_packet: u16) {
        if let Err(err) = self.evaluate_ep0_max_packet(address, max_packet) {
            log::error!("xhci: evaluate context for address {address} failed: {err:?}");
        }
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

    /// Walks the root ports and reports the devices it brought up.
    ///
    /// The devices are returned rather than acted on here on purpose: binding a
    /// class driver and publishing a block device need the `Device` *and* a
    /// shared handle to this controller, and only the probe (which owns the
    /// `Arc`) has both. Enumeration therefore stops at "these devices exist",
    /// which is also where the reference draws the line between `hub_wq` and the
    /// class drivers.
    pub fn enumerate(
        &mut self,
        dev: &Device<'_, driver_base::PlatformBus>,
    ) -> (usize, usize, Vec<usb::UsbDevice>) {
        let mut found = 0usize;
        let mut ready = 0usize;
        let mut devices: Vec<usb::UsbDevice> = Vec::new();
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
            if self.dead_ports.contains(&port) {
                continue;
            }
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
                                devices.extend(self.scan_hub_ports(dev, &device, port));
                            } else {
                                devices.push(device);
                            }
                        }
                        Err(err) => {
                            log::error!("xhci: port {} enumeration failed: {err:?}", port + 1);
                            if matches!(err, KError::TimedOut) {
                                // `hub_port_init()` stops after its descriptor
                                // attempts; a port that never answers must not be
                                // paid for again on the next pass.
                                log::warn!(
                                    "xhci: port {} does not answer at this speed; not retrying it \
                                     this boot",
                                    port + 1
                                );
                                self.dead_ports.push(port);
                            }
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
        (found, ready, devices)
    }

    /// Walks a hub's downstream ports and enumerates what is attached.
    ///
    /// A device behind a hub is reached differently from a root-port device:
    /// the hub's port is reset through the hub class (the controller knows
    /// nothing about it), the slot carries the route string of the port path,
    /// and a full/low-speed device behind a high-speed hub also carries that
    /// hub's transaction translator — without it the device is addressed but
    /// never answers, because every transaction has to be split by the hub.
    /// Brings up everything attached to `hub` and returns the devices found,
    /// for the same reason `enumerate()` does.
    fn scan_hub_ports(
        &mut self,
        dev: &Device<'_, driver_base::PlatformBus>,
        hub: &usb::UsbDevice,
        root_port: usize,
    ) -> Vec<usb::UsbDevice> {
        let mut found_devices: Vec<usb::UsbDevice> = Vec::new();
        // The controller has to be told this slot is a hub before it will route
        // anything to the ports below it.
        if let Some(info) = usb::hub::info(self, hub.address, hub.device.protocol) {
            if let Some(slot_index) = self.slots.iter().position(|slot| slot.id == hub.address) {
                let interrupt = hub.configurations.first().and_then(|config| {
                    config.interfaces.first().and_then(|interface| {
                        interface
                            .endpoints
                            .iter()
                            .find(|endpoint| endpoint.is_in() && endpoint.transfer_type() == 3)
                    })
                });
                if let Err(err) =
                    self.configure_hub(dev, slot_index, info.ports, interrupt, hub.speed)
                {
                    log::error!("xhci: hub {} configure failed: {err:?}", hub.address);
                }
            }
        }
        // Depth 0: this walker only descends one level (root port -> hub ->
        // device), so every hub it brings up sits directly on a root port. A
        // nested hub would need its parent's depth plus one -- and
        // `HUB_SET_DEPTH` is what makes that value reach the hub at all.
        let ports = usb::hub::scan(self, hub.address, hub.device.protocol, 0);
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
                match self.address_device_direct(dev, root_port, speed, addressing) {
                    ok @ Ok(_) => ok,
                    Err(err) => {
                        // Discriminator, run once per port on failure: the
                        // parent hub is now Configured, which invalidates the
                        // old "address 0 transaction error" data point taken
                        // while the hub was still only Addressed. The BSR
                        // bootstrap plus a No-Op Transfer TRB and an address-0
                        // descriptor read split the failure into ring/slot
                        // health (No-Op completes without touching the wire)
                        // and wire health (the descriptor read is exactly the
                        // transaction the reference's new scheme would send).
                        // The result decides whether the next round targets
                        // the controller side or the hub-forwarding side.
                        self.diagnose_super_child(dev, root_port, speed, addressing);
                        Err(err)
                    }
                }
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
                //
                // Beyond that, two recovery steps the reference reaches through
                // other paths are tried in order, logged as the experiment they
                // are: (a) `hub_port_disable()` for a SuperSpeed port drives the
                // link to Rx.Detect so the device re-trains from scratch, and
                // (b) a warm (BH) reset — the full LFPS reset that puts the
                // device back into Default even when the hot reset only
                // retrained the link. A drive that reports enabled/U0 yet never
                // answers address 0 is exactly the case both target.
                if retry == 1 && is_usb3 {
                    let _ = usb::hub::redetect_link(self, hub.address, entry.port);
                }
                let reset = if retry == 2 && is_usb3 {
                    usb::hub::reset_port_warm(
                        self,
                        hub.address,
                        entry.port,
                        usb::hub::HubInfo {
                            ports: 0,
                            protocol,
                            power_on_2ms: 0,
                        },
                    )
                    .map(|speed| match speed {
                        usb::hub::Speed::Low => usb::Speed::Low,
                        usb::hub::Speed::Full => usb::Speed::Full,
                        usb::hub::Speed::High => usb::Speed::High,
                        usb::hub::Speed::Super => usb::Speed::Super,
                        usb::hub::Speed::Unknown => usb::Speed::High,
                    })
                } else {
                    self.reset_hub_port(hub, entry.port, protocol)
                };
                if reset.is_none() {
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
                    Ok(mut device) => {
                        log::info!(
                            "xhci: hub {} port {} -> {}",
                            hub.address,
                            entry.port,
                            device.summary()
                        );
                        // Remember the bus, so a device that stops answering can be
                        // reset through its own hub port and re-addressed. The bus
                        // table needs the same fact: a port scan that only looks at
                        // root ports would otherwise decide a device *behind* a hub
                        // is gone -- its port number is the hub's, not a root port's
                        // -- and withdraw a working device's block publication.
                        device.parent = Some((hub.address, entry.port));
                        if let Some(slot_index) =
                            self.slots.iter().position(|slot| slot.id == address)
                        {
                            self.slots[slot_index].parent = Some((hub.address, entry.port));
                        }
                        found_devices.push(device);
                    }
                    Err(err) => log::error!(
                        "xhci: hub {} port {} enumeration failed: {err:?}",
                        hub.address,
                        entry.port
                    ),
                },
                Err(err) => {
                    log::error!(
                        "xhci: hub {} port {} address device failed: {err:?}",
                        hub.address,
                        entry.port
                    );
                    // Did the link survive the failure? A port that fell out of
                    // U0 (or out of Enabled) explains a later Transaction Error
                    // without implicating the device.
                    if let Some((status, change)) =
                        usb::hub::port_status(self, hub.address, entry.port)
                    {
                        log::error!(
                            "xhci: hub {} port {} after failure: status {status:#06x} change \
                             {change:#04x} (connected {}, enabled {}, link state {})",
                            hub.address,
                            entry.port,
                            status & 1 != 0,
                            status & 2 != 0,
                            (status >> 5) & 0xf,
                        );
                    }
                    // `hub_port_init()`'s `fail:` label calls
                    // `hub_port_disable(hub, port1, 0)` on every failed attempt
                    // path. Leaving the port enabled keeps it claimed by a child
                    // that never reached the Addressed state: the port cannot be
                    // reset again cleanly, and the next pass starts from a
                    // half-configured link. Disabling it returns the port to a
                    // state a later scan can pick up from scratch.
                    if !usb::hub::port_disable(self, hub.address, entry.port, is_usb3) {
                        log::error!(
                            "xhci: hub {} port {} could not be disabled",
                            hub.address,
                            entry.port
                        );
                    }
                }
            }
        }
        found_devices
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
        // `hub_port_reset()`'s retry loop, not a single attempt: a port that
        // reports connected but not enabled gets `PORT_RESET_TRIES` resets, with
        // the warm (BH) escalation for a SuperSpeed port.
        let speed = usb::hub::reset_port_retries(self, hub.address, port, info)?;
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
        interrupt: Option<&usb::descriptor::EndpointDescriptor>,
        speed: usb::Speed,
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
        // merged command answered Parameter Error (17).
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
        if let Some(ep) = interrupt {
            let (interval, max_burst, max_esit) = endpoint_context_params(speed, ep);
            let context_index = (u32::from(ep.number()) * 2) + 1;
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
            // dword0: interval at bits 23:16 in 2^interval × 125 µs units
            // (`xhci_get_endpoint_interval`); Mult and the ESIT high bits are
            // 0 for a hub's status endpoint.
            write(ctx, context::endpoint_dword0(interval));
            // dword1: interrupt IN (type 7), the descriptor's max packet and
            // Max Burst Size. The endpoint number itself has no field here —
            // it lives only in `context_index`. Bits 15:8 are Max Burst Size:
            // putting the endpoint number there (as this driver once did) is
            // ignored on high speed but rejected with Parameter Error (17) on
            // SuperSpeed, where the field must match the companion
            // descriptor's bMaxBurst (`xhci_get_endpoint_max_burst`).
            write(
                ctx + 1,
                context::endpoint_dword1(7, ep.max_packet_size, max_burst as u8),
            );
            write(ctx + 2, context::endpoint_dword2(ring_addr, dcs));
            write(ctx + 3, (ring_addr >> 32) as u32);
            // dword4: ESIT payload and average TRB length, both the periodic
            // service-interval bytes for a periodic endpoint
            // (`xhci_endpoint_init`: avg_trb_len = max_esit_payload).
            write(ctx + 4, context::endpoint_tx_info(max_esit, max_esit));
            // Input control context: add SLOT and the new endpoint; entries
            // raised to cover it. Contexts between EP0 and the new endpoint
            // must exist as disabled (state 0) entries per xHCI 4.4.23.
            for gap in 2..context_index {
                let gap_ctx = dwords * (gap + 1) as usize;
                for dword in 0..5usize {
                    write(gap_ctx + dword, 0);
                }
            }
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
        let trb_addr = self.slots[slot_index].ep0.push(trb);
        // Register the descriptor so its transfer event is routed like any
        // other, instead of surfacing as a "transferless" event.
        self.slots[slot_index].tds.push_back(Td {
            first_trb: trb_addr,
            last_trb: trb_addr,
            what: "no-op transfer",
        });
        let doorbell = self.doorbell + 4 * u32::from(slot_id) as usize;
        self.bar.write32(doorbell, 1);
        // Flush the write, as `xhci_ring_ep_doorbell` does with a read-back: a
        // posted doorbell that is still in flight when the controller looks for
        // work is indistinguishable from a ring that is not being fetched.
        let _ = self.bar.read32(doorbell);
        match self.wait_for_completion(slot_index, "no-op transfer") {
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

/// How many sectors one BOT command carries. Same cap the EHCI side uses: the
/// reference's `max_sectors` default for a bulk-only device is 120, but a single
/// 8-sector command keeps the CBW/data/CSW phases inside one timeout.
const BOT_MAX_SECTORS: usize = 8;

/// The mass-storage disk as the block layer sees it.
///
/// `BlockDeviceOperations::read_block` takes `&self`, so the controller is held
/// through a shared handle; `Xhci` itself cannot be borrowed mutably from there.
struct XhciBotDisk {
    xhci: alloc::sync::Arc<ksync::Mutex<Xhci>>,
    endpoints: usb::bot::BotEndpoints,
    /// First LBA of this view. A partition's filesystem is addressed relative to
    /// its own start, so every transfer adds this back (`part_stat` in the
    /// reference: `start_sect`).
    start_lba: u64,
    blocks: u64,
}

impl subsys_block::BlockDeviceOperations for XhciBotDisk {
    fn num_blocks(&self) -> u64 {
        self.blocks
    }

    fn block_size(&self) -> usize {
        512
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> kerrno::KResult {
        if buf.len() % 512 != 0 {
            log::error!(
                "xhci: read_block {block_id} + {} bytes is not a whole number of sectors",
                buf.len()
            );
            return Err(kerrno::KError::InvalidInput);
        }
        let mut guard = self.xhci.lock();
        // Reconcile the device table with the ports before touching the device:
        // this is the polled `hub_wq`, and the bindings it removes are dropped
        // outside this lock (see `UsbBus::detach`).
        let gone = guard.rescan();
        let still_there = guard
            .bus
            .as_ref()
            .is_none_or(|bus| bus.lock().contains(self.endpoints.address));
        if !still_there {
            drop(guard);
            usb::bus::drop_bindings(gone);
            log::error!(
                "xhci: address {} is no longer present",
                self.endpoints.address
            );
            return Err(kerrno::KError::NotFound);
        }
        drop(guard);
        usb::bus::drop_bindings(gone);
        let mut guard = self.xhci.lock();
        guard.sample_ports_before_transfer();
        let mut done = 0usize;
        while done < buf.len() {
            let left = (buf.len() - done) / 512;
            let sectors = left.min(BOT_MAX_SECTORS);
            let lba = (self.start_lba + block_id + (done / 512) as u64) as u32;
            let data = usb::bot::read_sectors(&mut *guard, &self.endpoints, lba, sectors as u16)
                .or_else(|err| {
                    // A bulk-only device that lost track of a command NAKs
                    // everything; the reference's soft reset is the documented way
                    // back, and it is what makes a retry a different attempt rather
                    // than a longer wait.
                    log::error!(
                        "xhci: read_block {block_id} sector {lba} x{sectors} failed: {err:?}; \
                         running the bulk-only reset"
                    );
                    usb::bot::reset_recovery(&mut *guard, &self.endpoints)?;
                    match usb::bot::read_sectors(&mut *guard, &self.endpoints, lba, sectors as u16)
                    {
                        Ok(data) => Ok(data),
                        Err(err) => {
                            // The bulk-only reset re-synchronizes the *device's* BOT
                            // state machine. A device that stopped answering
                            // altogether is where `usb_reset_and_verify_device()`
                            // comes in: port reset, then the address/configure
                            // sequence again, with the rings re-installed. Without
                            // this escalation the first timeout poisons the disk for
                            // the rest of the boot.
                            log::error!(
                                "xhci: sector {lba} still failing after the bulk-only reset \
                                 ({err:?}); resetting the port and re-addressing the device"
                            );
                            let Some(slot_index) = self
                                .xhci
                                .lock()
                                .slots
                                .iter()
                                .position(|slot| slot.id == self.endpoints.address)
                            else {
                                return Err(err);
                            };
                            let mut guard = self.xhci.lock();
                            guard.reset_and_reverify(slot_index)?;
                            drop(guard);
                            let mut guard = self.xhci.lock();
                            usb::bot::read_sectors(
                                &mut *guard,
                                &self.endpoints,
                                lba,
                                sectors as u16,
                            )
                        }
                    }
                })?;
            if data.len() < sectors * 512 {
                log::error!(
                    "xhci: read_block {block_id} short read {} < {}",
                    data.len(),
                    sectors * 512
                );
                return Err(kerrno::KError::Io);
            }
            buf[done..done + sectors * 512].copy_from_slice(&data[..sectors * 512]);
            done += sectors * 512;
        }
        Ok(())
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> kerrno::KResult {
        if buf.len() % 512 != 0 {
            return Err(kerrno::KError::InvalidInput);
        }
        let mut guard = self.xhci.lock();
        guard.sample_ports_before_transfer();
        let mut done = 0usize;
        while done < buf.len() {
            let left = (buf.len() - done) / 512;
            let sectors = left.min(BOT_MAX_SECTORS);
            let lba = (self.start_lba + block_id + (done / 512) as u64) as u32;
            let slice = &buf[done..done + sectors * 512];
            let result =
                usb::bot::write_sectors(&mut *guard, &self.endpoints, lba, slice).or_else(|err| {
                    log::error!(
                        "xhci: write_block {block_id} sector {lba} x{sectors} failed: {err:?}; \
                         running the bulk-only reset"
                    );
                    usb::bot::reset_recovery(&mut *guard, &self.endpoints)?;
                    usb::bot::write_sectors(&mut *guard, &self.endpoints, lba, slice)
                });
            result?;
            done += sectors * 512;
        }
        Ok(())
    }

    fn flush(&self) -> kerrno::KResult {
        // Every block is acknowledged by the CSW; there is no cache to drain.
        Ok(())
    }
}

/// The two bulk pipes of a bulk-only mass-storage interface.
struct MassStoragePipe {
    endpoints: usb::bot::BotEndpoints,
    max_packet_out: u16,
    max_packet_in: u16,
    max_burst_out: u8,
    max_burst_in: u8,
}

/// Finds the bulk-only mass-storage interface of `device`, if it has one.
fn mass_storage_endpoints(device: &usb::UsbDevice) -> Option<MassStoragePipe> {
    for config in &device.configurations {
        for interface in &config.interfaces {
            if interface.class != 0x08 || interface.protocol != 0x50 {
                continue;
            }
            let mut endpoint_in = 0u8;
            let mut endpoint_out = 0u8;
            let mut max_packet_in = 0u16;
            let mut max_packet_out = 0u16;
            let mut max_burst_in = 0u8;
            let mut max_burst_out = 0u8;
            for endpoint in &interface.endpoints {
                if endpoint.is_in() {
                    endpoint_in = endpoint.address;
                    max_packet_in = endpoint.max_packet_size;
                    max_burst_in = endpoint.max_burst;
                } else {
                    endpoint_out = endpoint.address;
                    max_packet_out = endpoint.max_packet_size;
                    max_burst_out = endpoint.max_burst;
                }
            }
            if endpoint_in == 0 || endpoint_out == 0 {
                log::warn!("xhci: mass storage interface without both bulk endpoints");
                continue;
            }
            // The endpoint context's Max Packet Size has to match the
            // descriptor; a SuperSpeed bulk endpoint usually declares 1024 and a
            // mismatch is answered with Parameter Error (17).
            log::info!(
                "xhci: mass storage interface {} ep out {endpoint_out:#04x} max {max_packet_out}, \
                 ep in {endpoint_in:#04x} max {max_packet_in}",
                interface.number
            );
            return Some(MassStoragePipe {
                endpoints: usb::bot::BotEndpoints {
                    address: device.address,
                    endpoint_in,
                    endpoint_out,
                    max_packet: max_packet_in,
                    interface: interface.number,
                },
                max_packet_out,
                max_packet_in,
                max_burst_out,
                max_burst_in,
            });
        }
    }
    None
}

/// Reads the first sectors and lists the root directory: enumeration alone does
/// not prove the bulk path works, and on this board it has been the difference
/// between "the device answered descriptors" and "the device answers commands".
pub(crate) fn probe_mass_storage(xhci: &mut Xhci, device: &usb::UsbDevice) {
    let Some(pipe) = mass_storage_endpoints(device) else {
        return;
    };
    let endpoints = pipe.endpoints;
    log::info!(
        "xhci: mass storage on ep in {:#04x} out {:#04x}",
        endpoints.endpoint_in,
        endpoints.endpoint_out
    );
    // `sd_revalidate_disk()` -> `sd_spinup_disk()`: readiness first. Everything
    // below asks the medium a question, and a stick that has just been powered
    // answers `NOT READY` until its flash controller is up -- which is also the
    // state in which a READ can come back short or full of zeros with a good
    // status wrapper.
    match usb::bot::wait_until_ready(xhci, &endpoints) {
        Ok(()) => log::info!("xhci: mass storage ready"),
        Err(err) => log::warn!("xhci: mass storage did not report ready: {err:?}"),
    }
    usb::bot::describe(xhci, &endpoints);
    if !usb::bot::probe_filesystem(xhci, &endpoints) {
        // Nothing mountable on the medium, so proving the write path costs
        // nothing: it writes and restores the last sector.
        usb::bot::verify_write_path(xhci, &endpoints);
    }
    if let Err(err) = usb::bot_fs::list_root(xhci, &endpoints) {
        log::warn!("xhci: root directory listing failed: {err:?}");
    }
}

/// Configures a mass-storage device's bulk endpoints from probe.
///
/// Ring allocation needs a `Device` (`dev.res().alloc_coherent()`), which only
/// exists during probe; a runtime re-publication therefore only re-installs the
/// contexts (`reinstall_bulk_endpoints()`). This has to run before any BOT
/// command: without it `Doorbell[slot] = DCI` has nothing to start and every bulk
/// transfer reports `Unsupported`.
pub(crate) fn configure_mass_storage(
    xhci: &mut Xhci,
    dev: &mut Device<'_, driver_base::PlatformBus>,
    device: &usb::UsbDevice,
) -> KResult<()> {
    let pipe = mass_storage_endpoints(device).ok_or(KError::NotFound)?;
    xhci.configure_bulk(
        dev,
        device.address,
        pipe.endpoints.endpoint_out,
        pipe.endpoints.endpoint_in,
        pipe.max_packet_out,
        pipe.max_packet_in,
        pipe.max_burst_out,
        pipe.max_burst_in,
    )
}

/// Publishes the mass-storage interface as a block device.
///
/// This is the action the xHCI path was missing entirely: the EHCI driver had it
/// and this one stopped at "the device enumerated", so a SuperSpeed disk never
/// produced a `/dev` node no matter how healthy its transfers were.
pub(crate) fn publish_mass_storage(
    handle: &driver_base::DeviceHandle,
    xhci: &alloc::sync::Arc<ksync::Mutex<Xhci>>,
    device: &usb::UsbDevice,
) -> KResult<alloc::vec::Vec<driver_base::Publication<subsys_block::class::BlockDeviceImpl>>> {
    let pipe = mass_storage_endpoints(device).ok_or(KError::NotFound)?;
    let endpoints = pipe.endpoints;
    // The endpoints have to exist before any command can be sent over them. A
    // runtime (re)publication reuses the rings probe allocated and only re-installs
    // their contexts; the `Configure Endpoint` in probe is the one that allocates.
    let mut guard = xhci.lock();
    if guard
        .slots
        .iter()
        .position(|slot| slot.id == device.address)
        .is_some_and(|index| guard.slots[index].bulk[0].is_none())
    {
        return Err(KError::BadState);
    }
    // Ready first, then the capacity read; the reset recovery happens *inside*
    // that call and only if the command actually fails, which is the reference's
    // order (`usb_stor_invoke_transport()` resets on a transport error, not
    // before every command).
    let total_blocks = match usb::bot::read_capacity_ready(&mut *guard, &endpoints) {
        Ok(data) if data.len() >= 8 => {
            let last = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            u64::from(last) + 1
        }
        _ => {
            log::error!("xhci: mass storage reported no capacity");
            return Err(KError::Io);
        }
    };
    // `sd`'s view of the same question: a partitioned stick exposes `…pN`, and a
    // filesystem is read relative to the partition's own start LBA.
    let views = usb::bot::plan_views(&mut *guard, &endpoints, total_blocks);
    drop(guard);
    let parent_path = alloc::string::String::from(handle.path());
    let modalias = handle.modalias().map(alloc::string::String::from);
    // Major 8 is the conventional disk major and the block core rejects a duplicate
    // dev_t, so every view takes its own minor from one counter shared with EHCI.
    static NEXT_MINOR: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);
    let mut publications = alloc::vec::Vec::new();
    for view in &views {
        let backend = XhciBotDisk {
            xhci: xhci.clone(),
            endpoints,
            start_lba: view.start_lba,
            blocks: view.blocks,
        };
        let name = if view.part == 0 {
            alloc::format!("usb{}", device.address)
        } else {
            alloc::format!("usb{}p{}", device.address, view.part)
        };
        let minor = NEXT_MINOR.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let disk = subsys_block::Gendisk::new(name, 8, minor, 1, alloc::boxed::Box::new(backend))
            .map(|disk| disk.with_devnode_context(&parent_path, modalias.clone()))
            .map_err(|err| {
                log::error!("xhci: could not create the block disk: {err:?}");
                KError::Io
            })?;
        let publication = handle
            .publish(
                &subsys_block::class::BLOCK,
                alloc::sync::Arc::new(disk),
                "usb-storage",
            )
            .map_err(|err| {
                log::error!("xhci: block publication failed: {err:?}");
                KError::Io
            })?;
        log::info!(
            "xhci: published {} as {} with {} 512-byte blocks (start LBA {})",
            device.summary(),
            if view.part == 0 {
                alloc::format!("usb{}", device.address)
            } else {
                alloc::format!("usb{}p{}", device.address, view.part)
            },
            view.blocks,
            view.start_lba
        );
        publications.push(publication);
    }
    let callback: devnode::DevNodeCallback = alloc::sync::Arc::new(|_| {});
    for node in devnode::subscribe(callback) {
        log::info!(
            "xhci: devnode {} subsystem {} number {:?}",
            node.devname(),
            node.subsystem(),
            node.device_number()
        );
    }
    Ok(publications)
}
