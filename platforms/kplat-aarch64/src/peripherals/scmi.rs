// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SCMI clock provider (read-only rate query) over the AArch64 SMC transport.
//!
//! On RK3588 the SD-card CIU clock is owned by TF-A/SCMI firmware and exposed
//! through the `arm,scmi-smc` clock protocol (`firmware/scmi/protocol@14`,
//! phandle `0x0e`). Reading its rate lets the dw_mshc driver compute `CLKDIV`
//! from the true frequency instead of the 50 MHz fallback.
//!
//! The transport mirrors Linux's `arm_scmi` SMC path: frame a `CLOCK_RATE_GET`
//! message in the shared SRAM buffer, ring the `arm,smc-id` doorbell, and parse
//! the synchronous reply. Tier 0 implements rate query only — no `CLOCK_RATE_SET`,
//! no reset protocol.
//!
//! Everything here is platform-specific by nature: the DT binding is
//! `arm,scmi-smc`, the transport is an AArch64 SMC, and the shared buffer lives
//! in PMU SRAM. The neutral clock framework in `drivers/clk` only sees the
//! [`ClkProvider`] this module registers.
//!
//! # Bring-up status (Orange Pi 5 Plus)
//!
//! The SMC transport is correct — `base VERSION` returns `0x20000` (SCMI 2.0),
//! and the shared-memory framing matches Linux. But the board's secure firmware
//! (TF-A / bl31) only implements a generic VERSION responder: every other
//! command, including the spec-mandatory base `PROTOCOL_ATTRIBUTES` /
//! `MESSAGE_ATTRIBUTES` and clock `RATE_GET`, returns `PROTOCOL_ERROR (-10)`.
//! So `RATE_GET` never succeeds on this board and the sdmmc driver keeps its
//! 50 MHz fallback. The provider is left in place — it is correct and will work
//! unchanged on firmware that genuinely implements the SCMI clock protocol.
//! See `drivers/clk/docs/design.md` and
//! `docs/ai/skills/linux-driver-port/knowledge/clk-subsystem.md`.

use alloc::{sync::Arc, vec::Vec};

use clk::{ClkProvider, register_provider};
use kspin::SpinNoIrq;
use of::FdtNode;

use crate::peripherals::smccc;

// Offsets within the SCMI shared-memory area (ARM SCMI spec; matches Linux
// `struct scmi_shared_mem`: reserved, channel_status, reserved1[2], flags,
// length, msg_header, then the message payload).
const SHMEM_CHANNEL_STATUS: usize = 0x04;
const SHMEM_FLAGS: usize = 0x10;
const SHMEM_LENGTH: usize = 0x14;
const SHMEM_MSG_HEADER: usize = 0x18;
const SHMEM_PAYLOAD: usize = 0x1c;

// SCMI clock protocol messages.
const SCMI_PROTO_CLOCK: u32 = 0x14;
const SCMI_MSG_CLOCK_RATE_GET: u32 = 0x06;
const SCMI_STATUS_SUCCESS: u32 = 0;

/// Pack a SCMI message header (ARM SCMI spec / Linux `pack_scmi_header`):
/// `message_id[7:0] | message_type[9:8] | protocol_id[17:10] | token[27:18]`.
/// `message_type` is 0 (command) for a rate query.
const fn pack_header(protocol: u32, message: u32, token: u32) -> u32 {
    (message & 0xff) | ((protocol & 0xff) << 10) | ((token & 0x3ff) << 18)
}

/// Full-system barrier wrapping the SMC, matching the upstream Rust SCMI SMC
/// transport: guarantees every TX write is observed system-wide before the trap
/// and orders the subsequent response reads.
#[inline(always)]
fn full_system_barrier() {
    // SAFETY: `dsb sy` + `isb` are ordering barriers only — no memory access,
    // no register side effects the compiler tracks.
    unsafe {
        core::arch::asm!("dsb sy", "isb", options(nostack, preserves_flags));
    }
}

/// A clock provider backed by the SCMI SMC transport.
///
/// One shared SRAM buffer serves every clock of the protocol, so concurrent
/// queries are serialised by a spinlock held across the (synchronous) SMC.
pub struct ScmiClockProvider {
    /// SMC function id used as the doorbell (`arm,smc-id`).
    func_id: u32,
    /// Kernel virtual base of the mapped SCMI shared memory.
    shmem: usize,
    lock: SpinNoIrq<()>,
}

