# drv-mmc — Design

> Bring-up history, symptom→cause table and the diagnostic rules this driver
> ended up with: [`bringup-pitfalls.md`](bringup-pitfalls.md).

## Purpose and scope

MMC host controller drivers — the Linux `drivers/mmc/host` analogue. One
crate per family, one module per controller core plus SoC glue:

| Module | Linux analogue | Content |
| --- | --- | --- |
| `dw_mmc.rs` | `dw_mmc.c` + `dw_mmc.h` | DW MSHC register map, clock sequencing, FIFO-polled PIO data path, `MmcHost` impl |
| `dw_mmc_rockchip.rs` | `dw_mmc-rockchip.c` | platform probe: MMIO/IRQ doors, clock board facts, card detection, card-bus add |

## Probe flow (RK3588)

1. `iomap(0)` the controller window; read `fifo-depth` / `max-frequency`
   from the firmware snapshot.
2. Take the CIU input rate and the card-clock ceiling as **board facts**
   (`MMC_CIU_HZ`, `MMC_MAX_CARD_HZ`), not from the clock framework: on
   RK3588 the SD clock domain is served by the EL3 SCMI agent, whose clock
   protocol rejects `SCMI_CCLK_SD` (`HARDWARE_ERROR`), and the source-rate
   registers live in the Secure CRU (reading them at EL1 faults), so the
   driver never reprograms the source and never reads it.
3. `alloc_coherent` the descriptor ring and the bounce window; both are
   dropped (and the slot falls back to PIO) if they are not below 4 GiB.
4. `DwMmcHost::new`: power, all-reset, raw-status clear, line mask (the
   driver is poll-only), max timeout, IDMAC routing forced off, baseline
   FIFOTH, VERID-selected FIFO port, HCON FIFO width, CIU clock gated.
5. `sd::detect_card` through the subsystem core; on success the card is
   added to the domain on the mmc bus (`add_bus_instance` +
   `add_device`, parented to this device). No card is a clean outcome.

## Data path

Two mechanisms, one contract. The default is the controller's **internal DMA
engine** (IDMAC, Linux `dw_mci_idmac_*`); the FIFO-polled **PIO** path stays
as the fallback the slot latches to when the engine cannot run.

### IDMAC (default)

One 4 KiB descriptor ring plus one 64 KiB bounce window, both
`alloc_coherent` (mapped `UNCACHED`, so no per-transfer cache maintenance)
and both required to sit below 4 GiB because the descriptor carries 32-bit
addresses.

Order per data phase, and the order is load-bearing:

1. stage the bounce window (writes only) — the engine may fetch immediately;
2. program FIFOTH/BLKSIZ/BYTCNT/`CDTHRCTL=0`, clear `RINTSTS`;
3. arm the engine: `IDSTS` clear → `CTRL.DMA_RESET` + `BMOD.SWRESET` polled to
   self-clear → `DBADDR` → `CTRL |= USE_IDMAC|DMA_ENABLE` → `BMOD = FB|DE` →
   `PLDMND = 1`;
4. issue the data command;
5. poll `RINTSTS` (`DATA_OVER`, errors) *and* `IDSTS` (`RI`/`TI`, engine
   errors), then copy the window out for reads.

Arming before the command is not cosmetic: the card starts streaming as soon
as it accepts the command, so an engine reset after step 4 drops the first
block. `PLDMND` is what makes the engine fetch the first descriptor —
without it `IDSTS` never moves and the phase hangs with a clean status.

Descriptors are chained 4 KiB chunks (`OWN|CH|DIC` on non-final, `FS` first,
`LD` last), never crossing a 4 KiB boundary (Linux
`dw_mci_idmac_start_dma`). Completion is polled from `IDSTS` + `RINTSTS`
with `INTMASK = 0` and `CTRL.INT_ENABLE = 0` (the first IRQ attempt on this
board stormed because the level-triggered line stayed asserted on uncleared
summary bits) — but **`IDINTEN` stays enabled**: the `IDSTS` completion bits
do not latch with it cleared, which shows up as `DATA_OVER` arriving while
`RI`/`TI` never do and the CIU staying `DATA_STATE_MC_BUSY`.

Only transfers of at least one block go through the engine
(`IDMAC_MIN_BYTES`): the SCR (8 B) and switch-function (64 B) reads stay on
PIO, so card detection never depends on the DMA path.

