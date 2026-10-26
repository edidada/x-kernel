# drv-mmc — Design

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
3. `DwMmcHost::new`: power, all-reset, raw-status clear, line mask (the
   driver is poll-only), max timeout, IDMAC routing forced off, baseline
   FIFOTH, VERID-selected FIFO port, HCON FIFO width, CIU clock gated.
4. `sd::detect_card` through the subsystem core; on success the card is
   added to the domain on the mmc bus (`add_bus_instance` +
   `add_device`, parented to this device). No card is a clean outcome.

## Data path

FIFO-polled PIO. The data phase is issued with `DATA_EXPECT` (plus
`SEND_STOP` for multi-block reads) and the loop polls `RINTSTS`, **error
bits first** — a data error never raises `DATA_OVER` — draining the FIFO
`FCNT`-bounded until the transfer completes, tail included.

Two properties of the FIFO are read from `HCON.HDATA_WIDTH` rather than
assumed: the access width (16/32/64-bit, Linux `dw_mci_pull_data16/32/64`)
and the unit of `STATUS.FCNT` (FIFO entries). Linux's `part_buf` logic is
not needed here because every transfer this core issues is a multiple of
the widest entry.

FIFOTH stays at the probe-time baseline: Linux's per-transfer
`dw_mci_adjust_fifoth` returns early unless DMA is in use, so it is a
DMA-burst concern, not a PIO one.

`RINTSTS` is cleared at the head of every data phase so no earlier phase's
`DATA_OVER`/error set can be mistaken for this one's.

Diagnostics always report the command's `RESP0` (R1) next to the
controller state: a card completes the exchange and *then* refuses the
transfer (illegal command, block-length or address error), and in that
case it never drives the data lines at all — the controller sees only a
start-bit error with zero bytes transferred, which is otherwise
indistinguishable from a dead data path.

IDMAC (internal DMA) is a documented follow-up: the tgoskits reference
(`drivers/blk/dwmmc-host`) is IDMAC-only and carries no FIFO fallback, so
re-introducing it should port that path rather than re-derive it here.

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

UHS-I is out of scope, so the sample/drive phase clocks
(`SCLK_SDMMC_DRV/SAMPLE`) are untouched. That is a deliberate gap with a
known cost: Linux programs the drive phase (90°) and the sample phase on
every `set_ios` because the phases are what make the bus enumerable, and
nothing on the board re-programs them after the boot stages.