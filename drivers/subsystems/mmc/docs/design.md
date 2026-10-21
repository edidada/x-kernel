# subsys-mmc — Design

## Purpose and scope

The MMC/SD subsystem: the Linux `drivers/mmc/core` analogue, split per the
L0-b rule into a host-free contract crate (`subsys-mmc`) and a host-side
service crate (`subsys-mmc-host`). It defines the per-slot host contract,
owns a first-class card bus, implements the SD protocol state machine, and
publishes discovered cards as block devices.

## Module map

| Module | Linux analogue | Content |
| --- | --- | --- |
| `lib.rs` | `core/host.h`, `mmc_ops.c` | `MmcHost` trait, command/response/IOS types, `read_blocks`/`write_blocks` helpers |
| `bus.rs` | `core/bus.c` (`mmc_bus_type`) | `MmcBus` (`impl Bus`), `MmcCardData`, `MmcIdEntry` |
| `sd.rs` | `core/sd.c` + `core/sd_ops.c` | `detect_card` init sequence, CSD v1/v2 + SCR decode, HS switch |
| `blk.rs` | `core/block.c` (`mmc_blk`) | card-bus consumer → `Gendisk` into the block class |
| `xfer.rs` | `core/core.c` completion | `XferSignal` contract + `XferStatus` slot (contract half only) |

## The card bus

Cards are real devices, not private state of the host driver: the host adds
them to the domain from its own probe (`mmc_add_card`), parented to the host
device so host removal cascades the card. `MmcBus::ID` is `"mmc"`;
`MmcCardData` carries the protocol identity (RCA, CID, geometry, addressing
mode) plus the `Arc<dyn MmcHost>` handle every consumer needs. Matching is
by card kind (`MmcIdEntry`).

## Data transfer contract

`MmcHost::read_data_cmd`/`write_data_cmd` take a **byte length**, not a
block count: the SCR read is 8 bytes and the switch-function reply is 64,
neither of which is a 512-multiple. Block I/O is expressed through the
free helpers — CMD17 for one block, CMD18 + auto-stop for multi-block
reads, per-block CMD24 for writes (multi-block writes + pre-erase
bookkeeping are a deliberate follow-up), with `poll_card_ready` (CMD13)
between writes.

Completion is a two-half design: `XferStatus` (this crate, lock-free
atomics) is published by the host's IRQ handler, and the waiter parks
through `XferSignal`. The task-context implementation lives in
`subsys-mmc-host` (`TaskXferSignal`, the `subsys-block-host` split); probe
context falls back to a bounded poll when `wait_until` reports
`InvalidInput`.

## Scope cuts (deliberate)

- SD memory cards only (no eMMC `mmc.c` state machine, no SDIO bus).
- Default speed (25 MHz) and high speed (50 MHz, CMD6); no UHS-I — the
  1.8 V switch and tuning need per-SoC PHY work.
- No partition scanning (one minor per disk), no runtime re-scan of an
  empty slot (a follow-up timer task would drive it).
