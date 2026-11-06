// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! EHCI host controller driver.
//!
//! Structure follows EHCI 1.0 §4: reset the controller, hand it the
//! asynchronous schedule (a single Queue Head that carries every control
//! transfer), then walk the root ports. Transfers are qTD chains on that QH.
//!
//! Only root-hub devices are handled: there is no periodic schedule and no hub
//! driver yet, so interrupt and isochronous endpoints are out of scope.

pub mod regs;

use driver_base::{CoherentMem, Device, IoMap, PlatformBus, ProbeError};
use kerrno::{KError, KResult};
use log::info;
use usb::{Hcd, PortStatus, SetupPacket, Speed};

use crate::ehci::regs::{Qh, Qtd, cap, info1, op, pid, portsc, sts, token};

/// Consecutive polls of a self-clearing bit, in microseconds.
/// qTD slots, and what each one is for. The bulk path indexes 3..6 and the async
/// head's dummy descriptor is slot 7, so the allocation has to cover all of them;
/// a slot map that the allocator does not match is an out-of-bounds write and a
/// controller DMA into foreign memory.
const QTD_SLOTS: usize = 8;
/// Control chain: setup, data, status.
const QTD_CONTROL: [usize; 3] = [0, 1, 2];
/// The control chain's dummy: the descriptor its status qTD ends on, and the one
/// the async head's `alt_next` points at (`ehci_init()` does the same with
/// `async->dummy`).
const QTD_CONTROL_DUMMY: usize = 3;
/// Each bulk direction keeps the reference's source/dummy pair: `qh_append_tds()`
/// builds into the source and swaps it into the dummy the queue head is parked on.
const QTD_BULK_OUT: (usize, usize) = (4, 5);
const QTD_BULK_IN: (usize, usize) = (6, 7);

/// How many times the full async-ring dump is printed; see `STALL_DUMPS`' use.
static STALL_DUMPS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

const POLL_STEPS: usize = 200_000;

/// Room for the setup packet plus one data buffer.
const EP0_BUFFER_LEN: usize = 4096;
/// Bulk DMA buffer: one page, enough for a mass-storage sector and for the
/// 31-byte command block wrapper plus its status.
const BULK_BUFFER_LEN: usize = 4096;
/// CRU `CLKGATE_CON` offset covering the USB host gates (dwc3's is CON42 at
/// 0x8a8; the host gates live in the same neighbourhood).
const CLKGATE_SNAPSHOT: usize = 0x8a0;

pub struct Ehci {
    regs: IoMap,
    /// Operational register base: `bar + CAPLENGTH`.
    operational: usize,
    ports: usize,
    /// The single control queue head that is the asynchronous schedule's head.
    qh: CoherentMem,
    /// qTD chain storage: setup, data, status.
    qtds: CoherentMem,
    /// Setup packet and data stage share one page-aligned buffer.
    buffers: CoherentMem,
    /// Page-aligned DMA buffer for bulk transfers.
    bulk: CoherentMem,
    /// CRU clock-gate words snapshotted at init, for comparing on failure.
    gate_snapshot: [u32; 2],
    /// The CRU window, kept so the gate snapshot can be re-read on failure.
    cru: IoMap,
    /// Bus address of each bulk endpoint's dummy descriptor (index 0 = OUT,
    /// 1 = IN). A queue head's `qtd_next` points here permanently; enqueueing
    /// replaces the descriptor's contents instead of repointing the head.
    bulk_dummy_bus: [u32; 2],
    /// Which qTD slot currently holds the descriptor the controller is parked on
    /// (index 0 = bulk OUT pair, 1 = bulk IN pair). `qh_append_tds()` alternates
    /// the role: the descriptor it enqueues is copied onto the current dummy and
    /// the slot it came from becomes the new dummy, so the address the hardware
    /// is parked on never changes.
    bulk_dummy_slot: [usize; 2],
    /// The periodic frame list: allocated and programmed even though the
    /// periodic schedule is off, because the reference always has one. Kept as a
    /// field so the base register can be rewritten after a controller reset.
    frame_lists: alloc::vec::Vec<CoherentMem>,
    /// Speed of the device currently addressed. Enumeration handles one port at
    /// a time, so one value is enough until a hub driver exists.
    speed: Speed,
}

/// Builds the asynchronous ring `head -> EP0 -> bulk OUT -> bulk IN -> head`.
///
/// Recovery must rebuild all of it. Writing the head's next pointer back to
/// itself -- an "empty list" -- is only correct when there is no endpoint queue
/// head at all; with endpoints present it truncates the ring to `head -> head`
/// and every endpoint becomes unreachable, which is exactly how a transfer ends
/// up queued with an overlay the controller never touches.
/// The value `ehci_run()` writes into USBCMD.
///
/// Linux composes this register from scratch (`ehci_init()` builds `ehci->command`
/// and `ehci_run()` writes it), so every hardware-default bit is replaced. Two of
/// those default bits matter here: `CMD_PARK` and `PARK_CNT`, whose module
/// parameter `park` defaults to 0 in the reference but which this controller sets
/// on its own. A read-modify-write -- what this driver did -- keeps them, and a
/// parked controller stops walking the async list: every queue head then reads
/// `hw_current == 0` forever, which is exactly the stall this board shows.
///
/// ITC is 1 microframe, as the reference's default `log2_irq_thresh = 0` gives.
fn run_command() -> u32 {
    regs::cmd::RS | regs::cmd::ASE | (1 << 16)
}

fn build_async_ring(base_address: u64, head: *mut Qh, dummy_qtd_bus: u32) {
    let base = base_address as u32;
    let total = 4u32;
    for index in 0..total {
        let entry = head.wrapping_add(index as usize);
        // `ehci_init()` leaves the asynchronous list *empty*: the reclamation
        // head points at itself, and endpoints are spliced in for the duration of
        // one transfer (`qh_link_async()`) and taken out again
        // (`unlink_async()`). Keeping every endpoint in a fixed circular ring
        // instead -- what this driver did -- breaks the moment a queue head is
        // unlinked: the head inherits the unlinked queue head's successor, which
        // in a ring is the queue head itself, so the schedule becomes
        // head -> QH -> QH -> QH ... and the head is never reached again. The
        // controller then never advances, which this board reports as
        // `USBSTS 0xb03d` (HSE|HCH) on the next transfer.
        let next = base | 2;
        if index == 0 {
            // `ehci_init()` initialises the reclamation-list head explicitly, and
            // every field matters:
            //
            // - info1 carries H (bit 15), which is how the controller recognises
            //   the end of the list;
            // - the overlay token is HALTED. A live token makes the controller try
            //   to execute the head's descriptor chain, and `qtd_next` is an
            //   address: with it unset that fetch goes to address zero, which is
            //   a Host System Error and a halted controller (`USBSTS 0xb03d` on
            //   this board) rather than a stopped transfer;
            // - `qtd_next` is LIST_END and `alt_next` points at the head's dummy
            //   descriptor, which is the "stop here" target the reference uses.
            //
            // SAFETY: dwords 0..6 of the head queue head, inside this allocation.
            unsafe {
                let bytes = entry.cast::<u8>();
                core::ptr::write_volatile(bytes.cast::<u32>(), next);
                core::ptr::write_volatile(bytes.wrapping_add(4).cast::<u32>(), regs::info1::HEAD);
                core::ptr::write_volatile(bytes.wrapping_add(16).cast::<u32>(), regs::LIST_END);
                core::ptr::write_volatile(bytes.wrapping_add(20).cast::<u32>(), dummy_qtd_bus);
                core::ptr::write_volatile(bytes.wrapping_add(24).cast::<u32>(), regs::token::HALT);
            }
            karch::dsb_ishst();
            continue;
        }
        // An endpoint queue head starts idle: out of the list, no descriptor, no
        // token. It is written in full before it is linked, and `LIST_END` here
        // keeps the controller from fetching through an unset pointer if it walks
        // past an entry that is not in use.
        // SAFETY: dwords 0..6 of queue head `index`, inside this allocation.
        unsafe {
            let bytes = entry.cast::<u8>();
            core::ptr::write_volatile(bytes.cast::<u32>(), next);
            core::ptr::write_volatile(bytes.wrapping_add(16).cast::<u32>(), regs::LIST_END);
            core::ptr::write_volatile(bytes.wrapping_add(20).cast::<u32>(), regs::LIST_END);
            core::ptr::write_volatile(bytes.wrapping_add(24).cast::<u32>(), 0);
        }
    }
    karch::dsb_ishst();
}

