# drv-mmc / mmcblk — RK3588 bring-up log

Everything below cost real time on the Orange Pi 5 Plus (RK3588, `mmc@fe2c0000`,
DW MSHC `VERID 0x270a`, `HCON 0x04c434c1`, 116.2 GiB SD 3.0 card). It is kept
because most of these failures are silent: they report as a generic `Io`, as
success with wrong data, or as a symptom two layers away from the cause.

Read it as "what the hardware said" → "what it actually meant".

## 1. A `& 0x1f` mask rewrote commands

`CMD.INDX` is **six** bits; bit 6 is `RESP_EXP`. Masking the opcode to five bits
turned ACMD41 (41) into CMD9 and CMD55 (55) into CMD23, so identification failed
with `RTO` on a card that was answering fine. Linux writes `cmdr = cmd->opcode`
verbatim.

**Tell:** commands below 32 work, the first `opcode >= 32` times out.

## 2. U-Boot's IDMAC routing survives a controller reset

`CTRL.USE_IDMAC`/`DMA_ENABLE` are configuration bits; the `DMA_RESET` pulse in
`CTRL` resets the engine but does not clear them. With them still set, the FIFO
feeds an unconfigured DMA engine: a PIO read never pops the FIFO, `DATA_OVER`
never fires, and **no error bit is raised**. Clear both at init (Linux does it in
`dw_mci_submit_data` when it enters PIO).

**Tell:** clock/commands/geometry all correct, data is flowing, FIFO stays full,
`DATA_OVER` never arrives, `RINTSTS` clean.

## 3. `RESP0` is the *least* significant word of a long response

For R2 (CID/CSD) the controller keeps `{31:0}` in `RESP0` … `{127:96}` in
`RESP3`; Linux stores them as `resp[3] = RESP0 … resp[0] = RESP3`. Filling the
`{127:96}-first` array in register order reverses every field. Nothing fails
loudly: the capacity comes out wrong (116.2 GiB → 1.75 GiB) and `CCC` decodes to
`0x0e0`, so `CCC_SWITCH` reads clear and the high-speed switch is **skipped on a
card that supports it** — which is also why the 4-bit/25 MHz data path was never
exercised and every later data failure was blamed on the wrong thing.

**Tell:** capacity does not match U-Boot's `mmc info`; `ccc` is not `0x5b5`/`0x5f5`.

## 4. `SBE` on a data phase usually means "the card never sent data"

`RINTSTS.SBE` (start-bit error) plus `TCBCNT == 0` means the CIU decoded no data
at all. Two very different causes produce it:

- the card **refused** the command and returned to TRAN without driving the data
  lines — its R1 says why (`ILLEGAL_COMMAND`, `BLOCK_LEN_ERROR`,
  `OUT_OF_RANGE`);
- the data path itself is unusable (wrong bus width, dead DAT lines).

They are only distinguishable by looking at **R1**, which the driver used to
ignore: `send_cmd` reported transport errors only, so a card that completed the
exchange and then refused the transfer looked exactly like a controller fault.
Every R1 with an error bit is now logged, and every failed data phase prints
`resp0` through the same decoder.

**Tell:** commands healthy (no `RCRC`/`RTO`), `SBE` + `TCBCNT=0` on the data
phase, clean `resp0` → the card accepted and sent nothing.

## 5. Do not use `STATUS.DATA_STATE_MC_BUSY` as "a transfer is in flight"

That bit stays asserted after a completed phase. Acting on it made phase-boundary
recovery send **CMD12 in TRAN**, which is an illegal command: the card answers
`R1.ILLEGAL_COMMAND`, and that answer then sits in `RESP0` and poisons the next
`resp0` reading — the observer can no longer tell "the card refused CMD17" from
"our own stray stop". Phase-boundary recovery is now pure controller hygiene
(abort pulse, DAT0 release, FIFO reset, status clear); the card-side stop only
happens on the error path, which knows a data phase was armed.

**Tell:** a data command's `resp0` reports `ILLEGAL_COMMAND` while
`CURRENT_STATE` says `tran` — in TRAN, CMD17/CMD18 are legal.

## 6. One gendisk needs one minor

Publishing one disk per partition with a fixed `first_minor = 0` works exactly
once: the second `add_disk` hits the minor-range conflict, the probe maps that to
`Fatal(Io)`, and because the probe failed the framework **withdraws the
publications that did succeed** — so the boot ends with "No block device found!"
even though the first partition was fine.

