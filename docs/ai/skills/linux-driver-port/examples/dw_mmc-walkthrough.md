# 真实移植走查：从零移植 dw_mmc（RK3588 SD 卡）

这是 `linux-driver-port` skill 的第二个实例，也是一次**从零、对照 Linux、全新实现**的移植（与 [`dwcmshc-walkthrough.md`](dwcmshc-walkthrough.md) 的"对照已有实现"互补）。它同时是这个驱动的缺口报告。

> **核心教训（先讲结论）**：这次移植同时撞上**框架级缺口**（无 mmc core）和**构建系统集成缺口**（feature 链 `KFEAT→kfeat→kdriver→block` 必须逐层打通）。前者靠"自建精简协议层"解决，后者靠补齐 Kconfig 定义 + defconfig + Cargo feature 链解决。

## Phase 0 — 采集

| 项 | 值 |
|----|----|
| Linux 源 | `drivers/mmc/host/dw_mmc.c`（core，~2500 行）+ `dw_mmc-rockchip.c`（平台层）+ `dw_mmc.h`（寄存器） |
| 目标 | RK3588 SD 卡接口 `0xfe2c0000`（DT `sdmmc` 节点） |
| compatible | `rockchip,rk3588-dw-mshc` → fallback `rockchip,rk3288-dw-mshc`（用 `rk3288_drv_data`，RK3588 dw_mshc IP 与 RK3288 兼容） |
| 设备类 | `Block`（SD 卡 → `BlockDevice`） |
| 落点 | 硬件层 [drivers/block/src/sdmmc/](../../../../../drivers/block/src/sdmmc/)；胶水层 [driver_registry/block/sdmmc.rs](../../../../../drivers/kdriver/src/driver_registry/block/sdmmc.rs) |
| 起点 | 删除项目里基于 `simple-sdmmc` 的旧实现（用户要求全新对照 Linux） |

> **关键澄清**（容易踩的坑）：RK3588 有**两类** MMC 接口——SD 卡 `0xfe2c0000` 走 `dw_mmc-rockchip`（compatible `rockchip,rk3588-dw-mshc`）；eMMC `0xfe2e0000` 走 `sdhci-of-dwcmshc`。两者寄存器不同、Linux 驱动不同。"做 sdmmc" = SD 卡 = `dw_mmc`。旧实现把 compatible 错配成 `snps,dwcmshc`（那是 eMMC 的 SDHCI），必须纠正。

## Phase 1 — 分析（驱动画像）

通读 `dw_mmc.c` 提取的可照搬序列（铁律 1 依据）：

- **probe 序列**（`dw_mci_probe`，PIO 子集）：`ctrl_reset(ALL_RESET_FLAGS)` → `RINTSTS=0xFFFFFFFF; INTMASK=0; TMOUT=0xFFFFFFFF` → 读 `FIFOTH` 得 `fifo_depth` → 设 `FIFOTH=SET_FIFOTH(2, depth/2-1, depth/2)` → `CLKENA=0; CLKSRC=0` → 读 `VERID` 选 FIFO 偏移（`<240a:0x100`, `≥240a:0x200`）。
- **时钟配置**（`dw_mci_setup_bus`）：`CLKENA=0 → send(UPD_CLK)` → `CLKDIV=div → send(UPD_CLK)` → `CLKENA=ENABLE → send(UPD_CLK)` → `CTYPE=width`。
- **发命令**（`mci_send_cmd`）：`CMDARG=arg → wait_while_busy → CMD=START|flags` → 轮询 `CMD` 直到 `START` 自清。
- **命令标志构造**（`dw_mci_prepare_command`）：`opcode + RESP_EXP/RESP_LONG/RESP_CRC + DAT_EXP/DAT_WR + STOP + PRV_DAT_WAIT + USE_HOLD_REG`。
- **PIO**（`dw_mci_read/write_data_pio`）：按 `STATUS` 的 `FCNT` 填/排 FIFO，直到 `DATA_OVER`。
- **RK 平台 init**（`dw_mci_rockchip_init`）：`sdio_irq=8`；`bus_hz/=RK3288_CLKGEN_DIV(2)`；`internal_phase=false`（RK3588 不走内部 phase）。

