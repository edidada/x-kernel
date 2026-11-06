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
/// Bulk descriptors: one per direction (`dummy_slot` in `bulk_transfer_impl`).
const QTD_BULK_OUT: usize = 5;
const QTD_BULK_IN: usize = 6;
/// The descriptor the async head's `alt_next` points at (`ehci_init()` does the
/// same with `async->dummy`).
const QTD_HEAD_DUMMY: usize = 7;

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
    /// The periodic frame list: allocated and programmed even though the
    /// periodic schedule is off, because the reference always has one.
    _frame_lists: alloc::vec::Vec<CoherentMem>,
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
    let size = core::mem::size_of::<Qh>() as u32;
    let total = 4u32;
    for index in 0..total {
        let entry = head.wrapping_add(index as usize);
        // A static ring: head -> EP0 -> bulk OUT -> bulk IN -> head. The
        // reference links only the head permanently and splices endpoints in per
        // transfer (`qh_link_async`), but splicing needs the unlink handshake as
        // its counterpart; with every endpoint in the ring and the head carrying
        // QH_HEAD, the controller walks all four and stops at the reclamation
        // head, which is the behaviour this driver is built around.
        let next = if index + 1 == total {
            base | 2
        } else {
            base + (index + 1) * size | 2
        };
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
        // SAFETY: the whole frame list was just allocated and zeroed below.
        unsafe {
            core::ptr::write_bytes(frame_list.as_ptr(), 0, 1024 * core::mem::size_of::<u32>());
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
        // SAFETY: slot `QTD_HEAD_DUMMY` is inside the qTD allocation.
        unsafe {
            core::ptr::write_volatile(
                qtds.as_ptr().cast::<Qtd>().wrapping_add(QTD_HEAD_DUMMY),
                Qtd {
                    next: regs::LIST_END,
                    alt_next: regs::LIST_END,
                    token: 0,
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
        let next_ptr = head_bytes.cast::<u32>();
        let ep_qh = head.wrapping_add(1);
        let ep_bus = qh.bus_addr() as u32 + core::mem::size_of::<Qh>() as u32;
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
            qtds.bus_addr() as u32 + (QTD_HEAD_DUMMY * core::mem::size_of::<Qtd>()) as u32,
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
            gate_snapshot: gates,
            cru,
            _frame_lists: frame_lists,
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
            next: self.qh_successor_bus(1),
            // No HEAD here: only the empty list head carries H, and a second
            // QH with it reads as the end of the reclamation list, so the
            // controller overlays this QH's qTD and never executes it.
            info1: info1::TOGGLE_CTL
                | info1::CONTROL_EP
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
            qtd_next: setup_addr,
            alt_next: regs::LIST_END,
            // Data toggle starts at DATA0 for the setup stage.
            token: 0,
            buffer: [0; 5],
        };
        // SAFETY: `head` is the single QH in this controller's allocation.
        unsafe {
            core::ptr::write_volatile(head, qh);
        }

        // A queue head stays in the ring, so it must not be left pointing at a
        // descriptor that never completed: the controller would retry that
        // descriptor forever and never advance to the next queue head. Linux
        // unlinks the queue head when a transfer ends or times out; parking it
        // with `qtd_next = LIST_END` is the same thing for a ring we never
        // rebuild per transfer.
        let park = |this: &Self, stop_schedule: bool| {
            let command = this.regs.read32(this.operational + op::USBCMD);
            if stop_schedule {
                this.regs
                    .write32(this.operational + op::USBCMD, command & !regs::cmd::RS);
                khal::time::busy_wait(ktime_types::TimeSpan::from_micros(200));
            }
            // SAFETY: dword 4 of the EP0 queue head, inside this allocation.
            unsafe {
                core::ptr::write_volatile(
                    head.cast::<u8>().wrapping_add(16).cast::<u32>(),
                    regs::LIST_END,
                );
            }
            if stop_schedule {
                this.regs
                    .write32(this.operational + op::USBCMD, run_command());
            }
        };

        // Wait for the *last* qTD of the chain to retire. The queue head overlay
        // starts with ACTIVE clear, so polling it returns immediately and reports
        // a transfer that never happened.
        let qtd_base = self.qtds.as_ptr().cast::<regs::Qtd>();
        let mut status_token = token::ACTIVE;
        for _ in 0..POLL_STEPS {
            // SAFETY: index 0 of the three qTDs in this allocation is the status
            // stage, which the controller executes last.
            status_token = unsafe { core::ptr::read_volatile(qtd_base.cast::<u32>().add(2)) };
            if status_token & token::ACTIVE == 0 {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
        }
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
                    self.log_async_ring("control transfer");
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
                park(self, false);
                return Ok(transferred);
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
        }
        log::error!("ehci: control transfer timed out");
        park(self, true);
        Err(KError::TimedOut)
    }

    /// The queue head that follows `index` in the asynchronous ring.
    ///
    /// The ring is head(0) -> EP0(1) -> bulk OUT(2) -> bulk IN(3) -> head, and
    /// every endpoint queue head is written with its own successor: writing the
    /// head's address into an endpoint queue head would cut the ring short and
    /// strand every endpoint after it.
    fn qh_successor_bus(&self, index: usize) -> u32 {
        let base = self.qh.bus_addr() as u32;
        let size = core::mem::size_of::<Qh>() as u32;
        match index {
            1 => base + 2 * size | 2,
            2 => base + 3 * size | 2,
            _ => base | 2,
        }
    }

    /// Rebuilds the asynchronous ring: head -> EP0 -> bulk OUT -> bulk IN -> head.
    ///
    /// Recovery must rebuild all of it. Writing the head's next pointer back to
    /// itself -- "empty list" -- is only correct while there is no endpoint
    /// queue head at all; with endpoints present it truncates the ring to
    /// `head -> head` and every endpoint becomes unreachable, which is precisely
    /// how a transfer ends up queued with an overlay the controller never
    /// touches.
    fn build_async_ring(&self) {
        build_async_ring(
            self.qh.bus_addr(),
            self.qh.as_ptr().cast::<Qh>(),
            self.qtds.bus_addr() as u32 + (QTD_HEAD_DUMMY * core::mem::size_of::<Qtd>()) as u32,
        );
    }

    /// Clears a latched halt before queueing new work.
    ///
    /// A Host System Error halts the controller and stays latched until software
    /// writes USBSTS back. Every transfer path needs this, control transfers
    /// included: the BOT soft reset is itself a control transfer, so without it
    /// the recovery path cannot run either.
    fn ensure_running(&self) {
        let status = self.regs.read32(self.operational + op::USBSTS);
        if status & (sts::HCHALTED | sts::HSE) != 0 {
            log::warn!(
                "ehci: controller halted before a transfer (USBSTS {status:#010x}); restarting \
                 the schedule"
            );
            self.recover_async_schedule();
        }
        // `ehci_poll_ASS()`: `want = (command & CMD_ASE) ? STS_ASS : 0` against
        // the status bit. The controller stops the asynchronous schedule on its
        // own once it finds it empty, and the enable bit alone does not bring it
        // back -- software has to notice and set it again. Without this the first
        // transfer after the schedule went idle waits out its timeout while every
        // structure in the ring is correct.
        let status = self.regs.read32(self.operational + op::USBSTS);
        if status & sts::ASS == 0 {
            let command = self.regs.read32(self.operational + op::USBCMD);
            log::warn!("ehci: asynchronous schedule stopped (USBSTS {status:#010x}); setting ASE");
            self.regs
                .write32(self.operational + op::USBCMD, command | regs::cmd::ASE);
            for _ in 0..40 {
                if self.regs.read32(self.operational + op::USBSTS) & sts::ASS != 0 {
                    break;
                }
                khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100));
            }
        }
    }

    /// Restarts the asynchronous schedule after a controller halt.
    fn recover_async_schedule(&self) {
        let status = self.regs.read32(self.operational + op::USBSTS);
        self.regs.write32(self.operational + op::USBSTS, status);
        // A halted controller leaves the queue head it was processing marked
        // HALTED in its overlay, and clearing USBSTS does not clear that. When
        // it is the head of the reclamation list, the whole asynchronous
        // schedule stops advancing and every later transfer waits forever with
        // reclamation reported -- which is exactly what the failing reads look
        // like. Rewrite the head's overlay and its links.
        let head = self.qh.as_ptr().cast::<Qh>();
        let head_bytes = head.cast::<u8>();
        // SAFETY: dword 6 of the head queue head is its overlay token.
        let token = unsafe { core::ptr::read_volatile(head_bytes.wrapping_add(24).cast::<u32>()) };
        // SAFETY: dword 6 of the same queue head.
        unsafe {
            core::ptr::write_volatile(
                head_bytes.wrapping_add(24).cast::<u32>(),
                token & (regs::token::DT | regs::token::PING),
            );
        }
        // Rebuild the whole ring: a self-linked head is only correct when there
        // are no endpoint queue heads, and with them present it strands every
        // endpoint ("0->0x...002" in the failure dump is this bug).
        build_async_ring(
            self.qh.bus_addr(),
            head,
            self.qtds.bus_addr() as u32 + (QTD_HEAD_DUMMY * core::mem::size_of::<Qtd>()) as u32,
        );
        let command = self.regs.read32(self.operational + op::USBCMD);
        self.regs
            .write32(self.operational + op::USBCMD, command & !regs::cmd::RS);
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_000));
        self.regs.write32(
            self.operational + op::ASYNCLISTADDR,
            self.qh.bus_addr() as u32,
        );
        self.regs
            .write32(self.operational + op::USBCMD, run_command());
        log::warn!("ehci: restarted the asynchronous schedule after a halt");
    }

    /// Links one of the two endpoint queue heads into the asynchronous schedule.
    ///
    /// The list stays `head -> qh -> head`: `qh_link_async()` splices a queue
    /// head in right after the list head and only the head carries H.
    fn link_endpoint(&self, qh_index: usize) {
        let head = self.qh.as_ptr().cast::<Qh>();
        let qh = head.wrapping_add(qh_index);
        let qh_bus = self.qh.bus_addr() as u32 + (qh_index * core::mem::size_of::<Qh>()) as u32;
        let head_bus = self.qh.bus_addr() as u32;
        // SAFETY: dword 0 of the endpoint queue head: point it back at the head.
        unsafe {
            core::ptr::write_volatile(qh.cast::<u32>(), head_bus | 2);
        }
        crate::ehci::regs::list_end_barrier();
        // SAFETY: dword 0 of the list head: splice the endpoint in.
        unsafe {
            core::ptr::write_volatile(head.cast::<u32>(), qh_bus | 2);
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
    ) -> KResult<usize> {
        self.ensure_running();
        let BULK_QH: usize = if is_in { 3 } else { 2 };
        let dummy_slot: usize = if is_in { 6 } else { 5 };
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
        let dummy_bus =
            self.qtds.bus_addr() as u32 + (dummy_slot * core::mem::size_of::<Qtd>()) as u32;
        if self.bulk_dummy_bus[BULK_QH - 2] != dummy_bus {
            let head = self.qh.as_ptr().cast::<Qh>();
            let qh = Qh {
                next: self.qh_successor_bus(BULK_QH),
                info1: info1::NAK_RELOAD
                    | (max_packet << info1::MAX_PACKET_SHIFT)
                    | eps
                    | (address as u32 & info1::ADDRESS_MASK)
                    | (u32::from(endpoint & 0x0f) << info1::ENDPOINT_SHIFT),
                info2: if eps == regs::eps_high() {
                    regs::info2::MULT_HIGH_SPEED
                } else {
                    0
                },
                current: 0,
                qtd_next: dummy_bus,
                alt_next: regs::LIST_END,
                token: 0,
                buffer: [0; 5],
            };
            // SAFETY: this endpoint's queue head, inside the four-QH allocation.
            // The schedule is not stopped for this (`qh_make()` writes the same
            // fields before linking the queue head): the endpoint characteristics
            // of an idle queue head are safe to write, and the descriptor the
            // controller is parked on is made inactive by `bulk_enqueue()`.
            unsafe {
                core::ptr::write_volatile(head.wrapping_add(BULK_QH), qh);
            }
            self.regs.write32(
                self.operational + op::ASYNCLISTADDR,
                self.qh.bus_addr() as u32,
            );
            let command = self.regs.read32(self.operational + op::USBCMD);
            self.regs.write32(
                self.operational + op::USBCMD,
                command | regs::cmd::RS | regs::cmd::ASE,
            );
            for _ in 0..40 {
                if self.regs.read32(self.operational + op::USBSTS) & sts::ASS != 0 {
                    break;
                }
                khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100));
            }
            self.bulk_dummy_bus[BULK_QH - 2] = dummy_bus;
        }
        self.bulk_enqueue(BULK_QH, dummy_slot, is_in, length)?;
        let qtd_ptr = self.qtds.as_ptr().cast::<Qtd>().wrapping_add(dummy_slot);
        let head = self.qh.as_ptr().cast::<Qh>();
        let mut token = token::ACTIVE;
        for _ in 0..POLL_STEPS {
            // SAFETY: dword 2 of the dummy descriptor is its token.
            token = unsafe { core::ptr::read_volatile(qtd_ptr.cast::<u32>().add(2)) };
            if token & token::ACTIVE == 0 {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
        }
        // "Never Active" is not a diagnostic: a transfer that completes before
        // the first poll reads back inactive with its status bits set, which is
        // the normal case for a 13-byte CSW. Only a descriptor that is *still*
        // Active after the timeout is evidence of anything.
        if token & token::ACTIVE != 0 {
            let status = self.regs.read32(self.operational + op::USBSTS);
            if status & sts::ASS != 0 {
                // The schedule is "running" yet never fetched an Active
                // descriptor: restart it the way `ehci_run()` does --
                // `disable_async()` clears ASE and waits for ASS to fall, then
                // the async head address is re-published and ASE set again.
                self.restart_async();
                self.bulk_enqueue(BULK_QH, dummy_slot, is_in, length)?;
                for _ in 0..POLL_STEPS {
                    // SAFETY: dword 2 of the dummy descriptor is its token.
                    token = unsafe { core::ptr::read_volatile(qtd_ptr.cast::<u32>().add(2)) };
                    if token & token::ACTIVE == 0 {
                        break;
                    }
                    khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
                }
                if token & token::ACTIVE == 0 {
                    log::warn!("ehci: transfer completed after an async-schedule restart");
                    return Ok(0);
                }
            }
            log::error!(
                "ehci: bulk transfer did not complete, token {token:#010x}, USBSTS {status:#010x} \
                 (halted {}, hse {}, ass {}, rec {})",
                status & sts::HCHALTED != 0,
                status & sts::HSE != 0,
                status & sts::ASS != 0,
                status & sts::REC != 0,
            );
            // The EP0 queue head's descriptors: if one is still Active, the
            // controller is parked on the control endpoint retrying it forever
            // and never reaches the bulk queue heads behind it in the ring.
            self.log_async_ring("bulk transfer");
            for slot in 0..3usize {
                // SAFETY: the first three qTDs belong to the control path.
                let control_qtd = unsafe {
                    core::ptr::read_volatile(self.qtds.as_ptr().cast::<Qtd>().wrapping_add(slot))
                };
                log::error!(
                    "ehci: control qtd{slot} token {:#x} (active {}, halt {}, len {})",
                    control_qtd.token,
                    control_qtd.token & token::ACTIVE != 0,
                    control_qtd.token & token::HALT != 0,
                    (control_qtd.token & token::LEN_MASK) >> token::LEN_SHIFT,
                );
            }
            // SAFETY: dword 4 of the EP0 queue head is its next-descriptor
            // pointer.
            let ep0_next = unsafe {
                core::ptr::read_volatile(head.cast::<u8>().wrapping_add(64 + 16).cast::<u32>())
            };
            log::error!("ehci: ep0 qh qtd_next {ep0_next:#x}");
            // Byte-for-byte readback of the descriptor as it is now: proves
            // whether what the controller is pointed at is what software wrote.
            let mut bytes = [0u32; 8];
            for (index, cell) in bytes.iter_mut().enumerate() {
                // SAFETY: the dummy slot holds eight dwords.
                *cell =
                    unsafe { core::ptr::read_volatile(qtd_ptr.cast::<u32>().wrapping_add(index)) };
            }
            log::error!(
                "ehci: descriptor readback: {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x}",
                bytes[0],
                bytes[1],
                bytes[2],
                bytes[3],
                bytes[4],
                bytes[5],
                bytes[6],
                bytes[7],
            );
            let now0 = self.cru.read32(CLKGATE_SNAPSHOT);
            let now1 = self.cru.read32(CLKGATE_SNAPSHOT + 4);
            log::error!(
                "ehci: cru gates now {now0:#010x} {now1:#010x} (snapshot {:#010x} {:#010x}){}",
                self.gate_snapshot[0],
                self.gate_snapshot[1],
                if now0 != self.gate_snapshot[0] || now1 != self.gate_snapshot[1] {
                    " -- CHANGED"
                } else {
                    ""
                },
            );
            return Err(KError::TimedOut);
        }
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

    /// `qh_update()` + `qh_append_tds()` for one bulk descriptor.
    ///
    /// Three steps, and the order is the mechanism:
    ///
    /// 1. park the queue head on the dummy descriptor and clear its overlay's
    ///    halt/active state. The controller reads the **overlay**, so writing a
    ///    descriptor it may still be executing is exactly the hazard the
    ///    reference's "writes to an active overlay are unsafe" warns about;
    /// 2. fill the dummy descriptor, token last — the Active bit is what hands
    ///    the transfer over;
    /// 3. ring the async-advance doorbell, because a controller parked in the
    ///    reclamation state does not re-walk the list on its own.
    ///
    /// Step 1 is what this driver was missing. After the controller exhausts a
    /// descriptor list it writes `LIST_END` and HALT into the queue head
    /// overlay; without re-pointing `qtd_next` at the dummy, every later
    /// descriptor stayed Active forever because nothing pointed the controller
    /// back at it.
    fn bulk_enqueue(
        &self,
        qh_index: usize,
        dummy_slot: usize,
        is_in: bool,
        length: usize,
    ) -> KResult<()> {
        let head = self.qh.as_ptr().cast::<Qh>();
        let dummy_bus =
            self.qtds.bus_addr() as u32 + (dummy_slot * core::mem::size_of::<Qtd>()) as u32;
        let overlay = |offset: usize| {
            head.wrapping_add(qh_index)
                .cast::<u8>()
                .wrapping_add(offset)
                .cast::<u32>()
        };
        // Step 1. Queue head dwords 4..6 are the overlay (`hw_qtd_next`,
        // `hw_alt_next`, `hw_token`).
        // SAFETY: the overlay of this endpoint's queue head, inside the
        // four-QH allocation; the offsets stay within one queue head.
        unsafe {
            core::ptr::write_volatile(overlay(16), dummy_bus);
            core::ptr::write_volatile(overlay(20), regs::LIST_END);
            let token = core::ptr::read_volatile(overlay(24));
            core::ptr::write_volatile(overlay(24), token & (token::DT | token::PING));
        }
        karch::dsb_ishst();
        // Step 2. The descriptor itself. The list ends on the dummy descriptor:
        // `qh_append_tds()` points the last qTD's `hw_next` at the queue head's
        // dummy (`qtd->hw_next = QTD_NEXT(ehci, dma)`) precisely so the
        // controller parks on an inactive descriptor instead of writing HALT
        // into the overlay behind software's back.
        let qtd_token = ((if is_in { pid::IN } else { pid::OUT }) << token::PID_SHIFT)
            | (3 << token::CERR_SHIFT)
            | ((length as u32) << token::LEN_SHIFT)
            | token::IOC
            | token::ACTIVE;
        let qtd_ptr = self.qtds.as_ptr().cast::<Qtd>().wrapping_add(dummy_slot);
        let base = qtd_ptr.cast::<u8>();
        let list_end =
            self.qtds.bus_addr() as u32 + (QTD_HEAD_DUMMY * core::mem::size_of::<Qtd>()) as u32;
        // For an IN transfer the reference points `hw_alt_next` at the same dummy
        // ("short reads advance to a 'magic' dummy instead of the next qtd"), so a
        // short packet stops the list without the controller halting the head.
        let alt_next = if is_in { list_end } else { regs::LIST_END };
        // SAFETY: the dummy slot is inside this controller's qTD allocation and
        // the offsets stay within its 32 bytes. `next`/`alt_next`/buffers land
        // before the token, which is written after the barrier below.
        unsafe {
            core::ptr::write_volatile(base.cast::<u32>(), list_end);
            core::ptr::write_volatile(base.wrapping_add(4).cast::<u32>(), alt_next);
            core::ptr::write_volatile(
                base.wrapping_add(12).cast::<u32>(),
                self.bulk.bus_addr() as u32,
            );
            core::ptr::write_volatile(base.wrapping_add(16).cast::<u32>(), 0);
            core::ptr::write_volatile(base.wrapping_add(20).cast::<u32>(), 0);
            core::ptr::write_volatile(base.wrapping_add(24).cast::<u32>(), 0);
            core::ptr::write_volatile(base.wrapping_add(28).cast::<u32>(), 0);
        }
        karch::dsb_ishst();
        // SAFETY: dword 2 of the same descriptor: the token, written last, is
        // what makes the transfer visible to the controller.
        unsafe {
            core::ptr::write_volatile(base.wrapping_add(8).cast::<u32>(), qtd_token);
        }
        karch::dsb_ishst();
        // Step 3.
        self.ring_async_advance();
        Ok(())
    }

    /// Restarts the async schedule if the controller claims it is still running.
    ///
    /// Returns `true` when a restart was performed, i.e. when the caller should
    /// retry the transfer once.
    fn recover_stalled_schedule(&self) -> bool {
        if self.regs.read32(self.operational + op::USBSTS) & sts::ASS == 0 {
            // The schedule is genuinely stopped; that is a different failure and
            // a restart is `ensure_running()`'s job, not this recovery's.
            return false;
        }
        self.restart_async();
        true
    }

    /// Restarts the async schedule: `disable_async()` then `ehci_run()`'s start.
    fn restart_async(&self) {
        let command = self.regs.read32(self.operational + op::USBCMD);
        self.regs
            .write32(self.operational + op::USBCMD, command & !regs::cmd::ASE);
        for _ in 0..100 {
            if self.regs.read32(self.operational + op::USBSTS) & sts::ASS == 0 {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100));
        }
        self.regs.write32(
            self.operational + op::ASYNCLISTADDR,
            self.qh.bus_addr() as u32,
        );
        self.regs.write32(
            self.operational + op::USBCMD,
            (command & !regs::cmd::ASE) | regs::cmd::ASE | regs::cmd::RS,
        );
        for _ in 0..100 {
            if self.regs.read32(self.operational + op::USBSTS) & sts::ASS != 0 {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100));
        }
        self.ring_async_advance();
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
            self.qtds.bus_addr() as u32 + (QTD_HEAD_DUMMY * core::mem::size_of::<Qtd>()) as u32;
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
        // Linux `ehci_hub_control`, SetPortFeature(PORT_RESET): assert the reset,
        // let it run for 50 ms, then *clear it again by hand* -- PR is not
        // self-clearing here, which is why waiting for it to fall times out.
        let current = self.regs.read32(offset);
        // PED is read/write in EHCI and a write of 0 disables the port, so it is
        // never part of a mask-out here: the port is being started, not stopped.
        self.regs.write32(
            offset,
            (current | portsc::PP | portsc::RST) & !portsc::CHANGE_BITS,
        );
        // The reset must be held for at least 50 ms: `hub_port_reset()` asks for
        // the feature and `hub_port_wait_reset()` only looks at the port again
        // after the reset timer expires. A high-speed device answers the host's
        // SE0 with a chirp K during this window, so the line states seen here
        // record whether it took part in the handshake.
        let mut saw_k = false;
        let mut saw_j = false;
        for _ in 0..50 {
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_000));
            let sample = self.regs.read32(offset);
            match (sample & portsc::LINE_MASK) >> 10 {
                1 => saw_k = true,
                2 => saw_j = true,
                _ => {}
            }
        }
        info!(
            "ehci: port {} reset window: chirp K seen {saw_k}, J seen {saw_j} (a high-speed \
             device chirps K; a full-speed one never leaves J)",
            port + 1
        );
        // `ehci_hub_control()` completes the reset from its GetPortStatus path:
        // it reads PORTSC, writes it back with the reset bit and the RWC bits
        // cleared (keeping PE exactly as read), then handshakes the reset bit to
        // zero. The controller does not clear PR on its own -- polling for that
        // leaves the port in reset forever, which reads as "connected, PR set,
        // never enabled".
        let in_reset = self.regs.read32(offset);
        self.regs
            .write32(offset, in_reset & !(portsc::CHANGE_BITS | portsc::RST));
        let mut value = in_reset;
        for _ in 0..200 {
            value = self.regs.read32(offset);
            if value & portsc::RST == 0 {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(5));
        }
        info!(
            "ehci: port {} after reset sequence, portsc {value:#010x} (pr {}, ped {}, ccs {}, \
             line {})",
            port + 1,
            value & portsc::RST != 0,
            value & portsc::PED != 0,
            value & portsc::CCS != 0,
            (value & portsc::LINE_MASK) >> 10,
        );
        // The device needs its reset recovery time before the first SETUP.
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(20_000));

        // Acknowledge the change bits this reset caused, keeping PED as the
        // controller set it.
        self.regs.write32(
            offset,
            (value & (portsc::PP | portsc::PED)) | portsc::CHANGE_BITS,
        );
        // A device that completed the high-speed chirp leaves the port in SE0
        // (line status 0); J-state means it stayed at full speed.
        let speed = match value & portsc::LINE_MASK {
            x if x == (1 << 10) => Speed::Low,
            x if x == (2 << 10) => Speed::Full,
            _ => Speed::High,
        };
        info!(
            "ehci: port {} reset, portsc {value:#010x} ped {} line {}",
            port + 1,
            value & portsc::PED != 0,
            (value & portsc::LINE_MASK) >> 10,
        );
        if value & portsc::PED == 0 {
            log::error!("ehci: port {} did not enable after reset", port + 1);
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
        match transfer {
            Err(KError::TimedOut) if self.recover_stalled_schedule() => {
                log::warn!("ehci: control transfer retried after an async-schedule restart");
                self.control_transfer_inner(address, setup, data, self.speed)
            }
            other => other,
        }
    }

    fn bulk_transfer(
        &mut self,
        address: u8,
        endpoint: u8,
        data: &mut [u8],
        is_in: bool,
    ) -> KResult<usize> {
        self.bulk_transfer_impl(address, endpoint, data, is_in)
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
                info!(
                    "ehci: usb2phy {block_base:#x} +0x0000 {:#06x} (clkout_ctl bit0 {})",
                    block.read32(0),
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
            for port in 0..ports {
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
                        {
                            let mut guard = ehci.lock();
                            super::probe_mass_storage(&mut *guard, &device);
                        }
                        if let Some(publication) = super::publish_mass_storage(dev, &ehci, &device)
                        {
                            disks.push(publication);
                        }
                    }
                    Err(err) => log::error!("ehci: port {} enumeration failed: {err:?}", port + 1),
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
