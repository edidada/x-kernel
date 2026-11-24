# Reference anchors

Every entry is one `scripts/ref-anchor.sh` run: the reference function that was
read before the driver code it aligns. `scripts/ref-anchor.sh --check` refuses to
run the board or to commit driver changes without a fresh entry, so this table
is the record of which mechanisms were transcribed rather than reconstructed.

| when (UTC) | reference | symbol | note |
|---|---|---|---|
| 2026-11-24 01:11 | `/workspace/linux-7.1.8/drivers/clk/rockchip/clk-rk3588.c` | `rk3588_clk_init` | rk-iommu: gate table for the NPU MMU clocks (CLKGATE_CON(x)=CRU+0x800+4x, hiword-mask writes, 1=gated) |
| 2026-11-24 01:14 | `/workspace/linux-7.1.8/arch/arm64/boot/dts/rockchip/rk3588s.dtsi` | `rknpu_mmu` | rk-iommu: upstream NPU MMU declares clocks = ACLK_NPU0 + HCLK_NPU0; the rig DTB declares NPU tree inputs without the bus gates, so ungate both sets |
| 2026-11-24 01:19 | `/workspace/linux-7.1.8/drivers/pmdomain/rockchip/pm-domains.c` | `rk3588_pmu/rockchip_do_pmu_set_power_domain/set_idle_request/mem_reset` | rk-iommu: PD_NPUTOP power-on (PMU@0xfd8d8000, pwr 0x14c bit3, repair 0x290 bit2, mem 0x1f8/0x1a0 bit11, req 0x10c bit1, idle 0x120 bit1, ack 0x118 bit1) |
| 2026-11-24 01:23 | `/workspace/linux-7.1.8/drivers/clk/rockchip/clk-rk3588.c` | `GATE(PCLK_PMU0IOC)/GATE(PCLK_PMU0)` | rk-iommu: PMU0 island MMU (iommu@fdd97e00) declares these two critical clocks |