FIFOTH uses the per-transfer burst geometry here (Linux
`dw_mci_adjust_fifoth`), because the engine's burst has to match the block
size. The thresholds follow the mechanism that moves *this* transfer, not the
slot's capability: a transfer smaller than `IDMAC_MIN_BYTES`, or one that
runs after the engine degraded, gets the PIO baseline (the CPU drains by
polling `FCNT`, so there is no burst to match). Mixing them cost a boot: the
8-byte SCR read passed with the DMA geometry and the 64-byte switch read
fails with it.

If the engine fails a transfer the slot **latches to PIO** (`idmac_ok`): the
engine is stopped, the card unwedged, and the same transfer is retried on the
FIFO path. A dead engine degrades throughput, never availability. Hosts
report their per-transfer cap through `MmcHost::max_transfer_bytes` so block
consumers split requests larger than the window.

### PIO (fallback)

The data phase is issued with `DATA_EXPECT` (plus `SEND_STOP` for multi-block
reads) and the loop polls `RINTSTS`, **error bits first** — a data error
never raises `DATA_OVER` — draining the FIFO `FCNT`-bounded until the
transfer completes, tail included.

Two properties of the FIFO are read from `HCON.HDATA_WIDTH` rather than
assumed: the access width (16/32/64-bit, Linux `dw_mci_pull_data16/32/64`)
and the unit of `STATUS.FCNT` (FIFO entries). Linux's `part_buf` logic is
not needed here because every transfer this core issues is a multiple of the
widest entry.

FIFOTH stays at the probe-time baseline in this mode: Linux's
`dw_mci_adjust_fifoth` returns early unless DMA is in use.

### Both

`RINTSTS` is cleared at the head of every data phase so no earlier phase's
`DATA_OVER`/error set can be mistaken for this one's.

Diagnostics always report the command's `RESP0` (R1) next to the controller
state: a card completes the exchange and *then* refuses the transfer
(illegal command, block-length or address error), and in that case it never
drives the data lines at all — the controller sees only a start-bit error
with zero bytes transferred, which is otherwise indistinguishable from a
dead data path.

## Recovery

Failure and phase-boundary recovery share one sequence
(`reset_data_path`), in databook order: pulse `CTRL.ABORT_READ_DATA` and
wait for the pulse to clear, wait for DAT0 (card busy) to release, reset
the FIFO, force-stop with CMD12, then clear raw status. The order is not
cosmetic — a FIFO reset plus CMD12 alone leaves `STATUS.DATA_STATE_MC_BUSY`
set and the next data phase completes instantly with an empty FIFO.

`MmcHost::recover` runs this only when the data state machine or DAT0 is
actually busy; at a healthy phase boundary it just clears raw status, so
no CMD12 is sent to a card in TRAN state.

## Clocking

Card clock = `MMC_CIU_HZ / (2 * CLKDIV)` (CLKDIV from the Linux
`dw_mci_setup_bus` math), so init 400 kHz, default 25 MHz and HS 50 MHz
all come out of the controller divider. Every `set_ios` request is clamped
to the card-clock ceiling — `min(max-frequency, MMC_MAX_CARD_HZ)` — which
is the bring-up knob for a board whose real source rate differs from the
`MMC_CIU_HZ` assumption.

The data bus width is capped the same way: `MmcHost::max_bus_width`
(Linux `MMC_CAP_4_BIT_DATA`, board fact `MMC_4BIT`) keeps the card in
1-bit mode when the slot's `DAT1..DAT3` cannot be driven. Without that
ceiling a slot with dead wide lines fails every data phase with
`RINTSTS.SBE` while commands stay perfectly healthy.

Because the source rate is unreadable, and because a card that accepts
commands but never drives the bus looks identical whether the clock is too
fast or the wide lines are dead, the probe verifies **a block read** and then
walks a ladder of `(width, ceiling)` pairs (`MMC_CLOCK_LADDER`: 4-bit at the
configured ceiling, halved ceilings down to 4 MHz, then the same at 1-bit).
It logs the pair that worked, which is the measurement the divider math was
missing; pin `MMC_MAX_CARD_HZ`/`MMC_4BIT` to it and turn the ladder off.

UHS-I is out of scope, so the sample/drive phase clocks
(`SCLK_SDMMC_DRV/SAMPLE`) are untouched. That is a deliberate gap with a
known cost: Linux programs the drive phase (90°) and the sample phase on
every `set_ios` because the phases are what make the bus enumerable, and
nothing on the board re-programs them after the boot stages.