// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SCMI clock provider (Linux `drivers/clk/clk-scmi.c` +
//! `drivers/firmware/arm_scmi/smc.c`, folded into one driver file).
//!
//! On RK3588 the SD controllers' clocks live behind the SCMI agent in
//! EL3 (`<&scmi_clk SCMI_CCLK_SD>` / `SCMI_HCLK_SD`), so this driver is
//! the platform's path to `dev.res().clk("ciu")`. The transport is the
//! SMC dispatch over one SMT shared-memory channel; the protocol is the
//! SCMI clock domain (protocol 0x14).
//!
//! The board's SMC function id and SMT channel address are board facts
//! the kernel cannot discover from the protocol node itself, so they
//! arrive as Kconfig constants (`SCMI_SMC_ID`, `SCMI_SHMEM_PADDR`) —
//! the same treatment `RTC_PADDR` gets. Unknown SCMI clock ids resolve
//! to `None`, leaving their consumers exactly where they were (a missing
//! provider); bring-up only needs CCLK_SD and HCLK_SD.
//!
//! `set_rate` is deliberately `NotSupported`: the consumers this slice
//! drives never reprogram the upstream rate (the DW controller divides
//! internally), and a silent rate change behind EL3 is not a thing to
//! bluff about.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use driver_base::{
    Device, DriverOps, IoMap, MmioRegion, OfMatch, PlatformBus, PlatformId, ProbeError,
    RuntimeError,
};
use karch::smc_call;
use kclk::{ClkBuilder, ClkCore, ClkError, ClkHwOps, ClkLookup, ClkResult, ProviderId, Rate};

/// `static struct scmi_clock_match_table` — matches the generic
/// clock-provider identity the enumeration synthesizes for naked
/// `#clock-cells` nodes (the SCMI `protocol@14` child).
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("clock-provider"))];

/// SCMI base protocol success code.
const SCMI_SUCCESS: i32 = 0;
/// SCMI clock protocol id (`protocol@14`).
const SCMI_PROTOCOL_CLOCK: u32 = 0x14;
/// SMT channel-status free bit (channel may accept a message).
const SMT_STATUS_FREE: u32 = 1;
/// SCMI message header fields: message id [7:0], type [10:8], protocol
/// [17:10]; type 0 = command.
const fn scmi_header(msg_id: u32, protocol: u32) -> u32 {
    (msg_id & 0xFF) | ((protocol & 0x3FF) << 10)
}
/// SCMI clock-protocol message ids (firmware table).
const CLOCK_ATTRIBUTES: u32 = 0x3;
const CLOCK_RATE_GET: u32 = 0x4;
const CLOCK_ENABLE: u32 = 0x6;
const CLOCK_DISABLE: u32 = 0x7;
/// The SMT channel is one page.
const SMT_CHANNEL_BYTES: usize = 0x100;
/// SMT payload begins after the 28-byte channel header.
const SMT_MSG_HEADER: usize = 0x18;
const SMT_MSG_LENGTH: usize = 0x14;
const SMT_CHANNEL_STATUS: usize = 0x04;
const SMT_PAYLOAD: usize = 0x1C;
/// The known RK3588 SCMI clock ids (`dt-bindings/clock/rockchip,rk3588-cru.h`).
const SCMI_CCLK_SD: u32 = 9;
const SCMI_HCLK_SD: u32 = 23;
/// The SMT channel page/offset split (Linux `SHMEM_PAGE`/`SHMEM_OFFSET`):
/// TF-A resolves the channel from `X1 = addr >> 12`, `X2 = addr & 0xFFF`.
const SHMEM_SHIFT: u32 = 12;
const fn shmem_page(addr: u64) -> u32 {
    (addr >> SHMEM_SHIFT) as u32
}
const fn shmem_offset(addr: u64) -> u32 {
    (addr & 0xFFF) as u32
}

/// One SMC-dispatched SMT channel (Linux `struct scmi_smc`).
///
/// The SMC argument contract is Linux `arm_scmi/transports/smc.c`:
/// `X1 = shmem page number (addr >> 12)`, `X2 = offset within the page`,
/// `X3 = 0` — TF-A's handler resolves the channel from the page/offset
/// pair, NOT from a raw address.
struct ScmiChannel {
    shmem: IoMap,
    smc_id: u32,
}