impl ScmiClockProvider {
    pub const fn new(func_id: u32, shmem: usize) -> Self {
        Self {
            func_id,
            shmem,
            lock: SpinNoIrq::new(()),
        }
    }

    /// Write a little-endian u32 to a shared-memory field.
    ///
    /// # Safety
    ///
    /// `self.shmem` must map the SCMI shared-memory region (see [`Self::new`]),
    /// and `off` must be a 4-byte-aligned offset within it. Both hold for the
    /// constants above after [`init_clock_provider`] maps the buffer.
    unsafe fn write32(&self, off: usize, val: u32) {
        ((self.shmem + off) as *mut u32).write_volatile(val.to_le());
    }

    /// Read a little-endian u32 from a shared-memory field.
    ///
    /// # Safety
    ///
    /// Same contract as [`Self::write32`].
    unsafe fn read32(&self, off: usize) -> u32 {
        u32::from_le(((self.shmem + off) as *const u32).read_volatile())
    }

    /// Send a SCMI command and return the reply payload words (after the status
    /// word) on success. Returns `None` (with a `log::warn!`) on transport or
    /// protocol error.
    fn xfer(&self, protocol: u32, message: u32, request: &[u32]) -> Option<Vec<u32>> {
        let _channel = self.lock.lock();

        // Frame the command. The header write is done last: it is the commit the
        // platform observes, so payload and length must be in place first.
        // SAFETY: `shmem` maps the SCMI buffer and these offsets are aligned
        // fields within it (see `write32`).
        unsafe {
            self.write32(SHMEM_CHANNEL_STATUS, 0);
            self.write32(SHMEM_FLAGS, 0);
            for (index, &word) in request.iter().enumerate() {
                self.write32(SHMEM_PAYLOAD + index * 4, word);
            }
            self.write32(SHMEM_LENGTH, 4 + (request.len() * 4) as u32); // header + payload
            self.write32(SHMEM_MSG_HEADER, pack_header(protocol, message, 0));
        }

        // Ring the doorbell. The SMC is synchronous: TF-A processes the message
        // and writes the reply before returning. Full barriers on both sides
        // (matching Linux / the upstream Rust SCMI stack) make every TX write
        // visible to the monitor before the trap and order the response reads.
        full_system_barrier();
        let result = smccc::smc_call(self.func_id, 0, 0, 0);
        full_system_barrier();

        // SAFETY: read path of the mapped buffer (see `read32`).
        let status = unsafe { self.read32(SHMEM_PAYLOAD) };
        if result.x0 != 0 {
            log::warn!(
                "scmi-clk: command (proto={:#x}, msg={:#x}) transport error a0={:#x}",
                protocol,
                message,
                result.x0
            );
            return None;
        }
        if status != SCMI_STATUS_SUCCESS {
            log::warn!(
                "scmi-clk: command (proto={:#x}, msg={:#x}) status={:#x}",
                protocol,
                message,
                status
            );
            return None;
        }

        // Reply payload (after the status word): (length - header - status) / 4.
        // SAFETY: read path of the mapped buffer.
        let length = unsafe { self.read32(SHMEM_LENGTH) };
        let words = (length as usize).saturating_sub(8) / 4;
        let mut reply = Vec::with_capacity(words);
        for index in 0..words {
            // SAFETY: mapped buffer; these offsets follow the status word.
            reply.push(unsafe { self.read32(SHMEM_PAYLOAD + 4 + index * 4) });
        }
        Some(reply)
    }
}

impl ClkProvider for ScmiClockProvider {
    fn get_rate(&self, specifier: &[u32]) -> Option<u64> {
        let &clock_id = specifier.first()?;
        let reply = self.xfer(SCMI_PROTO_CLOCK, SCMI_MSG_CLOCK_RATE_GET, &[clock_id])?;
        let rate_lo = reply.first().copied().unwrap_or(0) as u64;
        let rate_hi = reply.get(1).copied().unwrap_or(0) as u64;
        Some((rate_hi << 32) | rate_lo)
    }
}

struct ShmemRegion {
    base: usize,
    size: usize,
}