**Tell:** the first `mmcblk: published …` line is followed by `gave up on device
…: i/o error` and then no block device at all.

## 7. A partition's size comes from its entry, not from the card

Using `card_capacity - start_lba` makes every partition look like it runs to the
end of the card. Reads stay inside the mounted filesystem, so nothing breaks
immediately — but a write near the partition's real end lands in the **next**
partition. GPT's `last_lba` is inclusive, an MBR entry carries a sector count.

**Tell:** `start_lba + sectors` equals the card capacity for every partition.

## 8. "Lowest dev_t" is a preference, not the answer

Which partition carries the root filesystem is an image fact. Selecting the
lowest `dev_t` and panicking on the first failed mount stops the boot on the
board's 1 GiB boot partition (`InvalidMagic(0)`) while the root filesystem sits
on the next one. Follow Linux `mount_root_generic`: try the preferred device
first, then every remaining candidate.

## 9. The SD clock source is not readable, and `max-frequency` is not it

On RK3588 the SD clock domain lives behind the EL3 agent: the SCMI clock
protocol answers VERSION but rejects `CLOCK_ATTRIBUTES` for `SCMI_CCLK_SD`
(`SCMI_HARDWARE_ERROR`), and the Secure CRU faults when read from EL1. Linux
copes by *programming* the rate (`clk_set_rate(ciu, 2 * clock)`); a driver that
does not own the clock can only take the rate as a board fact
(`MMC_CIU_HZ`). The DT `max-frequency` is a different thing (the card-clock
ceiling) and must not be used as the source rate.

Consequence: when a data phase misbehaves, the divider arithmetic is a
prime suspect but unverifiable — so the ceiling is a knob
(`MMC_MAX_CARD_HZ`), and lowering it is how the source rate gets inferred
(a passing 12.5 MHz ceiling with a failing 25 MHz one means the effective rate
is twice `MMC_CIU_HZ`).

## 10. 4-bit and 25 MHz always changed together

Every data phase failed at the same point for the whole port, and "width" and
"clock" had never been varied independently. Both are now capabilities with a
lever: `MMC_4BIT` (`MmcHost::max_bus_width`, Linux `MMC_CAP_4_BIT_DATA`) and
`MMC_MAX_CARD_HZ`. One boot per lever is cheaper than any amount of reasoning
about which of the two it was.

## 11. The board-verified reference is IDMAC-only

`tgoskits/drivers/blk/dwmmc-host` removed its FIFO/PIO fallback; SD boot was
restored there through the IDMAC path. Citing it as "the board-verified PIO
shape" produced a PIO path that was validated against nothing, and then a
migration back to IDMAC that re-derived details the reference already had. For
PIO the reference is Linux `dw_mci.c`; for IDMAC it is the tgoskits driver.

Two Linux details that only matter in PIO:

- `dw_mci_adjust_fifoth` is DMA-only (`if (!host->use_dma) return;`) — the
  per-transfer FIFOTH geometry does not apply and the probe-time baseline is
  what PIO runs on;
- `STATUS.FCNT` counts FIFO **entries**, and the entry width is
  `HCON.HDATA_WIDTH` (16/32/64-bit), so the byte count is `FCNT << data_shift`
  and the FIFO access width must match (Linux `dw_mci_pull_data16/32/64`).

## 12. Auto-stop streams blocks back-to-back

`CMD18` + `SEND_STOP` removes the gap between blocks, so on a polled data path a
drain loop that loses the CPU long enough overruns the FIFO and the transfer
dies with a start-bit/data error that says nothing about the media. Per-block
`CMD17` leaves an empty FIFO and a command frame between blocks — which is why
the multi-block path is an optimisation with a latch: the first failure
downgrades the card to per-block reads for the rest of the session instead of
turning a scheduling hiccup into an I/O error.

## 13. `IDSTS` completion bits need `IDINTEN`, even when polling

The first polled IDMAC attempt timed out on the very first data phase (the
8-byte SCR read) with the CIU data machine still busy:

```
rintsts=0x2c [CMD_DONE|DATA_OVER]  tcbcnt=8 tbbcnt=8  status bit10 mc_busy=1
```