impl ScmiChannel {
    /// A transaction: wait FREE → pack → SMC → wait FREE → unpack.
    ///
    /// `out_words` is the protocol-declared response payload length after
    /// the status word (RATE_GET carries two — 64-bit rate; commands with
    /// an empty return carry zero). The SMC is a synchronous fast call —
    /// the channel is free again by the time it returns; the polls are
    /// belt-and-braces bounded loops.
    fn transact(&self, msg_id: u32, args: &[u32], out_words: usize) -> Result<Vec<u32>, ScmiError> {
        self.wait_free()?;
        self.shmem
            .write32(SMT_MSG_HEADER, scmi_header(msg_id, SCMI_PROTOCOL_CLOCK));
        self.shmem.write32(SMT_MSG_LENGTH, (args.len() * 4) as u32);
        for (i, word) in args.iter().enumerate() {
            self.shmem.write32(SMT_PAYLOAD + i * 4, *word);
        }

        // The SMT channel is device-shared memory: make the message
        // stores visible to EL3 before ringing the doorbell.
        karch::barrier::dsb_ishst();
        let base = self.shmem.region().base as u64;
        let ret = smc_call(
            u64::from(self.smc_id),
            u64::from(SHMEM_PAGE(base)),
            u64::from(SHMEM_OFFSET(base)),
            0,
        );
        if ret[0] != 0 {
            // Only SMCCC_SUCCESS is a valid doorbell return (Linux maps
            // anything else to -EOPNOTSUPP).
            return Err(ScmiError::Smccc(ret[0]));
        }

        self.wait_free()?;
        let status = self.shmem.read32(SMT_PAYLOAD) as i32;
        if status != SCMI_SUCCESS {
            return Err(ScmiError::Firmware(status));
        }
        let mut out = Vec::new();
        for i in 1..=out_words {
            out.push(self.shmem.read32(SMT_PAYLOAD + i * 4));
        }
        Ok(out)
    }

    fn wait_free(&self) -> Result<(), ScmiError> {
        for _ in 0..(1 << 16) {
            if self.shmem.read32(SMT_CHANNEL_STATUS) & SMT_STATUS_FREE != 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(ScmiError::ChannelBusy)
    }

    /// CLOCK_RATE_GET → 64-bit rate.
    fn rate_get(&self, clock_id: u32) -> ClkResult<Rate> {
        let out = self
            .transact(CLOCK_RATE_GET, &[clock_id], 2)
            .map_err(|e| e.into_clk_error("rate_get"))?;
        let lo = out.first().copied().ok_or(ClkError::Io)?;
        let hi = out.get(1).copied().unwrap_or(0);
        Ok((u64::from(hi) << 32) | u64::from(lo))
    }

    fn enable(&self, clock_id: u32) -> ClkResult<()> {
        self.transact(CLOCK_ENABLE, &[clock_id], 0)
            .map_err(|e| e.into_clk_error("enable"))?;
        Ok(())
    }

    fn disable(&self, clock_id: u32) {
        let _ = self.transact(CLOCK_DISABLE, &[clock_id], 0);
    }

    /// CLOCK_ATTRIBUTES: proves the agent serves the clock domain.
    fn attributes(&self, clock_id: u32) -> Result<(), ScmiError> {
        self.transact(CLOCK_ATTRIBUTES, &[clock_id], 0)?;
        Ok(())
    }
}

/// Transport-level failures (Linux `-ETIMEDOUT`/`-EPROTO` mapping).
enum ScmiError {
    ChannelBusy,
    /// The agent returned a non-SUCCESS SCMI status code.
    Firmware(i32),
    /// The SMCCC doorbell itself returned a non-zero code.
    Smccc(u64),
}

impl ScmiError {
    /// Log-and-map: the status code is diagnostic-only.
    fn into_clk_error(self, what: &str) -> ClkError {
        match self {
            ScmiError::ChannelBusy => {
                log::warn!("scmi-clk: {what}: channel stuck busy");
                ClkError::Busy
            }
            ScmiError::Firmware(code) => {
                log::warn!("scmi-clk: {what}: firmware status {code}");
                ClkError::Io
            }
            ScmiError::Smccc(code) => {
                log::warn!("scmi-clk: {what}: smccc returned {code:#x}");
                ClkError::Io
            }
        }
    }
}

/// kclk ops over one SCMI clock id (Linux `scmi_clk_ops`, minus set_rate).
struct ScmiClkOps {
    channel: Arc<ScmiChannel>,
    clock_id: u32,
}

impl ClkHwOps for ScmiClkOps {
    fn recalc_rate(&self, _parent_rate: Rate) -> ClkResult<Rate> {
        self.channel.rate_get(self.clock_id)
    }

    fn enable(&self) -> ClkResult<()> {
        self.channel.enable(self.clock_id)
    }