**执行模型决策**：Linux 用中断驱动的 bh-work 状态机（`dw_mci_work_func` + cto/dto timer）。x-kernel 起步改为**同步轮询**（`INTMASK=0`，轮询 `RINTSTS`）——与 U-Boot 的 dw_mmc 一致，省掉整个中断状态机，足够块驱动用。

## Phase 2 — 映射

### 命中（直接照搬/映射）

| Linux | x-kernel | 依据 |
|-------|----------|------|
| 寄存器 `#define SDMMC_*` + bit | [regs.rs](../../../../../drivers/block/src/sdmmc/regs.rs)（逐字照搬 `dw_mmc.h`） | dw_mmc.h |
| `readl/writel`（裸 MMIO） | 裸指针 `read_volatile/write_volatile`（base 由胶水层 `iomap_first_mmio` 映射） | device-res/src/mmio.rs 模式 |
| `register_blkdev`/`mmc_add_host`（→ block） | `impl BlockDevice` + `kclass::publish_block` | block/src/lib.rs |
| `spin_lock` 串行化硬件 | `SpinNoIrq<DwMciHost>` | task/kspin |
| `dev_err/dev_info` | `log::error!/info!` | — |
| `readl_poll_timeout` | `khal::time::monotonic_time_nanos()` 轮询 helper | khal |

### 缺口（见下报告）

## Phase 4 — 缺口报告

```
sdmmc (dw_mmc) 移植缺口报告

- Linux 源: drivers/mmc/host/dw_mmc.c + dw_mmc-rockchip.c + dw_mmc.h
- compatible: rockchip,rk3588-dw-mshc (→ rk3288-dw-mshc)
- 目标 DeviceKind/bus: Block / PLATFORM @ 0xfe2c0000
- 落点: drivers/block/src/sdmmc/{regs,host,mod}.rs + driver_registry/block/sdmmc.rs
- 移植结论: 受限可用（PIO 轮询，基础速率 SD/SDHC，单块读写）
```

| # | 子系统 | 类型 | 程度 | x-kernel 处置 |
|---|--------|------|------|---------------|
| 1 | **mmc core**（mmc_host/mmc_request/slot/协议状态机） | 框架级 | blocker(逐行翻译不可行) | 不移植整套 mmc core；在 [mod.rs](../../../../../drivers/block/src/sdmmc/mod.rs) 自建精简 SD 识别协议（CMD0/8/55+41/2/3/9/7/ACMD6）+ CSD 解码，挂 `BlockDevice` |
| 2 | **clk**（ciu_clk/biu_clk/ciu-drive/sample） | 子系统 | works-around | RK3588 U-Boot 已配 SDMMC 时钟，内核继承；`bus_hz` 用默认 100 MHz（[胶水层](../../../../../drivers/kdriver/src/driver_registry/block/sdmmc.rs) 常量），选频路径缺口 |
| 3 | reset_control | 子系统 | works-around | 假设 firmware 已解除复位；`ctrl_reset` 仅复位控制器/FIFO/DMA 逻辑 |
| 4 | phy / phase tuning（`execute_tuning`） | 子系统 | optional | 基础速率（≤25 MHz）不需；高速/UHS/HS200 需 phase tuning，暂不支持 |
| 5 | IDMAC（内部 DMA） | 子系统 | works-around | 用 PIO 轮询（`pio_read/pio_write`）替代；性能受限，无 scatter-gather |
| 6 | 中断驱动状态机（bh_work/cto/dto timer） | 子系统 | works-around | 同步轮询（`INTMASK=0`）替代；无并发请求、无 IRQ 处理函数 |
| 7 | SDIO 中断 / voltage switch (UHS 1.8V) | 子系统 | optional | 不支持 SDIO 卡、不支持 UHS |
| 8 | FIFO 访问宽度（`data_shift`） | 实现 | works-around | 固定 32-bit 访问；RK3588 实际 FIFO 宽度需按 `HCON` 核实（若 64-bit 需改 `data_shift=3`） |
| 9 | `bus_hz` 真值 | 实现 | works-around | 默认 100 MHz；应从 DT `clock-frequency` 读（依赖 clk 框架），真机需校准 |

**阻塞项汇总**：无"完全阻塞"——通过放弃 mmc core + 中断状态机 + DMA + tuning，驱动可在基础速率 PIO 读 SD 卡。限制：仅 SD/SDHC 单块读写、基础速率、轮询。