/// Resolve the SCMI shared-memory physical location from its DT phandle.
///
/// The `arm,scmi-shmem` node's `reg` is an offset within its `mmio-sram` parent
/// (PMU SRAM on RK3588, a flat 1:1-mapped region); the absolute address is the
/// parent's base plus that offset.
fn shmem_phys_region(shmem_phandle: u32) -> Option<ShmemRegion> {
    let node = of::find_node_by_phandle(shmem_phandle)?;
    let offset = node.reg()?.next()?;
    let parent = of::find_compatible("mmio-sram")?.reg()?.next()?;
    Some(ShmemRegion {
        base: parent.starting_address as usize + offset.starting_address as usize,
        size: offset.size,
    })
}

/// Whether a node's direct parent carries `compatible = "arm,scmi-smc"`.
fn parent_is_scmi_smc(node: FdtNode<'static, 'static>) -> bool {
    node.parent_property("compatible").is_some_and(|prop| {
        prop.value
            .split(|&byte| byte == 0)
            .next()
            .is_some_and(|first| first == b"arm,scmi-smc")
    })
}

/// Locate the SCMI clock protocol node structurally: the child of an
/// `arm,scmi-smc` node that carries `#clock-cells`. This avoids depending on
/// the `scmi_clk` alias or a hardcoded DT path, both of which are fragile.
fn find_scmi_clock_protocol() -> Option<FdtNode<'static, 'static>> {
    let fdt = of::fdt()?;
    fdt.all_nodes()
        .find(|node| node.property_u32("#clock-cells").is_some() && parent_is_scmi_smc(*node))
}

/// Probe the device tree for the SCMI clock protocol and register a rate-query
/// provider for it.
///
/// No-op (debug log) on platforms without an SCMI clock node, so this is safe to
/// call unconditionally from platform init.
pub fn init_clock_provider() {
    // Runs in `early_driver_init`, before the runtime logger is up, so use
    // `bootln!` (boot console) for diagnostics.
    kernel_boot::bootln!("scmi-clk: probing DT for SCMI clock protocol");

    let Some(protocol) = find_scmi_clock_protocol() else {
        kernel_boot::bootln!("scmi-clk: no SCMI clock protocol in DT; skipping");
        return;
    };

    // The provider's OS-neutral name is the protocol node's name with the
    // `@unit` suffix stripped; the bus layer derives the same key when it
    // builds `ClkDesc` from a consumer's `clocks = <&scmi_clk N>`.
    let provider_name = of::node_name_key(protocol.name);

    // Transport parameters live on the parent `arm,scmi-smc` node.
    let Some(func_id) = protocol.parent_property_u32("arm,smc-id") else {
        kernel_boot::bootln!("scmi-clk: missing arm,smc-id; skipping");
        return;
    };
    let Some(shmem_phandle) = protocol.parent_property_u32("shmem") else {
        kernel_boot::bootln!("scmi-clk: missing shmem; skipping");
        return;
    };

    let Some(region) = shmem_phys_region(shmem_phandle) else {
        kernel_boot::bootln!(
            "scmi-clk: cannot resolve shmem phandle {:#x}; skipping",
            shmem_phandle
        );
        return;
    };

    let vaddr = match memspace::iomap_device(region.base.into(), region.size, "scmi-shmem") {
        Ok(vaddr) => vaddr,
        Err(err) => {
            kernel_boot::bootln!(
                "scmi-clk: failed to map shmem @ {:#x} ({} B): {:?}; skipping",
                region.base,
                region.size,
                err
            );
            return;
        }
    };

    // Zero the shared-memory buffer so no stale platform state interferes with
    // the first message.
    let shmem_base = vaddr.as_usize();
    for offset in (0..region.size).step_by(4) {
        // SAFETY: `shmem_base` maps the full `region.size` SCMI buffer and every
        // `offset` steps by 4 bytes within it.
        unsafe { ((shmem_base + offset) as *mut u32).write_volatile(0) };
    }

    register_provider(
        provider_name,
        Arc::new(ScmiClockProvider::new(func_id, shmem_base)),
    );
    kernel_boot::bootln!(
        "scmi-clk: registered provider name={:?} doorbell={:#x} shmem={:#x}",
        provider_name,
        func_id,
        region.base
    );
}