    fn disable(&self) {
        self.channel.disable(self.clock_id);
    }
}

/// One published output: id → name, with the ops behind an `Arc<ClkCore>`.
struct ScmiClockOutput {
    id: u32,
    name: &'static str,
    core: Arc<ClkCore>,
}

/// The provider lookup (Linux `of_clk_add_hw_provider` registration).
struct ScmiClockLookup {
    provider_id: ProviderId,
    outputs: Vec<ScmiClockOutput>,
}

impl ClkLookup for ScmiClockLookup {
    fn provider_id(&self) -> ProviderId {
        self.provider_id
    }

    fn get_by_index(&self, index: u32) -> Option<Arc<ClkCore>> {
        self.outputs
            .iter()
            .find(|o| o.id == index)
            .map(|o| o.core.clone())
    }

    fn get_by_name(&self, name: &str) -> Option<Arc<ClkCore>> {
        self.outputs
            .iter()
            .find(|o| o.name == name)
            .map(|o| o.core.clone())
    }
}

/// Driver-private data: the channel plus the boot-static lookup.
pub struct ScmiClockData {
    _channel: Arc<ScmiChannel>,
    // Intentionally leaked with the lookup — clock providers live for the
    // kernel's lifetime (see `fixed_clock` for the model note).
    _lookup: &'static ScmiClockLookup,
}

/// The SCMI clock provider driver.
pub struct ScmiClockDriver;

impl DriverOps<PlatformBus> for ScmiClockDriver {
    type Data = ScmiClockData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<ScmiClockData, ProbeError> {
        // Only the clock protocol (protocol@14, reg 0x14) is ours; the
        // reset/other protocol children stay unclaimed.
        let protocol = dev.fw().u32("reg").unwrap_or(u32::MAX);
        if protocol != SCMI_PROTOCOL_CLOCK {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }
        let phandle = dev
            .fw()
            .node()
            .and_then(|node| node.phandle)
            .ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;

        let shmem: IoMap = dev
            .res()
            .iomap_region(
                MmioRegion::new(kbuild_config::SCMI_SHMEM_PADDR, SMT_CHANNEL_BYTES),
                "scmi-shmem",
            )
            .map_err(|_| {
                log::warn!(
                    "scmi-clk: shmem mapping failed at {:#x}",
                    kbuild_config::SCMI_SHMEM_PADDR
                );
                ProbeError::Fatal(RuntimeError::Invalid)
            })?;
        let channel = Arc::new(ScmiChannel {
            shmem,
            // The generated constant is `usize`; SMCCC ids are 32-bit.
            smc_id: u32::try_from(kbuild_config::SCMI_SMC_ID)
                .map_err(|_| ProbeError::Fatal(RuntimeError::Invalid))?,
        });

        // The agent must answer before anything parks on this provider.
        channel.attributes(SCMI_CCLK_SD).map_err(|err| {
            log::warn!(
                "scmi-clk: CLOCK_ATTRIBUTES probe failed: {err:?} (smc {:#x}, shmem {:#x})",
                kbuild_config::SCMI_SMC_ID,
                kbuild_config::SCMI_SHMEM_PADDR
            );
            ProbeError::Fatal(RuntimeError::Unsupported)
        })?;

        let known: [(&'static str, u32); 2] = [
            ("scmi_cclk_sd", SCMI_CCLK_SD),
            ("scmi_hclk_sd", SCMI_HCLK_SD),
        ];
        let mut outputs = Vec::new();
        for (name, id) in known {
            let ops = ScmiClkOps {
                channel: channel.clone(),
                clock_id: id,
            };
            let core = ClkBuilder::new(name, Arc::new(ops))
                .build()
                .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
            outputs.push(ScmiClockOutput { id, name, core });
        }

        // Publish LAST: the registration wakes consumers parked on this
        // phandle and cannot be rolled back by a later `Err`.
        let lookup: &'static ScmiClockLookup = Box::leak(Box::new(ScmiClockLookup {
            provider_id: phandle,
            outputs,
        }));
        dev.env()
            .provide::<driver_host::ClkProvider>(phandle, lookup)?;

        log::info!(
            "scmi-clk: published phandle {phandle:#x} (cclk_sd, hclk_sd) over smc {:#x}",
            kbuild_config::SCMI_SMC_ID
        );
        Ok(ScmiClockData {
            _channel: channel,
            _lookup: lookup,
        })
    }
}

driver_base::declare_driver!(PlatformBus, ScmiClockDriver, "scmi-clock", IDS);