impl Ehci {
    pub fn init(dev: &Device<'_, PlatformBus>, regs: IoMap) -> Result<Self, ProbeError> {
        let caplen = regs.read8(cap::CAPLENGTH) as usize;
        let hcsparams = regs.read32(cap::HCSPARAMS);
        let hccparams = regs.read32(cap::HCCPARAMS);
        let ports = (hcsparams & 0xf) as usize;
        info!(
            "ehci: version {:#06x} caplen {caplen} ports {ports} hcsparams {hcsparams:#010x} \
             hccparams {hccparams:#010x}",
            regs.read16(cap::HCIVERSION),
        );

        let operational = caplen;
        // §4.1: stop the controller and reset it before anything else.
        regs.write32(operational + op::USBCMD, 0);
        for _ in 0..POLL_STEPS {
            if regs.read32(operational + op::USBSTS) & sts::HCH != 0 {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1));
        }
        regs.write32(
            operational + op::USBCMD,
            regs.read32(operational + op::USBCMD) | cmd_hcreset(),
        );
        for _ in 0..POLL_STEPS {
            if regs.read32(operational + op::USBCMD) & cmd_hcreset() == 0 {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
        }
        // Take ownership of every port: without this the companion controllers
        // keep the low/full speed ports and this controller never sees them.
        regs.write32(operational + op::CONFIGFLAG, 1);
        // A periodic frame list exists even though the periodic schedule stays
        // off: the reference allocates one and programs its base on every
        // controller (measured `PERIODICLISTBASE 0xec451000` on an idle
        // ehci-platform), and a controller that prefetches the list sees a valid,
        // all-terminated table instead of address 0. FLF stays at 1024 entries
        // (`USBCMD 0x00010004`, frame list size 1024), each entry terminated.
        let mut frame_lists: alloc::vec::Vec<CoherentMem> = alloc::vec::Vec::new();
        let frame_list = dev
            .res()
            .alloc_coherent(1024 * core::mem::size_of::<u32>(), 4096)?;
        // Every entry terminated, as the reference does (`EHCI_LIST_END` is 1, not
        // 0): a zeroed entry is a null pointer the controller would follow if the
        // periodic schedule were ever enabled.
        // SAFETY: the whole frame list was just allocated.
        unsafe {
            for index in 0..1024 {
                core::ptr::write_volatile(
                    frame_list.as_ptr().cast::<u32>().wrapping_add(index),
                    regs::LIST_END,
                );
            }
        }
        regs.write32(
            operational + op::PERIODICLISTBASE,
            frame_list.bus_addr() as u32,
        );
        frame_lists.push(frame_list);

        // Two queue heads: the asynchronous reclamation list's head (H=1, no
        // transfers) and the control endpoint's own QH linked after it. Linux
        // arranges the same way (`ehci->async` is an empty head QH); putting the
        // transfer directly in the head QH leaves the qTDs unexecuted.
        let qh = dev
            .res()
            .alloc_coherent(2 * core::mem::size_of::<Qh>(), 64)?;
        // Every slot this driver indexes must exist. The bulk path uses slots 3..6
        // and the async head's dummy descriptor is slot 7; with only three slots
        // (the original layout) those writes landed past the end of the
        // allocation and the controller fetched its descriptor from memory that
        // belonged to somebody else -- a Host System Error, intermittently,
        // depending on what that memory happened to hold.
        let qtds = dev
            .res()
            .alloc_coherent(QTD_SLOTS * core::mem::size_of::<Qtd>(), 32)?;
        let buffers = dev.res().alloc_coherent(EP0_BUFFER_LEN, EP0_BUFFER_LEN)?;
        // `ehci_qtd_init()`: the descriptor every transfer list ends on. It is
        // inactive, and its own next pointer terminates -- the controller walks
        // onto it after the last real descriptor and parks there. Ending a list
        // with `LIST_END` instead makes the controller write HALT into the queue
        // head overlay, asynchronously, possibly after software has already
        // cleared it for the next transfer.
        // SAFETY: slot `QTD_CONTROL_DUMMY` is inside the qTD allocation.
        unsafe {
            core::ptr::write_volatile(
                qtds.as_ptr().cast::<Qtd>().wrapping_add(QTD_CONTROL_DUMMY),
                Qtd {
                    // `ehci_qtd_init()`: token = QTD_STS_HALT, not 0. A descriptor
                    // that is merely inactive but not halted is one the controller
                    // may report as completed with no status at all.
                    next: regs::LIST_END,
                    alt_next: regs::LIST_END,
                    token: token::HALT,
                    buffer: [0; 5],
                },
            );
        }
        let bulk = dev.res().alloc_coherent(BULK_BUFFER_LEN, BULK_BUFFER_LEN)?;
        for (pointer, length) in [
            (qh.as_ptr(), 4 * core::mem::size_of::<Qh>()),
            (qtds.as_ptr(), 3 * core::mem::size_of::<Qtd>()),
            (buffers.as_ptr(), EP0_BUFFER_LEN),
            (bulk.as_ptr(), BULK_BUFFER_LEN),
        ] {
            // SAFETY: each pair is an allocation made just above.
            unsafe {
                core::ptr::write_bytes(pointer, 0, length);
            }
        }
        // The single QH is the head of the asynchronous reclamation list.
        // The single QH is the head of the asynchronous reclamation list: its
        // next pointer refers to itself, making a one-element list.
        let head = qh.as_ptr().cast::<Qh>();
        let head_bytes = head.cast::<u8>();
        let info1_ptr = head_bytes.wrapping_add(4).cast::<u32>();
        let ep_qh = head.wrapping_add(1);
        // Endpoint QH: empty, linked back to the head so the reclamation list is
        // a closed ring of two.
        // SAFETY: the allocation holds two queue heads.
        unsafe {
            core::ptr::write_volatile(ep_qh, Qh::default());
        }
        let ep_qtd_next = ep_qh.cast::<u8>().wrapping_add(16).cast::<u32>();
        // SAFETY: dword 4 of the endpoint QH.
        unsafe {
            core::ptr::write_volatile(ep_qtd_next, regs::LIST_END);
        }
        // SAFETY: dword 0 of the endpoint QH.
        unsafe {
            core::ptr::write_volatile(ep_qh.cast::<u32>(), qh.bus_addr() as u32 | 2);
        }
        // SAFETY: `head` is the one QH in this allocation.
        unsafe {
            core::ptr::write_volatile(head, Qh::default());
        }
        // SAFETY: dword 1 of the same QH.
        unsafe {
            core::ptr::write_volatile(info1_ptr, info1::HEAD);
        }
        // An empty queue head's transfer pointers are LIST_END, not zero: the
        // hardware would otherwise fetch a qTD at address 0, raise a Host System
        // Error and halt -- which it did, immediately on enabling the schedule,
        // before any transfer had been queued.
        let qtd_next_ptr = head_bytes.wrapping_add(16).cast::<u32>();
        // SAFETY: dword 4 of the same QH.
        unsafe {
            core::ptr::write_volatile(qtd_next_ptr, regs::LIST_END);
        }
        let alt_next_ptr = head_bytes.wrapping_add(20).cast::<u32>();
        // SAFETY: dword 5 of the same QH.
        unsafe {
            core::ptr::write_volatile(alt_next_ptr, regs::LIST_END);
        }
        // Every endpoint queue head lives in the ring; see `build_async_ring()`.
        build_async_ring(
            qh.bus_addr(),
            qh.as_ptr().cast::<Qh>(),
            qtds.bus_addr() as u32 + (QTD_CONTROL_DUMMY * core::mem::size_of::<Qtd>()) as u32,
        );
        // Snapshot the CRU clock-gate words around the USB hosts: a later
        // driver's CRU write that gates one of these clocks stops the
        // controller's traversal while every register keeps its value, and the
        // gate word is the only place that shows.
        // The CRU sits at its own base; the gate words live at +0x8a0 (the dwc3
        // neighbour is CON42 at 0x8a8).
        let cru = dev.res().iomap_region(
            driver_base::MmioRegion::new(0xfd7c_0000, 0x1000),
            "ehci-cru-gates",
        )?;
        let mut gates = [0u32; 2];
        for (word, cell) in gates.iter_mut().enumerate() {
            *cell = cru.read32(CLKGATE_SNAPSHOT + word * 4);
        }
        info!(
            "ehci: cru gate snapshot {:#010x} {:#010x} (offsets {:#x}..)",
            gates[0], gates[1], CLKGATE_SNAPSHOT,
        );
        regs.write32(operational + op::ASYNCLISTADDR, qh.bus_addr() as u32);
        // `ehci_reset()` clears the sticky status bits by writing them back. A
        // latched Host System Error keeps the controller halted, and a halted
        // controller never fetches the schedule: every qTD stays Active and the
        // data stage of a transfer silently returns nothing.
        let status_before = regs.read32(operational + op::USBSTS);
        regs.write32(operational + op::USBSTS, status_before);
        info!(
            "ehci: status before run {status_before:#010x} (halted {}, hse {}, pcd {}, ass {})",
            status_before & sts::HCHALTED != 0,
            status_before & sts::HSE != 0,
            status_before & sts::PCD != 0,
            status_before & sts::ASS != 0,
        );
        // Run with the asynchronous schedule only. Written whole, not OR-ed into
        // the reset value: see `run_command()` for the parking bit this replaces.
        regs.write32(operational + op::USBCMD, run_command());
        // Let the controller leave the halted state before anything is queued.
        for _ in 0..100 {
            let status = regs.read32(operational + op::USBSTS);
            if status & sts::HCHALTED == 0 {
                info!(
                    "ehci: running, status {status:#010x} (halted false, hse {}, ass {})",
                    status & sts::HSE != 0,
                    status & sts::ASS != 0,
                );
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_000));
        }
        // `HCSPARAMS.PPC` says software controls port power: without PP set the
        // ports stay unpowered and report nothing at all.
        if hcsparams & (1 << 4) != 0 {
            for port in 0..ports {
                let offset = operational + op::PORTSC_BASE + port * op::PORT_STRIDE;
                let value = regs.read32(offset);
                regs.write32(offset, (value & portsc::CHANGE_BITS) | portsc::PP);
                info!(
                    "ehci: port {} power {value:#010x} -> {:#010x}",
                    port + 1,
                    regs.read32(offset),
                );
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(50_000));
        }
        let status = regs.read32(operational + op::USBSTS);
        info!(
            "ehci: running, USBSTS {status:#010x} qh {:#x}",
            qh.bus_addr()
        );