## Phase 3 — 生成（代码结构）

```
drivers/block/src/sdmmc/
├── regs.rs     寄存器偏移 + bit（逐字照搬 dw_mmc.h）
├── host.rs     DwMciHost：init / setup_bus / send_command / build_cmd_flags / pio_read / pio_write
└── mod.rs      SD 协议（CMD 序列 + CSD 解码）+ SdmmcDriver(BlockDevice) + 单测
```

胶水层 [driver_registry/block/sdmmc.rs](../../../../../drivers/kdriver/src/driver_registry/block/sdmmc.rs)：`iomap_first_mmio → SdmmcDriver::new(vaddr, bus_hz) → publish_block`。

## Phase 5 — 集成（注册链要打通 5 层）

除了标准"注册三连"，这次还撞上**构建系统 feature 链**必须逐层打通（缺一层 sdmmc 都进不了镜像）：

1. [block/src/lib.rs](../../../../../drivers/block/src/lib.rs)：`#[cfg(feature="sdmmc")] pub mod sdmmc;`
2. [block/Cargo.toml](../../../../../drivers/block/Cargo.toml)：`sdmmc = []`（去掉旧 `simple-sdmmc` 依赖，加 `khal` 依赖）
3. [kdriver/Cargo.toml](../../../../../drivers/kdriver/Cargo.toml)：`sdmmc = ["any_firmware_driver", "block", "block/sdmmc"]`（关键：加 `block/sdmmc` 向下传递）
4. [api/kfeat/Cargo.toml](../../../../../api/kfeat/Cargo.toml)：`driver_sdmmc = ["kdriver/sdmmc"]`（已存在，确认即可）
5. [drivers/Kconfig](../../../../../drivers/Kconfig)：新增 `config KFEAT_DRIVER_SDMMC`（gen-cargo 只认 Kconfig 定义的符号，缺这步 `.xconfig.toml` 不含 `driver_sdmmc`）
6. [rk3588_defconfig](../../../../../platforms/kplat-aarch64/rk3588_defconfig)：`KFEAT_DRIVER_SDMMC=y`
7. [firmware_specs.rs](../../../../../drivers/kdriver/src/driver_registry/firmware_specs.rs)：`dt_compatibles = ["rockchip,rk3588-dw-mshc","rockchip,rk3288-dw-mshc"]`

> **第 5 层是最隐蔽的坑**：`gen-cargo`（生成 `.cargo/.xconfig.toml` 的 features 列表）用 Kconfig engine 解析，只认 Kconfig 里 `config KFEAT_DRIVER_*` 定义的符号。直接往 `.config` 写 `KFEAT_DRIVER_SDMMC=y` 没用——会被当作未知符号丢弃。必须先在 `drivers/Kconfig` 加 `config` 定义。

## 验证状态

- [x] feature 链打通：`kfeat/driver_sdmmc` 进入 `.cargo/.xconfig.toml`
- [x] `make clippy` 编译 block crate 的 sdmmc 模块（详见构建日志；首次迭代修了 3 个编译 bug：漏 import `clkena`、子模块 `ctype` 作用域、`off_b` 未读赋值）
- [ ] 真机 RK3588 冒烟：`bus_hz` 校准是关键——若默认 100 MHz 与 U-Boot 实配不符，卡识别会失败
- [ ] 多块读写（CMD18/25 + CMD12）

## 教训总结

1. **框架级缺口（mmc core）是最重的 blocker**，但可以靠"自建精简协议层挂设备类 trait"绕过——不移植整套框架，只复刻**硬件寄存器序列**（铁律 1）。
2. **执行模型可以降级**：Linux 的中断状态机 → 起步用同步轮询（U-Boot 式），大幅降低复杂度，够块驱动用。
3. **构建系统 feature 链是隐蔽坑**：x-kernel 的 `KFEAT_*` 必须在 Kconfig 定义 → gen-cargo 转 feature → kfeat → kdriver → block 逐层传递。任何一层断，驱动都进不了镜像且**不报错**（静默不编译）。
4. **PIO + 轮询 + 基础速率**是最小可用切面；高速/DMA/UHS 是清晰的后续迭代项（缺口已记录）。
