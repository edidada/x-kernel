# drv-mmc — Design

## Purpose and scope

MMC host controller drivers — the Linux `drivers/mmc/host` analogue. One
crate per family, one module per controller core plus SoC glue:

| Module | Linux analogue | Content |
| --- | --- | --- |
| `dw_mmc.rs` | `dw_mmc.c` + `dw_mmc.h` | DW MSHC register map, clock sequencing, IDMAC engine, `MmcHost` impl |
| `dw_mmc_rockchip.rs` | `dw_mmc-rockchip.c` | platform probe: clocks/IRQ/DMA doors, card detection, card-bus add |

## Probe flow (RK3588)

1. `iomap(0)` the controller window; read `fifo-depth` / `max-frequency`
   from the firmware snapshot.
2. `clk("biu")` / `clk("ciu")` through the framework — on RK3588 these are
   `SCMI_HCLK_SD` / `SCMI_CCLK_SD`, served by the SCMI clock provider
   (`drv/clk/scmi_clock`); a missing provider defers the probe, and the
   wakeup loop is the framework's.
3. `alloc_coherent` for the 4 KiB IDMAC descriptor ring and the 64 KiB
   data bounce window (both bus-address-checked below 4 GiB — the 32-bit
   descriptor mode cannot reach higher; 64-bit descriptors are a
   follow-up).
4. `request_irq` the controller interrupt; the handler upgrades a `Weak`
   into the host and publishes transfer status.
5. `sd::detect_card` through the subsystem core; on success the card is
   added to the domain on the mmc bus (`add_bus_instance` +
   `add_device`, parented to this device). No card is a clean outcome.

## Data path

Coherent bounce window + IDMAC descriptor chain: callers' buffers are
copied through the window, which keeps the first slice free of cache
maintenance and virt→phys translation. Commands are issued by polling the
CMD START bit (`mci_send_cmd` shape); data completion rides the DTO/IDMAC
interrupt into `XferStatus`, with a bounded poll fallback for probe-time
transfers (SCR/switch reads).

## Clocking

The driver never reprograms the upstream ciu rate (no `clk_set_rate`):
init → default → HS all come out of the controller divider, computed with
the Linux `dw_mci_setup_bus` math from `ciu.get_rate()`. UHS-I is out of
scope, so the sample/drive phase clocks (`SCLK_SDMMC_DRV/SAMPLE`) are
untouched.