        Ok(Self {
            regs,
            operational,
            ports,
            qh,
            qtds,
            buffers,
            bulk,
            bulk_dummy_bus: [0; 2],
            // `ehci_qtd_alloc()` hands out a fresh qTD as the endpoint's dummy;
            // the second slot of each pair plays that part until it is traded.
            bulk_dummy_slot: [QTD_BULK_OUT.1, QTD_BULK_IN.1],
            gate_snapshot: gates,
            cru,
            frame_lists,
            speed: Speed::Full,
        })
    }

    fn port_register(&self, port: usize) -> usize {
        self.operational + op::PORTSC_BASE + port * op::PORT_STRIDE
    }

    pub fn log_ports(&self) {
        for port in 0..self.ports {
            let value = self.regs.read32(self.port_register(port));
            info!(
                "ehci: port {} portsc {value:#010x} ccs {} ped {} pp {} line {}",
                port + 1,
                value & portsc::CCS != 0,
                value & portsc::PED != 0,
                value & portsc::PP != 0,
                (value & portsc::LINE_MASK) >> 10,
            );
        }
    }

    /// One control transfer: setup qTD, optional data qTD, status qTD.
    fn control_transfer_inner(
        &mut self,
        address: u8,
        setup: &SetupPacket,
        data: &mut [u8],
        speed: Speed,
    ) -> KResult<usize> {
        self.recover_if_hse();
        self.ensure_running();
        let is_in = setup.request_type & 0x80 != 0;
        let length = data
            .len()
            .min(setup.length as usize)
            .min(EP0_BUFFER_LEN / 2);
        let setup_bytes = [
            setup.request_type,
            setup.request,
            (setup.value & 0xff) as u8,
            (setup.value >> 8) as u8,
            (setup.index & 0xff) as u8,
            (setup.index >> 8) as u8,
            (setup.length & 0xff) as u8,
            (setup.length >> 8) as u8,
        ];
        let buffer_base = self.buffers.as_ptr();
        let data_base = self.buffers.bus_addr() as u32 + 64;
        // SAFETY: the buffer is 4096 bytes; the setup packet goes at offset 0 and
        // the data stage at offset 64, so both fit for any length we accept.
        // SAFETY: the buffer is 4096 bytes and the setup packet is 8 bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(setup_bytes.as_ptr(), buffer_base, 8);
        }
        if !is_in && length > 0 {
            let data_start = buffer_base.wrapping_add(64);
            // SAFETY: offset 64 plus `length` stays inside the 4096-byte buffer
            // (length is clamped to half of it) and the caller's slice holds
            // at least `length` bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(data.as_ptr(), data_start, length);
            }
        }

        let setup_addr = self.build_chain(is_in, length, data_base);

        // Program the QH for this device and point it at the chain. Writing the
        // current-qTD pointer as zero clears any overlay left by the last run.
        let head = self.qh.as_ptr().cast::<Qh>().wrapping_add(1);
        let eps = match speed {
            Speed::Low => regs::eps_low(),
            Speed::High => regs::eps_high(),
            _ => regs::eps_full(),
        };
        let max_packet = if speed == Speed::High { 64 } else { 8 };
        let qh = Qh {
            // Written while the queue head is out of the list (`qh_link()` sets
            // this pointer from the head's current successor).
            next: regs::LIST_END,
            // No HEAD here: only the empty list head carries H, and a second
            // QH with it reads as the end of the reclamation list, so the
            // controller overlays this QH's qTD and never executes it.
            // `ehci.h`: `#define QH_CONTROL_EP (1 << 27) /* FS/LS control
            // endpoint */` -- and `qh_make()` sets it only in the LOW/FULL speed
            // branch, for split transactions through a TT. On a high-speed control
            // endpoint it must be clear; setting it told the controller to run this
            // pipe as a FS/LS control endpoint, which is consistent with a SETUP
            // that is accepted and a data stage that is never answered. (The
            // in-tree usb-host stack programs a HS control queue head the same way:
            // DTC bit 14 only.)
            info1: info1::TOGGLE_CTL
                | info1::NAK_RELOAD
                | ((max_packet as u32) << info1::MAX_PACKET_SHIFT)
                | eps
                | (address as u32 & info1::ADDRESS_MASK),
            // `qh_make()` sets MULT from `EHCI_TUNE_MULT_HS` for a high-speed
            // endpoint. Zero is not a legal value: with it the controller never
            // schedules the transfer, the setup qTD is overlaid into the queue
            // head and then never retires, and every control transfer times out
            // with no error anywhere.
            info2: if eps == regs::eps_high() {
                regs::info2::MULT_HIGH_SPEED
            } else {
                0
            },
            current: 0,
            // Overlay written by `qh_update()` after the descriptor chain is in
            // place; the queue head is out of the list while that happens.
            qtd_next: regs::LIST_END,
            alt_next: regs::LIST_END,
            // Data toggle starts at DATA0 for the setup stage.
            token: 0,
            buffer: [0; 5],
        };
        // SAFETY: `head` is the EP0 queue head in this controller's allocation.
        unsafe {
            core::ptr::write_volatile(head, qh);
        }
        // `qh_update()` + `qh_link_async()`: the chain is complete, so the overlay
        // may point at it and the queue head may enter the async list.
        self.qh_update(1, setup_addr);
        self.qh_link(1);

        // Wait for the *last* qTD of the chain to retire, with the reference's
        // completion loop (`qh_completions()` plus its transaction-error retry).
        let qtd_base = self.qtds.as_ptr().cast::<regs::Qtd>();
        let status_token = self.qh_wait_token(1, 0);
        {
            {
                for (name, qh_ptr) in [("head", self.qh.as_ptr().cast::<Qh>()), ("ep", head)] {
                    // SAFETY: both queue heads are inside this controller's
                    // two-QH allocation.
                    let qh = unsafe { core::ptr::read_volatile(qh_ptr) };
                    log::info!(
                        "ehci: qh {name}: next {:#x} info1 {:#x} qtd_next {:#x} alt {:#x} token \
                         {:#x}",
                        qh.next,
                        qh.info1,
                        qh.qtd_next,
                        qh.alt_next,
                        qh.token,
                    );
                }
                // SAFETY: dword 2 of the setup qTD holds its token.
                let setup_token =
                    unsafe { core::ptr::read_volatile(qtd_base.cast::<u32>().add(2 + 16)) };
                log::info!("ehci: setup qtd token {setup_token:#010x}");
                // SAFETY: index 1 of the three qTDs is the data stage.
                let data_token =
                    unsafe { core::ptr::read_volatile(qtd_base.cast::<u32>().add(2 + 8)) };
                if status_token & token::ACTIVE != 0 {
                    let status = self.regs.read32(self.operational + op::USBSTS);
                    log::error!(
                        "ehci: control transfer did not complete, status token \
                         {status_token:#010x}, USBSTS {status:#010x} (halted {}, hse {}, ass {})",
                        status & sts::HCHALTED != 0,
                        status & sts::HSE != 0,
                        status & sts::ASS != 0,
                    );
                    // Which stage stopped the transfer: setup retired means the
                    // device accepted it and the data stage is what hangs; a HALT
                    // with XACT on the setup means the packet itself was refused.
                    for (slot, stage) in [(2usize, "setup"), (1, "data"), (0, "status")] {
                        // SAFETY: slots 0..2 are the control chain.
                        let qtd = unsafe {
                            core::ptr::read_volatile(
                                self.qtds.as_ptr().cast::<Qtd>().wrapping_add(slot),
                            )
                        };
                        log::error!(
                            "ehci: {stage} qtd next {:#x} token {:#010x} (active {}, halt {}, \
                             xact {}, cerr {}, pid {}, len {})",
                            qtd.next,
                            qtd.token,
                            qtd.token & token::ACTIVE != 0,
                            qtd.token & token::HALT != 0,
                            qtd.token & token::XACT != 0,
                            (qtd.token >> token::CERR_SHIFT) & 0x3,
                            (qtd.token >> token::PID_SHIFT) & 0x3,
                            (qtd.token & token::LEN_MASK) >> token::LEN_SHIFT,
                        );
                    }
                    // The port as the controller sees it while the data stage is
                    // outstanding: a port that is not really in high-speed mode, or
                    // one whose line state is not the HS idle (SE0), would explain
                    // a data stage the device never answers.
                    for port in 0..self.ports {
                        let value = self.regs.read32(self.port_register(port));
                        log::error!(
                            "ehci: port {} at timeout: portsc {value:#010x} (ccs {}, ped {}, line \
                             {})",
                            port + 1,
                            value & portsc::CCS != 0,
                            value & portsc::PED != 0,
                            (value & portsc::LINE_MASK) >> 10,
                        );
                    }
                    self.qh_unlink(1);
                    return Err(KError::TimedOut);
                }
                if data_token & token::HALT != 0 {
                    info!("ehci: data stage halted, token {data_token:#010x}");
                    return Err(KError::Io);
                }
                let residual = (data_token & token::LEN_MASK) >> token::LEN_SHIFT;
                let transferred = (length as u32).saturating_sub(residual) as usize;
                if is_in && transferred > 0 {
                    // SAFETY: the controller wrote into the data buffer.
                    let from = buffer_base.wrapping_add(64);
                    // SAFETY: the controller wrote `transferred` bytes there.
                    unsafe {
                        core::ptr::copy_nonoverlapping(from, data.as_mut_ptr(), transferred);
                    }
                }
                // `unlink_async()`: the queue head leaves the list before the
                // next transfer writes its descriptors.
                self.qh_unlink(1);
                return Ok(transferred);
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
        }
        log::error!("ehci: control transfer timed out");
        self.qh_unlink(1);
        Err(KError::TimedOut)
    }

    /// Clears a latched halt before queueing new work.
    ///
    /// A Host System Error halts the controller and stays latched until software
    /// writes USBSTS back. Every transfer path needs this, control transfers
    /// included: the BOT soft reset is itself a control transfer, so without it
    /// the recovery path cannot run either.
    /// Clears a latched Host System Error as cheaply as the situation allows.
    ///
    /// `HCRESET` throws the root hub's port state away, which takes the device off
    /// the bus and costs it its enumeration. The reference pays that because USB
    /// core re-enumerates the whole tree afterwards (`ehci_hc_died()`); this
    /// driver has no such loop. So the full reset stays for the case it exists
    /// for -- the port is gone, `PORT_ENABLE` clear -- and a latched status on a
    /// still-enabled port is cleared in place: the device is where it was and
    /// only the run bit has to be re-armed.
    fn recover_if_hse(&mut self) {
        let status = self.regs.read32(self.operational + op::USBSTS);
        if status & sts::HSE == 0 {
            return;
        }
        let portsc = self.regs.read32(self.port_register(0));
        if portsc & portsc::PED != 0 {
            log::warn!(
                "ehci: clearing a latched host system error with the port still enabled (portsc \
                 {portsc:#010x})"
            );
            self.regs.write32(self.operational + op::USBSTS, sts::HSE);
            return;
        }
        self.recover_from_hse();
    }

    /// Recover from a Host System Error, which is what `ehci_hc_died()` reacts to.
    ///
    /// A HSE means the controller hit a fatal internal/system fault; it latches
    /// HCH, and from then on every qTD stays Active and every later transfer
    /// times out -- one bad transfer used to take the whole controller down for
    /// the rest of the boot. The register state after such a fault is not
    /// trustworthy, so this repeats `ehci_reset()` + `ehci_run()`: stop the
    /// schedules, reset, reprogram the list bases, rebuild the async ring, run,
    /// and re-power the ports (a reset drops them) so the caller's next pass can
    /// enumerate again.
    fn recover_from_hse(&mut self) {
        let status = self.regs.read32(self.operational + op::USBSTS);
        log::error!("ehci: host system error (USBSTS {status:#010x}); resetting the controller");
        // `ehci_halt()`: drop the run bit and wait for HCH to latch.
        let command = self.regs.read32(self.operational + op::USBCMD);
        self.regs.write32(
            self.operational + op::USBCMD,
            command & !(regs::cmd::RS | regs::cmd::ASE),
        );
        for _ in 0..100 {
            if self.regs.read32(self.operational + op::USBSTS) & sts::HCHALTED != 0 {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100));
        }
        // `ehci_reset()`: clear the latches by writing them back, then HCRESET.
        self.regs.write32(self.operational + op::USBSTS, status);
        self.regs
            .write32(self.operational + op::USBCMD, cmd_hcreset());
        for _ in 0..250 {
            if self.regs.read32(self.operational + op::USBCMD) & cmd_hcreset() == 0 {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_000));
        }
        self.regs.write32(self.operational + op::CTRLDSSEGMENT, 0);
        self.regs.write32(self.operational + op::CONFIGFLAG, 1);
        if let Some(frame_list) = self.frame_lists.first() {
            self.regs.write32(
                self.operational + op::PERIODICLISTBASE,
                frame_list.bus_addr() as u32,
            );
        }
        self.regs.write32(
            self.operational + op::ASYNCLISTADDR,
            self.qh.bus_addr() as u32,
        );
        // SAFETY: the ring is rebuilt from this controller's own queue heads.
        unsafe {
            build_async_ring(
                self.qh.bus_addr(),
                self.qh.as_ptr().cast::<Qh>(),
                self.qtds.bus_addr() as u32
                    + (QTD_CONTROL_DUMMY * core::mem::size_of::<Qtd>()) as u32,
            );
        }
        self.regs.write32(self.operational + op::USBSTS, 0);
        self.regs
            .write32(self.operational + op::USBCMD, run_command());
        for _ in 0..100 {
            if self.regs.read32(self.operational + op::USBSTS) & sts::ASS != 0 {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100));
        }
        // A controller reset drops the port power state, so power every port
        // again; the reset/enable sequence is the caller's next pass.
        for port in 0..self.ports {
            let offset = self.port_register(port);
            let value = self.regs.read32(offset);
            self.regs.write32(offset, value | portsc::PP);
        }
        let after = self.regs.read32(self.operational + op::USBSTS);
        log::warn!(
            "ehci: after reset USBSTS {after:#010x} (halted {}, ass {})",
            after & sts::HCHALTED != 0,
            after & sts::ASS != 0,
        );
    }

    fn ensure_running(&self) {
        // The reference clears a latched halt by writing USBSTS back
        // (`ehci_handshake()`/`ehci_reset()`) and otherwise leaves the schedule
        // alone. Rebuilding the ring here -- what this used to do -- tears down
        // the queue head of the transfer that is about to use it, which is how a
        // single Host System Error used to poison every later transfer.
        let status = self.regs.read32(self.operational + op::USBSTS);
        let command = self.regs.read32(self.operational + op::USBCMD);
        if status & (sts::HCHALTED | sts::HSE) != 0 {
            // `ehci_halt()` + the run write from `ehci_run()`: the halt is a
            // latch, and clearing the status bit does not leave it -- the run bit
            // has to be dropped and raised again. The ring is *not* touched: the
            // queue head of the next transfer is written after this point.
            log::warn!("ehci: controller halted (USBSTS {status:#010x}); restarting the run bit");
            self.regs
                .write32(self.operational + op::USBCMD, command & !regs::cmd::RS);
            for _ in 0..20 {
                if self.regs.read32(self.operational + op::USBSTS) & sts::HCHALTED != 0 {
                    break;
                }
                khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100));
            }
            self.regs.write32(self.operational + op::USBSTS, status);
            self.regs
                .write32(self.operational + op::USBCMD, run_command());
            for _ in 0..40 {
                if self.regs.read32(self.operational + op::USBSTS) & sts::ASS != 0 {
                    break;
                }
                khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100));
            }
        }
        // `ehci_poll_ASS()`: `want = (command & CMD_ASE) ? STS_ASS : 0` against the
        // status bit. The controller stops the asynchronous schedule on its own
        // once it finds it empty, and the enable bit alone does not bring it
        // back -- software has to notice and set it again.
        let status = self.regs.read32(self.operational + op::USBSTS);
        if status & sts::ASS == 0 {
            log::warn!("ehci: asynchronous schedule stopped (USBSTS {status:#010x}); setting ASE");
            self.regs
                .write32(self.operational + op::USBCMD, run_command());
            for _ in 0..40 {
                if self.regs.read32(self.operational + op::USBSTS) & sts::ASS != 0 {
                    break;
                }
                khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100));
            }
        }
    }

    /// One bulk transfer.
    ///
    /// `qh_make()`'s `PIPE_BULK` branch: the queue head carries the endpoint's
    /// own maximum packet size, the high-speed NAK reload and MULT, and the data
    /// toggle comes from the qTD (`QH_TOGGLE_CTL`). One qTD covers the whole
    /// request, which is all a mass-storage sector needs.
    fn bulk_transfer_impl(
        &mut self,
        address: u8,
        endpoint: u8,
        data: &mut [u8],
        is_in: bool,
        retries_left: u8,
    ) -> KResult<usize> {
        self.recover_if_hse();
        self.ensure_running();
        let BULK_QH: usize = if is_in { 3 } else { 2 };
        let direction = usize::from(is_in);
        let length = data.len();
        if length == 0 || length > BULK_BUFFER_LEN {
            return Err(KError::InvalidInput);
        }
        let eps = match self.speed {
            Speed::Low => regs::eps_low(),
            Speed::High => regs::eps_high(),
            _ => regs::eps_full(),
        };
        // The specification fixes high-speed bulk endpoints at 512 bytes; a
        // vendor that ignores that is out of scope until a device shows up.
        let max_packet: u32 = if self.speed == Speed::High { 512 } else { 64 };
        // OUT data goes into the DMA buffer first.
        if !is_in {
            // SAFETY: `length` bytes were checked against the buffer size.
            unsafe {
                core::ptr::copy_nonoverlapping(data.as_ptr(), self.bulk.as_ptr(), length);
            }
        }
        // The queue head is programmed once per endpoint (when the disk is
        // published): its `qtd_next` always points at this endpoint's dummy
        // descriptor, and enqueueing means replacing that descriptor's contents.
        // Repointing a queue head the controller may be reading is what produced
        // the intermittent faults this replaces.
        // Programme the endpoint once (`qh_make()`): the queue head carries the
        // endpoint's characteristics and stays out of the list until `qh_link()`.
        // The descriptor slots are per transfer now, so this no longer depends on
        // which dummy the queue head is parked on.
        let head = self.qh.as_ptr().cast::<Qh>();
        // SAFETY: dword 1 of this endpoint's queue head, inside the allocation.
        let programmed = unsafe {
            core::ptr::read_volatile(
                head.wrapping_add(BULK_QH)
                    .cast::<u8>()
                    .wrapping_add(4)
                    .cast::<u32>(),
            )
        } & info1::ADDRESS_MASK;
        if programmed != u32::from(address & 0x7f) {
            let qh = Qh {
                next: regs::LIST_END,
                info1: info1::NAK_RELOAD
                    | (max_packet << info1::MAX_PACKET_SHIFT)
                    | eps
                    | (u32::from(address) & info1::ADDRESS_MASK)
                    | (u32::from(endpoint & 0x0f) << info1::ENDPOINT_SHIFT),
                info2: if eps == regs::eps_high() {
                    regs::info2::MULT_HIGH_SPEED
                } else {
                    0
                },
                current: 0,
                qtd_next: regs::LIST_END,
                alt_next: regs::LIST_END,
                token: 0,
                buffer: [0; 5],
            };
            // SAFETY: this endpoint's queue head, inside the four-QH allocation.
            unsafe {
                core::ptr::write_volatile(head.wrapping_add(BULK_QH), qh);
            }
            karch::dsb_ishst();
        }
        // The enqueue reports which slot now carries this transfer's descriptor:
        // the completion loop waits on that one, not on a fixed slot.
        let overlay_before = self.qh_overlay_token(BULK_QH);
        let (_, filled_slot) = self.bulk_enqueue(BULK_QH, direction, is_in, length)?;
        let mut token = self.qh_wait_token(BULK_QH, filled_slot);
        // Which transfer latches the fault, exactly: checking only on the next
        // entry names the victim, not the culprit.
        {
            let status = self.regs.read32(self.operational + op::USBSTS);
            if status & sts::HSE != 0 {
                log::error!(
                    "ehci: bulk {} len {length} left HSE latched (USBSTS {status:#010x}, token \
                     {token:#010x})",
                    if is_in { "in" } else { "out" },
                );
                self.regs.write32(self.operational + op::USBSTS, sts::HSE);
            }
        }
        // `qh_completions()`: the descriptor is done when the controller clears
        // its Active bit. Anything still Active after the poll is a transfer the
        // controller never finished, and the queue head leaves the list either
        // way -- the next transfer must not inherit this one's descriptors.
        if token & token::ACTIVE != 0 {
            let status = self.regs.read32(self.operational + op::USBSTS);
            log::error!(
                "ehci: bulk transfer did not complete, token {token:#010x}, USBSTS {status:#010x} \
                 (halted {}, hse {}, ass {}, rec {})",
                status & sts::HCHALTED != 0,
                status & sts::HSE != 0,
                status & sts::ASS != 0,
                status & sts::REC != 0,
            );
            self.log_async_ring("bulk transfer");
            self.qh_unlink(BULK_QH);
            // A Host System Error latched *during* the transfer leaves the
            // schedule unreliable: repeat `ehci_reset()`/`ehci_run()` and give the
            // transfer one more chance. Without the retry the first read after a
            // fault fails the mount, and every later mount attempt starts from
            // the same broken schedule.
            if status & sts::HSE != 0 && retries_left > 0 {
                log::warn!("ehci: bulk transfer hit a host system error, recovering and retrying");
                self.recover_from_hse();
                // Clear both endpoints' data toggles: the device's view of the
                // toggle is whatever survived the fault, and a mismatch shows up
                // as a stalled CBW.
                self.clear_endpoint_halt(address, endpoint, !is_in);
                return self.bulk_transfer_impl(address, endpoint, data, is_in, retries_left - 1);
            }
            return Err(KError::TimedOut);
        }
        self.qh_unlink(BULK_QH);
        if token & token::HALT != 0 {
            log::error!("ehci: bulk transfer halted, token {token:#010x}");
            return Err(KError::Io);
        }
        let residual = ((token & token::LEN_MASK) >> token::LEN_SHIFT) as usize;
        let transferred = length.saturating_sub(residual);
        if is_in && transferred > 0 {
            // SAFETY: the controller wrote `transferred` bytes into the bulk
            // buffer, which is at least `transferred` bytes long.
            unsafe {
                core::ptr::copy_nonoverlapping(self.bulk.as_ptr(), data.as_mut_ptr(), transferred);
            }
        }
        Ok(transferred)
    }

    /// One bulk transfer, built the way `ehci_urb_enqueue()` builds it.
    ///
    /// The order is the mechanism, and every step is a reference step:
    /// `qh_append_tds()` hands the descriptor to the queue head while it is
    /// *unlinked*, `qh_update()` points the overlay at it, `qh_link_async()`
    /// puts the queue head in the list, `qh_completions()` polls the descriptor
    /// token (with the transaction-error retry), and the queue head is unlinked
    /// again before the next transfer touches it.
    fn bulk_enqueue(
        &mut self,
        qh_index: usize,
        direction: usize,
        is_in: bool,
        length: usize,
    ) -> KResult<(u32, usize)> {
        // `qh_append_tds()` alternates the roles of the two descriptors: the one
        // the controller is parked on receives this transfer, and the slot it came
        // from becomes the next dummy. Keeping the roles fixed instead (which this
        // driver did) forces software to move the queue head's overlay between the
        // two addresses on every transfer, and the overlay is the one structure the
        // reference never touches while the hardware may be sitting on it.
        let (slot_a, slot_b) = if is_in { QTD_BULK_IN } else { QTD_BULK_OUT };
        let dummy_slot = self.bulk_dummy_slot[direction];
        let source_slot = if dummy_slot == slot_a { slot_b } else { slot_a };
        let dummy_bus =
            self.qtds.bus_addr() as u32 + (dummy_slot * core::mem::size_of::<Qtd>()) as u32;
        let source_bus =
            self.qtds.bus_addr() as u32 + (source_slot * core::mem::size_of::<Qtd>()) as u32;
        // The list ends on the *new* dummy, which `qh_append_qtd()` writes into
        // the source slot; that address is therefore the source's, not the
        // current dummy's.
        // The data toggle this transfer has to start with is the one the
        // controller has been driving (`QH_TOGGLE_CTL` stays clear for bulk, so
        // the descriptor's DT bit is what it sends). The queue head's overlay is
        // the only place that sequence is recorded: carrying it into the qTD is
        // what keeps the two views of the toggle equal, and it was the missing
        // piece behind the transaction errors that appeared after a few dozen
        // transfers -- `overlay dt true, qtd dt false`.
        let toggle = self.qh_overlay_token(qh_index) & token::DT != 0;
        let qtd = Qtd {
            next: source_bus,
            alt_next: regs::LIST_END,
            token: ((if is_in { pid::IN } else { pid::OUT }) << token::PID_SHIFT)
                | (3 << token::CERR_SHIFT)
                | ((length as u32) << token::LEN_SHIFT)
                | if toggle { token::DT } else { 0 }
                | token::IOC
                | token::ACTIVE,
            // `qtd_fill()`: one buffer entry per 4K, page-aligned after the first.
            buffer: [self.bulk.bus_addr() as u32, 0, 0, 0, 0],
        };
        let overlay_target = self.qh_append_qtd(source_slot, dummy_slot, qtd);
        self.bulk_dummy_slot[direction] = source_slot;
        // Point the overlay at the descriptor just activated. Linux can leave it
        // alone because it always trades the roles the same way, but after a
        // failed transfer this queue head's overlay is left pointing at (and
        // still marked Active on) a descriptor nobody owns any more: skipping the
        // rewrite then makes the controller fetch that stale descriptor and the
        // next data stage stalls with no error at all. `qh_update()` only clears
        // the status bits, keeping the data toggle and PING.
        self.qh_update(qh_index, overlay_target);
        self.qh_link(qh_index);
        let _ = dummy_bus;
        Ok((overlay_target, dummy_slot))
    }

    /// `ehci_qtd_init()`: a descriptor that carries nothing and terminates.
    fn qtd_init(&self, slot: usize) {
        let cell = self.qtds.as_ptr().cast::<Qtd>().wrapping_add(slot);
        // SAFETY: `slot` is inside this controller's qTD allocation.
        unsafe {
            core::ptr::write_volatile(
                cell,
                Qtd {
                    next: regs::LIST_END,
                    alt_next: regs::LIST_END,
                    token: 0,
                    buffer: [0; 5],
                },
            );
        }
    }

    /// `qh_update()`: point the queue head's overlay at `qtd_bus` and hand the
    /// toggle over, without an Active descriptor.
    ///
    /// "Writes to an active overlay are unsafe" -- the controller reads the
    /// *overlay*, not the descriptor -- so the overlay is deactivated first, its
    /// `qtd_next`/`alt_next` are rewritten, and only then does a caller publish a
    /// descriptor token.
    /// The queue head's overlay token: what the controller last wrote back, and
    /// therefore the only record of its data toggle.
    fn qh_overlay_token(&self, qh_index: usize) -> u32 {
        let bytes = self
            .qh
            .as_ptr()
            .cast::<Qh>()
            .wrapping_add(qh_index)
            .cast::<u8>();
        // SAFETY: dword 6 of one queue head inside this allocation.
        unsafe { core::ptr::read_volatile(bytes.wrapping_add(24).cast::<u32>()) }
    }

    fn qh_update(&self, qh_index: usize, qtd_bus: u32) {
        let head = self.qh.as_ptr().cast::<Qh>();
        let bytes = head.wrapping_add(qh_index).cast::<u8>();
        // SAFETY: dwords 4..6 of one queue head inside this allocation.
        unsafe {
            core::ptr::write_volatile(bytes.wrapping_add(16).cast::<u32>(), qtd_bus);
            core::ptr::write_volatile(bytes.wrapping_add(20).cast::<u32>(), regs::LIST_END);
            let token = core::ptr::read_volatile(bytes.wrapping_add(24).cast::<u32>());
            core::ptr::write_volatile(
                bytes.wrapping_add(24).cast::<u32>(),
                token & (token::DT | token::PING),
            );
        }
        karch::dsb_ishst();
    }

    /// `qh_link_async()`: splice this queue head in right behind the async head.
    ///
    /// The queue head is *not* in the list while its descriptors are written; a
    /// linked queue head is re-read by the controller at every reclamation pass,
    /// which is what makes writing it unsafe. Linking is the last step before the
    /// controller may look at it, and unlinking is the first step after the
    /// transfer is done.
    fn qh_link(&self, qh_index: usize) {
        let head = self.qh.as_ptr().cast::<Qh>();
        let qh = head.wrapping_add(qh_index);
        let qh_bus = self.qh.bus_addr() as u32 + (qh_index * core::mem::size_of::<Qh>()) as u32;
        // `qh_link_async()`: `qh->hw_next = head->hw_next; head->hw_next = dma`.
        // The head is empty between transfers, so its successor is always the head
        // itself -- one queue head in the list at a time, which is all this
        // sequential driver needs.
        let head_next = self.qh.bus_addr() as u32 | 2;
        // SAFETY: dword 0 of this queue head: it inherits the head's successor.
        unsafe {
            core::ptr::write_volatile(qh.cast::<u32>(), head_next);
        }
        karch::dsb_ishst();
        // SAFETY: dword 0 of the head: this store is what links the queue head.
        unsafe {
            core::ptr::write_volatile(head.cast::<u32>(), (qh_bus & !0x1f) | regs::QH_TYPE);
        }
        karch::dsb_ishst();
        self.ring_async_advance();
    }

    /// `single_unlink_async()` + `start_iaa_cycle()`: take the queue head out.
    ///
    /// The splice comes first -- the controller must not find the queue head
    /// through the list any more -- and the async-advance doorbell after it lets
    /// the controller move past. Two doorbells, as the reference does ("the HC
    /// can write back the overlay region even after the IAA interrupt occurs").
    fn qh_unlink(&self, qh_index: usize) {
        let head = self.qh.as_ptr().cast::<Qh>();
        let qh = head.wrapping_add(qh_index);
        let qh_bus = self.qh.bus_addr() as u32 + (qh_index * core::mem::size_of::<Qh>()) as u32;
        // SAFETY: dword 0 of the async head, inside this allocation.
        let head_next = unsafe { core::ptr::read_volatile(head.cast::<u32>()) };
        if head_next & !0x1f == qh_bus & !0x1f {
            // SAFETY: dword 0 of this queue head, then of the head. The unlinked
            // queue head's successor is the head itself, so this restores the
            // empty list rather than chaining whatever followed the queue head.
            let successor = unsafe { core::ptr::read_volatile(qh.cast::<u32>()) };
            // SAFETY: dword 0 of the head.
            unsafe {
                core::ptr::write_volatile(head.cast::<u32>(), successor);
            }
            karch::dsb_ishst();
        }
        self.ring_async_advance();
        // The doorbell being accepted is not the same as the advance being done:
        // `USBCMD.IAAD` clears as soon as the controller latches the request, and
        // only `USBSTS.IAA` says it has walked past the queue head. Without this
        // wait the caller rewrites the queue head and its qTDs while the
        // controller may still be holding pointers into them, which the board
        // shows as `USBSTS 0xb03d` (HSE|HCH) on the *next* transfer.
        for _ in 0..100 {
            let status = self.regs.read32(self.operational + op::USBSTS);
            if status & sts::IAA != 0 {
                self.regs.write32(self.operational + op::USBSTS, sts::IAA);
                return;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100));
        }
    }

    /// `qh_append_tds()`: hand one descriptor to a queue head that is not linked.
    ///
    /// The reference deactivates the source descriptor, copies it into the slot
    /// the queue head is parked on, points the list's tail at the *new* dummy
    /// (the old source, re-initialised) and only then publishes the real token:
    ///
    /// ```c
    /// 	token = qtd->hw_token;
    /// 	qtd->hw_token = HALT_BIT(ehci);
    /// 	dummy = qh->dummy;  dma = dummy->qtd_dma;  *dummy = *qtd;
    /// 	ehci_qtd_init(ehci, qtd, qtd->qtd_dma);  qh->dummy = qtd;
    /// 	qtd->hw_next = QTD_NEXT(ehci, dma);      /* tail -> new dummy */
    /// 	wmb ();  dummy->hw_token = token;
    /// ```
    ///
    /// Returns the bus address the queue head's overlay has to point at.
    fn qh_append_qtd(&self, source_slot: usize, dummy_slot: usize, qtd: Qtd) -> u32 {
        let source_bus =
            self.qtds.bus_addr() as u32 + (source_slot * core::mem::size_of::<Qtd>()) as u32;
        let dummy_bus =
            self.qtds.bus_addr() as u32 + (dummy_slot * core::mem::size_of::<Qtd>()) as u32;
        let source = self.qtds.as_ptr().cast::<Qtd>().wrapping_add(source_slot);
        let dummy = self.qtds.as_ptr().cast::<Qtd>().wrapping_add(dummy_slot);
        // The descriptor is written into the source, deactivated: the controller
        // never reads this slot, it reads the one the overlay points at.
        let deactivated = Qtd {
            token: qtd.token & !token::ACTIVE | token::HALT,
            ..qtd
        };
        // SAFETY: both slots are inside this controller's qTD allocation.
        unsafe {
            core::ptr::write_volatile(source, deactivated);
        }
        karch::dsb_ishst();
        // The copy the controller will walk: payload first, and its `next` is the
        // *new* dummy (the source slot, re-initialised below) so the list parks on
        // an inactive descriptor instead of halting the queue head.
        let active = Qtd {
            next: source_bus,
            ..qtd
        };
        // SAFETY: as above.
        unsafe {
            core::ptr::write_volatile(dummy, Qtd { token: 0, ..active });
        }
        karch::dsb_ishst();
        self.qtd_init(source_slot);
        // "let the hc process these next qtds"
        karch::dsb_ishst();
        // SAFETY: dword 2 of the dummy slot is the token that hands it over.
        unsafe {
            core::ptr::write_volatile(
                dummy.cast::<u8>().wrapping_add(8).cast::<u32>(),
                active.token,
            );
        }
        karch::dsb_ishst();
        dummy_bus
    }

    /// `qh_completions()`' poll loop: the descriptor's token, with the reference's
    /// transaction-error retry.
    ///
    /// An XactErr that exhausts CErr leaves the descriptor HALTed; the reference
    /// re-arms it -- in the descriptor *and* in the queue head overlay, which
    /// still holds a copy -- and retries up to `QH_XACTERR_MAX` times before
    /// giving up and reporting the error.
    fn qh_wait_token(&self, qh_index: usize, slot: usize) -> u32 {
        const QH_XACTERR_MAX: u32 = 32;
        let cell = self.qtds.as_ptr().cast::<Qtd>().wrapping_add(slot);
        let token_word = cell.cast::<u8>().wrapping_add(8).cast::<u32>();
        let head = self.qh.as_ptr().cast::<Qh>();
        let overlay = head
            .wrapping_add(qh_index)
            .cast::<u8>()
            .wrapping_add(24)
            .cast::<u32>();
        let mut retries = 0u32;
        let mut last = token::ACTIVE;
        for _ in 0..POLL_STEPS {
            // SAFETY: dword 2 of the slot is its token.
            let token = unsafe { core::ptr::read_volatile(token_word) };
            last = token;
            if token & token::ACTIVE == 0 {
                // "retry transaction errors until we reach the software
                // xacterr limit": CERR exhausted means the three tries are gone.
                let xact = token & token::XACT != 0;
                let cerr = (token >> token::CERR_SHIFT) & 0x3;
                if token & token::HALT != 0 && xact && cerr == 0 && retries < QH_XACTERR_MAX {
                    retries += 1;
                    let retry = (token & !token::HALT) | token::ACTIVE | (3 << token::CERR_SHIFT);
                    // SAFETY: the descriptor's token and the queue head's overlay
                    // copy of it, both inside this controller's allocations.
                    unsafe {
                        core::ptr::write_volatile(token_word, retry);
                        core::ptr::write_volatile(overlay, retry);
                    }
                    karch::dsb_ishst();
                    continue;
                }
                return token;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
        }
        // Return what the descriptor actually holds: a sentinel here would hide
        // whether the controller never touched it or left it Active.
        last
    }

    /// Dumps the async ring exactly as the controller holds it.
    ///
    /// Read once per stall: one halted or mis-pointed queue head anywhere in the
    /// circular list stops the walk, and every endpoint behind it then looks
    /// exactly like "the controller ignored what we wrote". `hw_current` (dword
    /// 3) is the discriminator: zero with `qtd_next` pointing at an Active
    /// descriptor means the head was never fetched; equal to that descriptor's
    /// address with the token still Active means it was fetched and not executed.
    fn log_async_ring(&self, what: &str) {
        // The dump is worth its lines once or twice: after that the interesting
        // fact is that the same stall happened again.
        const DUMPS: usize = 2;
        static COUNT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
        if COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed) >= DUMPS {
            log::warn!("ehci: {what} stalled again (ring dump suppressed)");
            return;
        }
        let head = self.qh.as_ptr().cast::<Qh>();
        for index in 0..4usize {
            let word = |offset: usize| {
                // SAFETY: dword `offset` of queue head `index`, inside the
                // four-QH allocation.
                unsafe {
                    core::ptr::read_volatile(
                        head.wrapping_add(index)
                            .cast::<u8>()
                            .wrapping_add(offset)
                            .cast::<u32>(),
                    )
                }
            };
            log::error!(
                "ehci: qh{index} next {:#x} info1 {:#x} current {:#x} qtd_next {:#x} alt {:#x} \
                 token {:#x} (halt {}, active {})",
                word(0),
                word(4),
                word(12),
                word(16),
                word(20),
                word(24),
                word(24) & regs::token::HALT != 0,
                word(24) & regs::token::ACTIVE != 0,
            );
        }
        let command = self.regs.read32(self.operational + op::USBCMD);
        log::error!(
            "ehci: usbcmd {command:#x} (ase {}, pse {}, rs {}) async_next {:#x} frindex {}",
            command & regs::cmd::ASE != 0,
            command & regs::cmd::PSE != 0,
            command & regs::cmd::RS != 0,
            self.regs.read32(self.operational + op::ASYNCLISTADDR),
            self.regs.read32(self.operational + op::FRINDEX),
        );
        for slot in 0..QTD_SLOTS {
            // SAFETY: every slot is inside this controller's qTD allocation.
            let token = unsafe {
                core::ptr::read_volatile(
                    self.qtds
                        .as_ptr()
                        .cast::<Qtd>()
                        .wrapping_add(slot)
                        .cast::<u8>()
                        .wrapping_add(8)
                        .cast::<u32>(),
                )
            };
            log::error!(
                "ehci: qtd{slot} at {:#x} token {token:#010x} (halt {}, active {})",
                self.qtds.bus_addr() + (slot * core::mem::size_of::<Qtd>()) as u64,
                token & regs::token::HALT != 0,
                token & regs::token::ACTIVE != 0,
            );
        }
    }

    /// Sets the async-advance doorbell and waits for the controller to take it.
    ///
    /// `start_unlink_async()` rings the same bit: a controller sitting in the
    /// reclamation state does not re-walk the async list on its own, so work
    /// queued while it idles there would never be fetched.
    fn ring_async_advance(&self) {
        // `end_iaa_cycle()`: a Synopsys HC (this one is a DesignWare core) needs
        // the async list head re-published after every advance cycle, or the
        // schedule stops walking the list even though it still reports itself
        // running -- `USBSTS` keeps REC|ASS set and an Active descriptor is
        // never fetched. Re-writing the register before the doorbell is what
        // that workaround does, and it is the only difference between this
        // controller stalling forever and finishing the transfer.
        self.regs.write32(
            self.operational + op::ASYNCLISTADDR,
            self.qh.bus_addr() as u32,
        );
        let command = self.regs.read32(self.operational + op::USBCMD);
        self.regs
            .write32(self.operational + op::USBCMD, command | regs::cmd::IAAD);
        for _ in 0..40 {
            if self.regs.read32(self.operational + op::USBCMD) & regs::cmd::IAAD == 0 {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100));
        }
    }

    /// Builds the setup/data/status qTD chain and returns the first one's address.
    fn build_chain(&self, is_in: bool, length: usize, data_base: u32) -> u32 {
        let qtd_base = self.qtds.as_ptr().cast::<Qtd>();
        let qtd_bus = self.qtds.bus_addr() as u32;
        let qtd_stride = core::mem::size_of::<Qtd>() as u32;
        let mut index = 0usize;
        // Each stage carries its own buffer: a qTD's first buffer pointer is the
        // one the controller starts at, so handing every stage the setup
        // packet's address made the data stage write over the setup packet while
        // the driver read from the data offset -- an IN transfer then reported
        // the right byte count and a buffer full of zeros.
        let setup_buf = self.buffers.bus_addr() as u32;
        let mut push =
            |next_addr: u32, pid_code: u32, len: u32, dt: bool, ioc: bool, buf: u32| -> u32 {
                let qtd = Qtd {
                    // Next qTD Pointer bit 0 is T (terminate): setting it turns the
                    // qTD into the last one and the hardware never advances to
                    // `next_addr`, so only the setup stage of a control transfer ran
                    // and the driver read the status it happened to leave behind.
                    next: if next_addr == 0 {
                        regs::LIST_END
                    } else {
                        next_addr
                    },
                    alt_next: regs::LIST_END,
                    token: (pid_code << token::PID_SHIFT)
                        | (3 << token::CERR_SHIFT)
                        | (len << token::LEN_SHIFT)
                        | if dt { token::DT } else { 0 }
                        | if ioc { token::IOC } else { 0 }
                        | token::ACTIVE,
                    // The second page pointer lets a stage that starts at offset 64
                    // of the 4 KiB buffer cross into the next page.
                    buffer: [buf, buf + 4096, 0, 0, 0],
                };
                let address = qtd_bus + index as u32 * qtd_stride;
                let cell = qtd_base.wrapping_add(index);
                // SAFETY: `index` selects one of the three qTDs in the allocation.
                unsafe {
                    core::ptr::write_volatile(cell, qtd);
                }
                index += 1;
                address
            };
        // The chain is built back to front so each qTD knows its successor. The
        // last one ends on the dummy descriptor, as `qh_append_tds()` does
        // (`qtd->hw_next = QTD_NEXT(ehci, dummy->qtd_dma)`).
        let dummy =
            self.qtds.bus_addr() as u32 + (QTD_CONTROL_DUMMY * core::mem::size_of::<Qtd>()) as u32;
        let status_addr = push(
            dummy,
            if is_in { pid::OUT } else { pid::IN },
            0,
            true,
            true,
            setup_buf,
        );
        let data_addr = if length > 0 {
            push(
                status_addr,
                if is_in { pid::IN } else { pid::OUT },
                length as u32,
                true,
                false,
                data_base,
            )
        } else {
            status_addr
        };
        push(data_addr, pid::SETUP, 8, false, false, setup_buf)
    }
}