`DATA_OVER` had arrived and the bytes had crossed the BIU, but `IDSTS.RI/TI`
never latched, so the completion condition never held. Cause: `IDINTEN` was
written as 0 ("completion is polled"). The completion bits are gated by that
enable; both the reference driver and Linux keep `IDINTEN` enabled in IDMAC
mode and only mask the *interrupt line* (`CTRL.INT_ENABLE` = 0, `INTMASK` = 0)
when they poll. Two other facts from the same dump: the engine-side progress
pointers (`DSCADDR`, `BUFADDR`) belong in every IDMAC failure dump, and a
stalled engine must not be waited on with the PIO budget (that attempt spent
30 s before degrading).

## 14. FIFOTH belongs to the mechanism, not to the slot

`dw_mci_adjust_fifoth` is real for DMA and wrong for PIO, and "the slot has
IDMAC" is not the same question as "this transfer uses IDMAC": a 64-byte
register read stays on PIO even when the engine is enabled, and anything can
land on PIO after the engine degrades. Keying FIFOTH off the slot's
capability gave the PIO path a DMA burst geometry, and the board answered
with `SBE` + `TCBCNT=0` on the switch-function read that had worked the boot
before — a regression that looked like a hardware fault because the *only*
change was two extra register writes per data phase.

Corollary for diagnostics: dump the `RINTSTS` snapshot that triggered the
failure, not the register after `take_errors` cleared it. The first IDMAC
run printed `rintsts=0x4 [CMD_DONE]` for a phase that actually ended in an
error, which is worse than no dump at all.

## 15. One card, several disks, several tasks: the slot needs a guard

The board booted to a shell and then wedged the storage stack the moment user
space started real I/O:

```
CPU1: idmac timeout cmd 18  bytcnt=0x4000  tcbcnt=0  tbbcnt=2048
CPU7: idmac data error ... bmod=0x202  dscaddr=0x201030  tbbcnt=12416
```

Two tasks were inside data phases on the same slot at the same time. `bmod`
still showed `DE` set on one side while the other had already run
`IDMAC_INT_CLEAR`, stopped the engine and cleared the routing. Every register
the driver writes per phase — ring, `DBADDR`, `CTRL`, `BMOD`, `PLDMNT` — is
shared state with no ownership, and `mmcblk0p1`/`mmcblk0p2` are two disks over
one controller. Nothing above the driver serializes, so the driver must.

A single-user boot hid this completely: detection and the first mount are
sequential, so the corruption only appears once the shell runs jobs.

## 16. The switch read must not be the first wide, fast data phase

Diffing the init sequence against Linux `mmc_sd_init_card` showed the order was
inverted. Linux runs, in this order: SCR, **CMD6 (the 64-byte switch read) at
the identification clock on one data line**, then `mmc_set_clock` (25 MHz, or
50 MHz when the switch reported HS), then `SET_BLOCKLEN` for byte-addressed
cards, and only **last** `ACMD6` + the host's 4-bit width. This driver did the
width and the clock first and the switch read afterwards, so its *first*
4-bit/25 MHz data phase was the switch read itself — and that is exactly where
the board failed, every boot, with `SBE` and `TCBCNT=0`, while the same read
succeeded on a second full initialisation.

Putting the two changes at opposite ends of the sequence removes the corner
instead of retrying around it: the switch read happens on the slow, narrow bus
where it is uncritical, and the first wide/fast transfer is ordinary block
I/O. Confirmed on the board: the boot that carried this change reports
`data path ok at 4-bit / 25000000 Hz` on the first attempt, with no `cmd 6
`SBE`` and no retry — after four earlier hypotheses (clock ceiling, FIFO
geometry, `BMOD` reset, power cycling) each absorbed the symptom without
explaining it.

The lesson generalised: when a transfer fails right after a mode change,
compare *where* in the sequence Linux performs that transfer before suspecting
the mode itself. The retry mechanisms added while guessing (`LADDER_REPEATS`,
the clock ladder) stay only as safety nets, not as the fix.

## 17. Measure the clock, do not trust the board fact

`MMC_CIU_HZ` was a guess from the start (the source rate lives behind EL3), and
it was wrong by a factor of two: with the divider bypassed the driver believed
it programmed 50 MHz, while the card was running at ~25 MHz. Three throughput
measurements derived the truth without any timer:

