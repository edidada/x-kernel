# drv-mmc — Security

## Trust boundaries

| Boundary | Input | Handling |
| --- | --- | --- |
| Device tree → driver | `fifo-depth`, `max-frequency`, reg/irq/clock specs | `fifo-depth < 2` and a zero ciu rate fail the probe; resources come only through the `Device` doors. |
| Card → DMA engine | transfer geometry | BLKSIZ/BYTCNT and descriptor lengths are driver-computed from the caller's `bytes`, bounded by the 64 KiB window check before any descriptor is armed. |
| Firmware → rate | `ciu.get_rate()` | Validated non-zero and `u32`-range; zero aborts the probe rather than dividing by it. |

## Unsafe inventory

- `unsafe impl Sync/Send for DwMmcHost` — the slot state is MMIO through
  `IoMap` (bounds-checked accessors) plus lock-free atomics; justified at
  the impl site.
- Coherent staging slices (`coherent_slice`) — two call sites, each
  bounded by the `bytes > DMA_WINDOW_BYTES` guard against the allocation
  the host owns outright.
- The DMA mappings are owned by the host (`Arc`), so the card consumers'
  references cannot outlive them; the probe Data only pins the IRQ and
  the host reference, and removal order (IRQ first, cascade second) is
  documented on the Data struct.

## Failure behavior

Command timeouts/CRC errors map to `MmcError` and surface as probe `Fatal`
or block-I/O errors; a detection failure never leaves the IDMAC enabled
(`idmac_stop` runs on every exit path of a data transfer). Interrupt
flooding is bounded: the handler only samples masked bits and clears what
it read.