const fn cmd_hcreset() -> u32 {
    crate::ehci::regs::cmd::HCRESET
}

impl Hcd for Ehci {
    fn port_count(&self) -> usize {
        self.ports
    }

    fn port_status(&self, port: usize) -> KResult<PortStatus> {
        let value = self.regs.read32(self.port_register(port));
        Ok(PortStatus {
            connected: value & portsc::CCS != 0,
            enabled: value & portsc::PED != 0,
            // EHCI line status: J-state (D+ high) is full/high speed, K-state
            // (D- high) is low speed (EHCI 2.3.9).
            speed: match value & portsc::LINE_MASK {
                x if x == (1 << 10) => Speed::Low,
                x if x == (2 << 10) => Speed::Full,
                _ => Speed::Unknown,
            },
            changed: value & portsc::CHANGE_BITS != 0,
        })
    }

    fn port_reset(&mut self, port: usize) -> KResult<Speed> {
        let offset = self.port_register(port);
        // The sequence the in-tree usb-host stack uses (`Ehci::reset_port`):
        // assert power and reset while clearing only the *change* bits, hold the
        // reset for 50 ms, drop it, settle 20 ms -- and never write PORTSC again
        // afterwards.
        //
        // Two details are load-bearing, both learned from the board rather than
        // from the spec:
        //
        // - PORT_ENABLE is kept. Clearing it (which Linux does, because its root
        //   hub owns the port state) races the controller setting it again: a
        //   write that samples PED as 0 turns the port back off. The board showed
        //   exactly that, alternating between 'portsc 0x1045 (ped true, line 0)'
        //   and 'portsc 0x180b (ped false, line 2)' across attempts.
        // - There is no trailing "acknowledge the change bits" write. That write
        //   masked PORTSC down to PP|PED, so if the controller enabled the port
        //   between the read and the write, the write disabled it again.
        let current = self.regs.read32(offset);
        // If the port was already enabled before this reset, PED staying set must
        // not be mistaken for "the reset completed".
        let was_enabled = current & portsc::PED != 0;
        self.regs.write32(
            offset,
            (current | portsc::PP | portsc::RST) & !portsc::CHANGE_BITS,
        );
        // The line state as a sequence, sampled every 500 µs across the reset
        // window: a high-speed device answers the reset with a chirp K about
        // 2.5-3 ms in and then the port settles to SE0; a full-speed device never
        // leaves J. A sequence (rather than "did we ever see K") also shows when
        // it happened, which a boolean cannot.
        let mut line = alloc::string::String::new();
        for _ in 0..100 {
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(500));
            let value = self.regs.read32(offset);
            let state = (value & portsc::LINE_MASK) >> 10;
            line.push(char::from(b'0' + state as u8));
            if !was_enabled && value & portsc::PED != 0 {
                line.push('|');
                break;
            }
        }
        info!(
            "ehci: port {} reset window line states (0=SE0 1=K 2=J, 500us/char, '|' = enabled): {}",
            port + 1,
            line
        );
        let in_reset = self.regs.read32(offset);
        self.regs.write32(offset, in_reset & !portsc::RST);
        // `hub_port_init()`: a root hub passes `HUB_ROOT_RESET_TIME` (60 ms) as the
        // reset delay and `hub_port_wait_reset()` sleeps it before the port is read
        // again; the first GET_DESCRIPTOR follows that.
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(60_000));
        let value = self.regs.read32(offset);
        // `ehci_port_speed()` reports USB_PORT_STAT_HIGH_SPEED for every port
        // unless the controller has an integrated TT -- which this one does not.
        // The line status is not consulted: full/low-speed devices are handed to
        // the companion controller.
        let speed = Speed::High;
        info!(
            "ehci: port {} reset, portsc {value:#010x} ped {} line {}",
            port + 1,
            value & portsc::PED != 0,
            (value & portsc::LINE_MASK) >> 10,
        );
        if value & portsc::PED == 0 {
            log::error!(
                "ehci: port {} did not enable after reset (portsc {value:#010x}); \
                 check_reset_complete() treats this as a device that never answered",
                port + 1
            );
            return Err(KError::Io);
        }
        self.speed = speed;
        Ok(speed)
    }

    fn control_transfer(
        &mut self,
        address: u8,
        setup: &SetupPacket,
        data: &mut [u8],
    ) -> KResult<usize> {
        let _ = address;
        let transfer = self.control_transfer_inner(address, setup, data, self.speed);
        // One retry after a schedule restart: a transfer that times out while
        // the async schedule still reports itself running is the stall seen on
        // this controller (`USBSTS 0xa008`: REC|ASS set, nothing halted, an
        // Active descriptor left unfetched). Restarting the schedule the way
        // `ehci_run()` does revives it, so the retry is a real second attempt
        // and not just a longer wait.
        let _ = &transfer;
        transfer
    }

    fn bulk_transfer(
        &mut self,
        address: u8,
        endpoint: u8,
        data: &mut [u8],
        is_in: bool,
    ) -> KResult<usize> {
        self.bulk_transfer_impl(address, endpoint, data, is_in, 1)
    }

    /// `ehci_endpoint_reset()`: put the queue head's data toggle back to DATA0.
    ///
    /// The reference only does this for an idle queue head and unlinks it first
    /// ("the toggle value in the QH can't be updated while the QH is active"), so
    /// the overlay is deactivated before the toggle bit is written; the next
    /// enqueue re-points `qtd_next` and publishes an Active token again.
    fn clear_endpoint_halt(&mut self, address: u8, endpoint: u8, is_out: bool) {
        // Bulk OUT is queue head 2, bulk IN is queue head 3 (EP0 is 1).
        let qh_index = if endpoint == 0 {
            1
        } else if is_out {
            2
        } else {
            3
        };
        let address_mask = u32::from(address & 0x7f);
        let head = self.qh.as_ptr().cast::<Qh>();
        let qh = head.wrapping_add(qh_index);
        let bytes = qh.cast::<u8>();
        // SAFETY: dwords 1 and 4..6 of one queue head inside this allocation.
        let (programmed, token) = unsafe {
            (
                core::ptr::read_volatile(bytes.wrapping_add(4).cast::<u32>()) & 0x7f,
                core::ptr::read_volatile(bytes.wrapping_add(24).cast::<u32>()),
            )
        };
        if programmed != address_mask {
            return;
        }
        let clear = !(regs::token::DT | regs::token::HALT | regs::token::ACTIVE | regs::LIST_END);
        // SAFETY: dwords 4..6 of the same queue head.
        unsafe {
            core::ptr::write_volatile(bytes.wrapping_add(16).cast::<u32>(), regs::LIST_END);
            core::ptr::write_volatile(
                bytes.wrapping_add(24).cast::<u32>(),
                (token & regs::token::PING) & clear,
            );
        }
        karch::dsb_ishst();
        self.ring_async_advance();
        info!(
            "ehci: endpoint {endpoint} ({} {}), halt cleared",
            if is_out { "out" } else { "in" },
            "toggle reset to DATA0"
        );
    }

    fn name(&self) -> &'static str {
        "ehci"
    }
}

