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
const POLL_STEPS: usize = 200_000;

/// Room for the setup packet plus one data buffer.
const EP0_BUFFER_LEN: usize = 4096;

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
    /// Speed of the device currently addressed. Enumeration handles one port at
    /// a time, so one value is enough until a hub driver exists.
    speed: Speed,
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
        // No periodic schedule (all-zero base means "disabled" once PSE is off).
        regs.write32(operational + op::PERIODICLISTBASE, 0);

        let qh = dev.res().alloc_coherent(core::mem::size_of::<Qh>(), 64)?;
        let qtds = dev
            .res()
            .alloc_coherent(3 * core::mem::size_of::<Qtd>(), 32)?;
        let buffers = dev.res().alloc_coherent(EP0_BUFFER_LEN, EP0_BUFFER_LEN)?;
        for (pointer, length) in [
            (qh.as_ptr(), core::mem::size_of::<Qh>()),
            (qtds.as_ptr(), 3 * core::mem::size_of::<Qtd>()),
            (buffers.as_ptr(), EP0_BUFFER_LEN),
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
        // SAFETY: `head` is the one QH in this allocation.
        unsafe {
            core::ptr::write_volatile(head, Qh::default());
        }
        // SAFETY: dword 1 of the same QH.
        unsafe {
            core::ptr::write_volatile(info1_ptr, info1::HEAD);
        }
        // SAFETY: dword 0 of the same QH.
        unsafe {
            core::ptr::write_volatile(next_ptr, (qh.bus_addr() as u32) | 2);
        }
        regs.write32(operational + op::ASYNCLISTADDR, qh.bus_addr() as u32);
        // Run with the asynchronous schedule only.
        regs.write32(
            operational + op::USBCMD,
            regs.read32(operational + op::USBCMD)
                | crate::ehci::regs::cmd::RS
                | crate::ehci::regs::cmd::ASE,
        );
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
        let head = self.qh.as_ptr().cast::<Qh>();
        let eps = match speed {
            Speed::Low => regs::eps_low(),
            Speed::High => regs::eps_high(),
            _ => regs::eps_full(),
        };
        let max_packet = if speed == Speed::High { 64 } else { 8 };
        let qh = Qh {
            next: (self.qh.bus_addr() as u32) | 2,
            info1: info1::HEAD
                | info1::CONTROL_EP
                | ((max_packet as u32) << info1::MAX_PACKET_SHIFT)
                | eps
                | (address as u32 & info1::ADDRESS_MASK),
            info2: 0,
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

        for _ in 0..POLL_STEPS {
            // SAFETY: dword 6 of the QH is its overlay token, which the hardware
            // keeps up to date while the transfer runs.
            let token_ptr = head.cast::<u8>().wrapping_add(24).cast::<u32>();
            // SAFETY: `token_ptr` is dword 6 of the QH allocated above.
            let overlay = unsafe { core::ptr::read_volatile(token_ptr) };
            if overlay & token::ACTIVE == 0 {
                if overlay & token::HALT != 0 {
                    info!("ehci: transfer halted, token {overlay:#010x}");
                    return Err(KError::Io);
                }
                // STATUS_MASK: bytes still to transfer in the status stage.
                let residual = (overlay & token::LEN_MASK) >> token::LEN_SHIFT;
                let transferred = (length as u32).saturating_sub(residual) as usize;
                if is_in && transferred > 0 {
                    // SAFETY: the controller wrote into the data buffer.
                    let from = buffer_base.wrapping_add(64);
                    // SAFETY: the controller wrote `transferred` bytes there.
                    unsafe {
                        core::ptr::copy_nonoverlapping(from, data.as_mut_ptr(), transferred);
                    }
                }
                return Ok(transferred);
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
        }
        log::error!("ehci: control transfer timed out");
        Err(KError::TimedOut)
    }

    /// Builds the setup/data/status qTD chain and returns the first one's address.
    fn build_chain(&self, is_in: bool, length: usize, data_base: u32) -> u32 {
        let qtd_base = self.qtds.as_ptr().cast::<Qtd>();
        let qtd_bus = self.qtds.bus_addr() as u32;
        let qtd_stride = core::mem::size_of::<Qtd>() as u32;
        let mut index = 0usize;
        let mut push = |next_addr: u32, pid_code: u32, len: u32, dt: bool, ioc: bool| -> u32 {
            let qtd = Qtd {
                next: next_addr | regs::LIST_END,
                alt_next: regs::LIST_END,
                token: (pid_code << token::PID_SHIFT)
                    | (3 << token::CERR_SHIFT)
                    | (len << token::LEN_SHIFT)
                    | if dt { token::DT } else { 0 }
                    | if ioc { token::IOC } else { 0 }
                    | token::ACTIVE,
                buffer: [self.buffers.bus_addr() as u32, data_base, 0, 0, 0],
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
        // The chain is built back to front so each qTD knows its successor.
        let status_addr = push(0, if is_in { pid::OUT } else { pid::IN }, 0, true, true);
        let data_addr = if length > 0 {
            push(
                status_addr,
                if is_in { pid::IN } else { pid::OUT },
                length as u32,
                true,
                false,
            )
        } else {
            status_addr
        };
        push(data_addr, pid::SETUP, 8, false, false)
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
        self.control_transfer_inner(address, setup, data, self.speed)
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
        /// keeps the mapping alive for the controller's lifetime.
        _ehci: Ehci,
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

            let ports = Hcd::port_count(&ehci);
            let mut devices = 0usize;
            for port in 0..ports {
                let status = ehci
                    .port_status(port)
                    .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
                if !status.connected {
                    continue;
                }
                info!("ehci: port {} has a device", port + 1);
                let address = 1u8;
                match usb::enumerate::enumerate_port(&mut ehci, port, address) {
                    Ok(device) => {
                        info!("ehci: {}", device.summary());
                        devices += 1;
                    }
                    Err(err) => log::error!("ehci: port {} enumeration failed: {err:?}", port + 1),
                }
            }
            info!("ehci: enumeration found {devices} device(s)");

            Ok(EhciData {
                _ehci: ehci,
                _clocks: clocks,
                _phys: phys,
            })
        }
    }

    driver_base::declare_driver!(PlatformBus, EhciDriver, "ehci", IDS);
}
