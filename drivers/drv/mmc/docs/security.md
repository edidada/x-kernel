# drv-mmc — Security

## Trust boundaries

| Boundary | Input | Handling |
| --- | --- | --- |
| Device tree → driver | `fifo-depth`, `max-frequency`, reg specs | `fifo-depth < 2` fails the probe; the card-clock ceiling is `min(max-frequency, MMC_MAX_CARD_HZ)`, 0 meaning "no cap" from that side; resources come only through the `Device` doors. |
| Card → driver | transfer geometry, R1 responses | BLKSIZ/BYTCNT are driver-computed from the caller's `bytes` (512-byte blocks, or the exact byte count below one block); FIFO accesses are bounded by `STATUS.FCNT` and the destination slice, one FIFO entry per access, so a bogus `FCNT` can over-copy an entry but never leave the buffer. |
| Card → driver | FIFO data width / IP revision | `HCON.HDATA_WIDTH` and `VERID` are read once at probe and only ever select between fixed access widths (16/32/64-bit) and two fixed register offsets; no value from the register file sizes an allocation or an index. |
| Driver → DMA engine | descriptor chain | The ring is a probe-owned 4 KiB coherent allocation written only by `idmac_prepare`, which bounds the chain by `IDMAC_RING_DESCS` and by the window length; a transfer larger than the window is refused there and split by the block consumer instead. Descriptor addresses are 32-bit, so the ring and window are rejected at probe when they land above 4 GiB. |
| DMA engine → CPU | bounce window | The window comes from `alloc_coherent`, which maps it `UNCACHED`; the driver therefore performs no cache maintenance and cannot be made to read stale lines through a device-written buffer. |
| Firmware → rate | `MMC_CIU_HZ`, `MMC_MAX_CARD_HZ` | Compile-time board facts, not runtime input; a zero CIU rate fails the probe instead of dividing by it. |

## Unsafe inventory

- `unsafe impl Sync/Send for DwMmcHost` — the slot state is MMIO through
  `IoMap` (bounds-checked accessors) plus lock-free atomics; justified at
  the impl site.
- The host is owned by an `Arc`, so card consumers' references cannot
  outlive it; the probe Data pins the host reference and the card devices
  are parented to the host device, so removal order is cascade-defined.
- Raw pointer writes into the descriptor ring (`core::ptr::write_bytes`,
  `write_volatile`) — one operation per block, each bounded by
  `IDMAC_RING_DESCS` against the ring allocation; and the two window slices
  (`from_raw_parts(_mut)`) bounded by `idmac_max_bytes`. A slot performs one
  transfer at a time, which is what keeps the ring single-writer.

## Failure behavior

Command timeouts/CRC errors map to `MmcError` and surface as probe `Fatal`
or block-I/O errors. A failed data phase runs the full unwedge sequence
(bounded waits on every step: abort pulse, DAT0 release, FIFO reset) before
returning, so a card parked in the data state cannot silently poison later
transfers. Diagnostics log register state and FIFO byte counters only —
never card data.

A failing DMA engine latches the slot to the FIFO-polled path (engine
stopped, routing returned to the FIFO) instead of failing every request, so
an engine that stops fetching degrades throughput rather than the root
filesystem.

Polling has a fixed budget per phase (`CMD_POLL_LIMIT`, `DATA_POLL_LIMIT`);
exhausting it reports a timeout instead of spinning forever. With the
interrupt line masked for good, a stuck controller cannot flood the CPU.