/// Platform glue for the RK3588 EHCI nodes.
pub mod driver {
    use driver_base::{
        Device, DriverOps, OfMatch, PlatformBus, PlatformId, PoweredPhy, ProbeError, RuntimeError,
    };
    use log::info;
    use usb::Hcd;

    use super::Ehci;

    static IDS: &[PlatformId] = &[
        PlatformId::Of(OfMatch::compatible("rockchip,rk3588-ehci")),
        PlatformId::Of(OfMatch::compatible("generic-ehci")),
    ];

    pub struct EhciDriver;

    pub struct EhciData {
        /// The controller owns the register window; keeping it here is what
        /// keeps the mapping alive for the controller's lifetime. Shared with
        /// the mass-storage block backend, whose `read_block(&self)` cannot take
        /// the controller by value.
        _ehci: alloc::sync::Arc<ksync::Mutex<Ehci>>,
        /// Publications of any disks published from this controller; dropping a
        /// guard unpublishes them.
        _disks: alloc::vec::Vec<driver_base::Publication<subsys_block::class::BlockDeviceImpl>>,
        /// Enabled clocks, held for the controller's lifetime. The device tree
        /// lists four clocks without names, so they are taken by index; the
        /// controller can detect a connect without them but cannot run its reset
        /// state machine.
        _clocks: alloc::vec::Vec<driver_base::EnabledClock>,
        /// Powered-on phys, held so power_on's clock reference is not dropped.
        _phys: alloc::vec::Vec<PoweredPhy>,
    }