| setting | measured |
| --- | --- |
| 4-bit, divider 0 (`programmed 50000000 Hz`) | 9.0 MB/s |
| 4-bit, divider 1 | 5.1 MB/s |
| PIO instead of IDMAC (same clock) | 9.0 MB/s |

Bytes-per-second is `512 / (1024/C + gap)` for 4-bit and `512 / (4096/C + gap)`
for 1-bit; the divider-0 and divider-1 runs give two equations, which solve to
`C ≈ 23.5 MHz` and a ~13 us per-block card delay. The PIO run rules out the DMA
bounce window as the cause, and a 1-bit run over-determines the model: it
predicted ~2.9 MB/s for C = 25 MHz (and ~5.4 for C = 50) and measured **2.7**.
With `MMC_CIU_HZ = 25 MHz` the programmed rates, the 400 kHz init divider and
any future high-speed request all agree with the hardware.

U-Boot's `mmc info` on the same board says `Bus Speed: 50000000` and
`Mode: SD High Speed (50MHz)`, which looks like a contradiction. It is not a
measurement: the bus speed is the rate U-Boot's own divider math requested
(the same assumption chain this driver had), and the mode name is the SD
spec's label taken from the switch-function reply. Two nominal numbers cannot
overrule a measurement.

Two things made this hard to see, and both were logging mistakes of mine:
`data path ok at ... 50000000 Hz` printed the *ceiling*, and
`sd: card bus high speed (50000000 Hz)` printed the protocol's *request*. Both
now print what the divider actually programmed.

## Diagnostic rules that came out of this

1. **A completed command is not a successful command.** Decode R1; log it when
   it carries an error bit.
2. **Every data-phase failure prints the controller state** (ctrl/status/
   rintsts/clkdiv/ctype/blksiz/bytcnt) plus `TCBCNT`/`TBBCNT` and `resp0`: the
   byte counters separate "the card sent nothing" from "the controller dropped
   it", and `resp0` separates "the card refused" from both.
3. **Never name `STATUS` bits with the `RINTSTS` vocabulary** — the layouts are
   unrelated, and a wrong label sends the next hour in the wrong direction.
4. **A diagnostic that cannot reach the hardware is worse than none** (the
   byte-size ladder was rejected by a buffer-length check before the first
   transfer, and "8 B failed" was reported for weeks).
5. **Compare against a known-good run of the same board** (U-Boot `mmc info`:
   50 MHz, 4-bit, 116.2 GiB). Every mismatch above was visible as a number that
   did not match that baseline.

## The measurement that unblocked it

Two unreadable board facts produced the same symptom (`SBE` + `TCBCNT=0`, card
in TRAN, command answered cleanly): a card clock faster than the divider math
assumes, and unusable wide data lines. Neither is observable from software, so
the probe measures instead of assuming: it verifies a block read and walks
`(width, ceiling)` pairs, logging the pair that works. On the bench that
printed `data path only works at 4-bit / 25000000 Hz (configured 4-bit /
50000000 Hz)` and the board booted — one boot instead of a rebuild per guess.

Two lessons from the same boot:

- **`IDSTS` silence is not an engine fault.** `idsts` had no error bit and
  `DSCADDR == DBADDR`, which reads as "the engine never fetched". It had
  nothing to fetch: the data phase itself died with `SBE`, so the card never
  drove the bus. One boot later, at the working pair, the same engine carried
  the self-test read with no complaint.
- **The first attempt can fail where the second succeeds** at an identical
  width and clock, which points at the switch into 4-bit mode being marginal
  rather than at any register value. The ladder absorbs it by re-running the
  whole initialisation; a retry of just the failing phase would not.

## Where the data path landed

IDMAC is the default (`MMC_IDMAC`), PIO is the fallback the slot latches to
when the engine allocation is unusable (not below 4 GiB) or the engine fails
a transfer. The engine was retried *after* items 2, 4 and 5 were understood:
its original failure evidence ("IDSTS all zero", `SBE`, `RTO`) was produced
while the CMD12-in-TRAN bug was still poisoning `resp0` and the command-index
mask was still wrong, so it never described the engine. Two facts made the
retry reviewable rather than hopeful: the descriptor-format constraint (32-bit
addresses) is checked at probe, and every engine failure degrades to a path
that is already known good on this board.