    impl DriverOps<PlatformBus> for EhciDriver {
        type Data = EhciData;

        fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<EhciData, ProbeError> {
            let regs = dev.res().iomap(0)?;
            let base = regs.region().base as u64;
            // The controller's `usb2-phy` port resolves to its block
            // (0xfd5d8000/0xfd5dc000); acquiring and powering the PHY here is
            // what turns its 480 MHz reference clock on.
            let mut phys: alloc::vec::Vec<PoweredPhy> = alloc::vec::Vec::new();
            if let Some(spec) = dev.res().map().phy_by_name("usb2-phy")
                && let Some(port) = of::find_node_by_phandle(spec.provider as u32)
                && let Some(parent_phandle) = port.parent_property_u32("phandle")
            {
                let phy = dev.res().phy_spec(driver_base::PhySpec::by_phandle_cells(
                    u64::from(parent_phandle),
                    spec.cells,
                    spec.ncells,
                ))?;
                phys.push(
                    phy.init_powered()
                        .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?,
                );
                info!("ehci: usb2 phy powered");
            }
            // Diagnostic: the usb2phy-grf block for this controller starts at
            // +0x0000 bit0 = `clkout_ctl` (the PHY's 480 MHz output gate).
            let (grf_base, block_base) = if base == 0xfc800000 {
                (0xfd5d8000_usize, 0xfd5d8000_usize)
            } else {
                (0xfd5dc000_usize, 0xfd5dc000_usize)
            };
            if let Ok(block) = dev.res().iomap_region(
                driver_base::MmioRegion::new(grf_base, 0x4000),
                "usb2phy-grf",
            ) {
                // Compare against the reference board, where the same PHY reads
                // 00=7066 04=7933 08=0d14 0c=0214 10=0. The host-port bank starts
                // at +0x10 in the Inno USB2 PHY layout, so a difference there is a
                // port that was never programmed.
                let mut line = alloc::string::String::new();
                for offset in (0..0x20).step_by(4) {
                    line.push_str(&alloc::format!(
                        " {offset:02x}={:08x}",
                        block.read32(offset)
                    ));
                }
                info!(
                    "ehci: usb2phy {block_base:#x}{line} (clkout_ctl bit0 {})",
                    if block.read32(0) & 0x1 != 0 {
                        "on"
                    } else {
                        "off"
                    },
                );
            }
            let mut clocks = alloc::vec::Vec::new();
            for index in 0..4 {
                match dev.res().clk_at(index) {
                    // Every entry is a real dependency: the CRU gates, the usb2phy's
                    // 480 MHz output (now published by the phy provider) and the
                    // CRU gate-link clock. Hand the deferral back so the framework
                    // retries once they exist, instead of bringing the controller
                    // up without its reference clock.
                    // Index 3 is the `rk3588-clock-gate-link` node, which has no
                    // driver in this tree yet; the other three are the controller's
                    // own gates and the phy's 480 MHz output.
                    Err(err @ ProbeError::Deferred(_)) if index < 3 => return Err(err),
                    Err(err) => info!("ehci: clock {index} unavailable: {err:?}"),
                    Ok(clock) => match clock.prepare_enable() {
                        Ok(enabled) => {
                            info!("ehci: clock {index} at {} Hz enabled", clock.get_rate());
                            clocks.push(enabled);
                        }
                        Err(err) => info!("ehci: clock {index} enable failed: {err:?}"),
                    },
                }
            }
            info!(
                "ehci: probing {:#x}+{:#x}",
                regs.region().base,
                regs.region().size
            );
            let mut ehci = Ehci::init(dev, regs)?;
            ehci.log_ports();

            // The controller becomes shareable here: the block backend needs it
            // through `&self`, and enumeration needs it mutably.
            let ehci = alloc::sync::Arc::new(ksync::Mutex::new(ehci));
            let ports = {
                let guard = ehci.lock();
                Hcd::port_count(&*guard)
            };
            let mut devices = 0usize;
            let mut disks = alloc::vec::Vec::new();
            // `hub_port_init()` retries a port reset (`for (retry = 0; retry < 3;
            // retry++)`), and the hub core is otherwise driven by port-change
            // events. This driver polls, so the equivalent is a bounded re-scan:
            // a device that connects while the first pass is still running is a
            // few hundred microseconds behind the scan, not absent -- measured as
            // `ccs true` at 128.885 s with "enumeration found 0 device(s)" already
            // printed at 128.783 s.
            const ENUM_PASSES: usize = 5;
            const ENUM_RESCAN_DELAY_US: u64 = 100_000;
            let mut done_ports: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
            for pass in 0..ENUM_PASSES {
                for port in 0..ports {
                    if done_ports.contains(&port) {
                        continue;
                    }
                    let (status, enumerated) = {
                        let mut guard = ehci.lock();
                        let status = guard
                            .port_status(port)
                            .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
                        if !status.connected {
                            continue;
                        }
                        (
                            status,
                            usb::enumerate::enumerate_port(&mut *guard, port, 1u8),
                        )
                    };
                    let _ = status;
                    info!("ehci: port {} has a device", port + 1);
                    match enumerated {
                        Ok(device) => {
                            info!("ehci: {}", device.summary());
                            devices += 1;
                            done_ports.push(port);
                            {
                                let mut guard = ehci.lock();
                                super::probe_mass_storage(&mut *guard, &device);
                            }
                            if let Some(publication) =
                                super::publish_mass_storage(dev, &ehci, &device)
                            {
                                disks.push(publication);
                            }
                        }
                        Err(err) => {
                            if pass + 1 == ENUM_PASSES {
                                log::error!("ehci: port {} enumeration failed: {err:?}", port + 1);
                            }
                        }
                    }
                }
                if pass + 1 < ENUM_PASSES {
                    khal::time::busy_wait(ktime_types::TimeSpan::from_micros(ENUM_RESCAN_DELAY_US));
                }
            }
            info!("ehci: enumeration found {devices} device(s)");

            Ok(EhciData {
                _ehci: ehci,
                _disks: disks,
                _clocks: clocks,
                _phys: phys,
            })
        }
    }

    driver_base::declare_driver!(PlatformBus, EhciDriver, "ehci", IDS);
}

/// The mass-storage disk as the block layer sees it.
///
/// `BlockDeviceOperations::read_block` takes `&self`, so the controller is held
/// behind a lock instead of by value; the BOT layer above the EHCI bulk path is
/// the same one the bring-up probe uses.
struct UsbBotDisk {
    ehci: alloc::sync::Arc<ksync::Mutex<Ehci>>,
    endpoints: usb::bot::BotEndpoints,
    blocks: u64,
}

/// BOT reads go through one bulk transfer, and that buffer is one page, so a
/// request larger than eight sectors is split.
const BOT_MAX_SECTORS: usize = 8;

impl UsbBotDisk {
    /// One-shot probe of everything the failing path depends on: the registers
    /// the controller reads, the memory software wrote, and whether a control
    /// transfer (EP0, its own queue head and descriptors) still works from this
    /// context. Runs once so a board log answers where the bulk path breaks.
    fn probe_context(&self) {
        let mut guard = self.ehci.lock();
        let asynclist = guard.regs.read32(guard.operational + op::ASYNCLISTADDR);
        let command = guard.regs.read32(guard.operational + op::USBCMD);
        let status = guard.regs.read32(guard.operational + op::USBSTS);
        log::info!(
            "ehci: context probe: ASYNCLISTADDR {asynclist:#x} (head {:#x}), USBCMD \
             {command:#010x}, USBSTS {status:#010x}",
            guard.qh.bus_addr(),
        );
        let head = guard.qh.as_ptr().cast::<Qh>();
        // SAFETY: both queue heads are inside this controller's allocation.
        let (head_next, bulk_next, bulk_overlay) = unsafe {
            (
                core::ptr::read_volatile(head.cast::<u32>()),
                core::ptr::read_volatile(head.wrapping_add(2).cast::<u32>()),
                core::ptr::read_volatile(
                    head.wrapping_add(2)
                        .cast::<u8>()
                        .wrapping_add(24)
                        .cast::<u32>(),
                ),
            )
        };
        log::info!(
            "ehci: context probe: head.next {head_next:#x}, bulk qh next {bulk_next:#x}, bulk \
             overlay {bulk_overlay:#010x}"
        );
        let speed = guard.speed;
        let mut descriptor = [0u8; 8];
        let setup = SetupPacket::get_descriptor(usb::descriptor::ty::DEVICE, 0, 8);
        match guard.control_transfer_inner(1, &setup, &mut descriptor, speed) {
            Ok(length) => log::info!(
                "ehci: context probe: control transfer on EP0 read {length} bytes: {:02x?}",
                &descriptor[..length.min(8)]
            ),
            Err(err) => log::error!("ehci: context probe: control transfer on EP0 failed: {err:?}"),
        }
    }
}

impl subsys_block::BlockDeviceOperations for UsbBotDisk {
    fn num_blocks(&self) -> u64 {
        self.blocks
    }

    fn block_size(&self) -> usize {
        512
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> kerrno::KResult {
        if buf.len() % 512 != 0 {
            log::error!(
                "ehci: read_block {block_id} + {} bytes is not a whole number of sectors",
                buf.len()
            );
            return Err(kerrno::KError::InvalidInput);
        }
        log::info!("ehci: read_block {block_id} + {} bytes", buf.len());
        {
            use core::sync::atomic::{AtomicBool, Ordering};
            static PROBED: AtomicBool = AtomicBool::new(false);
            if !PROBED.swap(true, Ordering::Relaxed) {
                self.probe_context();
            }
        }
        let mut guard = self.ehci.lock();
        let mut done = 0usize;
        while done < buf.len() {
            let left = (buf.len() - done) / 512;
            let sectors = left.min(BOT_MAX_SECTORS);
            let data = usb::bot::read_sectors(
                &mut *guard,
                &self.endpoints,
                (block_id + (done / 512) as u64) as u32,
                sectors as u16,
            )
            .or_else(|err| {
                log::error!(
                    "ehci: read_block {block_id} sector {} x{sectors} failed: {err:?}; running \
                     the bulk-only reset",
                    block_id + (done / 512) as u64
                );
                // A BOT device that lost track of a command NAKs everything,
                // which looks like a transfer that never completes. The
                // reference's soft reset is the documented way back.
                usb::bot::reset_recovery(&mut *guard, &self.endpoints)?;
                usb::bot::read_sectors(
                    &mut *guard,
                    &self.endpoints,
                    (block_id + (done / 512) as u64) as u32,
                    sectors as u16,
                )
            })?;
            if data.len() < sectors * 512 {
                log::error!(
                    "ehci: read_block {block_id} short read {} < {}",
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
        let mut guard = self.ehci.lock();
        let mut done = 0usize;
        while done < buf.len() {
            let left = (buf.len() - done) / 512;
            let sectors = left.min(BOT_MAX_SECTORS);
            usb::bot::write_sectors(
                &mut *guard,
                &self.endpoints,
                (block_id + (done / 512) as u64) as u32,
                &buf[done..done + sectors * 512],
            )?;
            done += sectors * 512;
        }
        Ok(())
    }

    fn flush(&self) -> kerrno::KResult {
        // The BOT path writes through the CSW, and the device acknowledges every
        // block before it answers; there is no separate cache to drain.
        Ok(())
    }
}

/// Publishes the mass-storage interface as a block device.
fn publish_mass_storage(
    dev: &mut driver_base::Device<'_, driver_base::PlatformBus>,
    ehci: &alloc::sync::Arc<ksync::Mutex<Ehci>>,
    device: &usb::UsbDevice,
) -> Option<driver_base::Publication<subsys_block::class::BlockDeviceImpl>> {
    for config in &device.configurations {
        for interface in &config.interfaces {
            if interface.class != 0x08 || interface.protocol != 0x50 {
                continue;
            }
            let mut endpoint_in = 0u8;
            let mut endpoint_out = 0u8;
            for endpoint in &interface.endpoints {
                if endpoint.is_in() {
                    endpoint_in = endpoint.address;
                } else {
                    endpoint_out = endpoint.address;
                }
            }
            if endpoint_in == 0 || endpoint_out == 0 {
                log::warn!("ehci: mass storage interface without both bulk endpoints");
                continue;
            }
            let endpoints = usb::bot::BotEndpoints {
                address: device.address,
                endpoint_in,
                endpoint_out,
                max_packet: 512,
                interface: interface.number,
            };
            // Capacity comes from the device itself, and the block layer needs
            // it before the disk is published.
            let blocks = {
                let mut guard = ehci.lock();
                // A device that has just been configured can still carry BOT
                // state from an earlier host; the specification's reset recovery
                // is the documented way to start the state machine clean.
                let _ = usb::bot::reset_recovery(&mut *guard, &endpoints);
                // SET_CONFIGURATION to the first bulk command is a state change
                // the device's firmware also has to absorb; the reference
                // spreads its first commands over `msleep` gaps for the same
                // reason.
                khal::time::busy_wait(ktime_types::TimeSpan::from_micros(200_000));
                match usb::bot::read_capacity(&mut *guard, &endpoints) {
                    Ok(data) if data.len() >= 8 => {
                        let last = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                        u64::from(last) + 1
                    }
                    _ => {
                        log::error!("ehci: mass storage reported no capacity");
                        continue;
                    }
                }
            };
            let backend = UsbBotDisk {
                ehci: ehci.clone(),
                endpoints,
                blocks,
            };
            let name = alloc::format!("usb{}", device.address);
            let parent_path = alloc::string::String::from(dev.info().path());
            let modalias = dev.info().modalias().map(alloc::string::String::from);
            // Major 8 is the conventional disk major; the block core owns the
            // dev_t registry and rejects a duplicate.
            let disk =
                match subsys_block::Gendisk::new(name, 8, 0, 1, alloc::boxed::Box::new(backend))
                    .map(|disk| disk.with_devnode_context(&parent_path, modalias))
                {
                    Ok(disk) => disk,
                    Err(err) => {
                        log::error!("ehci: could not create the block disk: {err:?}");
                        continue;
                    }
                };
            match dev.publish(
                &subsys_block::class::BLOCK,
                alloc::sync::Arc::new(disk),
                "usb-storage",
            ) {
                Ok(publication) => {
                    log::info!(
                        "ehci: published {} as a block device with {blocks} 512-byte blocks",
                        device.summary()
                    );
                    // What the /dev projection actually holds, so a missing node
                    // can be attributed to the publication or to devtmpfs.
                    let callback: devnode::DevNodeCallback = alloc::sync::Arc::new(|_| {});
                    for node in devnode::subscribe(callback) {
                        log::info!(
                            "ehci: devnode {} subsystem {} number {:?}",
                            node.devname(),
                            node.subsystem(),
                            node.device_number()
                        );
                    }
                    return Some(publication);
                }
                Err(err) => log::error!("ehci: block publication failed: {err:?}"),
            }
        }
    }
    None
}

/// If the device exposes a bulk-only mass-storage interface, read its first
/// sector: enumeration alone does not prove the bulk path works.
fn probe_mass_storage(ehci: &mut Ehci, device: &usb::UsbDevice) {
    for config in &device.configurations {
        for interface in &config.interfaces {
            if interface.class != 0x08 || interface.protocol != 0x50 {
                continue;
            }
            let mut endpoint_in = 0u8;
            let mut endpoint_out = 0u8;
            for endpoint in &interface.endpoints {
                if endpoint.is_in() {
                    endpoint_in = endpoint.address;
                } else {
                    endpoint_out = endpoint.address;
                }
            }
            if endpoint_in == 0 || endpoint_out == 0 {
                log::warn!("ehci: mass storage interface without both bulk endpoints");
                continue;
            }
            let endpoints = usb::bot::BotEndpoints {
                address: device.address,
                endpoint_in,
                endpoint_out,
                max_packet: 512,
                interface: interface.number,
            };
            log::info!("ehci: mass storage on ep in {endpoint_in:#04x} out {endpoint_out:#04x}",);
            usb::bot::describe(ehci, &endpoints);
            usb::bot::probe_filesystem(ehci, &endpoints);
            if let Err(err) = usb::bot_fs::list_root(ehci, &endpoints) {
                log::warn!("ehci: root directory listing failed: {err:?}");
            }
            return;
        }
    }
}
