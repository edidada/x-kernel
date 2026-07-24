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

---

## 复盘：真机 Io 失败——从预言到验证（Orange Pi 5 Plus）

上面这份 walkthrough 在"验证状态"里留了一条没勾的项：

> `[ ] 真机 RK3588 冒烟：bus_hz 校准是关键——若默认 100 MHz 与 U-Boot 实配不符，卡识别会失败`

然后把驱动刷上 Orange Pi 5 Plus 真机，日志正是预言的那个失败：

```text
[14.487] sdmmc: dw_mshc verid=0x270a fifo_depth=128 hdata_width=1 trans_mode=0
[14.491] sdmmc: failed to initialize card at FirmwareNode { id: 0 }: Io
[14.492] driver sdmmc probe failed for descriptor DeviceDescId(0): Io
[14.493] sdmmc: dw_mshc verid=0x270a fifo_depth=256 hdata_width=1 trans_mode=0   # 第二个节点(sdio)
[14.494] sdmmc: failed to initialize card at FirmwareNode { id: 1 }: Io
```

`verid/fifo_depth` 能读出来 → MMIO 映射没问题；但卡识别直接 `Io`。这是一次**教科书式的"P5 全绿、P6 挂"**——也是这次复盘要固化的教训。

### 定位方法：拿 U-Boot 当"已知可用"基线

关键动作：**别从零猜，先确认同一块板、同一个控制器在 U-Boot 下是好的**。OPi5+ 上 `mmc info`：

```text
Device: mmc@fe2c0000      Bus Speed: 50000000   Mode: SD High Speed (50MHz)
Bus Width: 4-bit   SD version 3.0   High Capacity   Capacity: 116.2 GiB
```

U-Boot 能完整枚举同一张卡（50 MHz / 4-bit）→ 硬件、时钟、供电、卡本身**全部 OK**。那么 x-kernel 这版挂掉**只能是驱动序列漏了东西**。这就把搜索范围从"硬件/固件/驱动"收窄到"驱动序列差异"。

### 真正的根因：命令索引被截断成 5 位（一条铁律 1 翻译错误）

加了可观测性后，真相一条日志就暴露了：

```text
sdmmc: CMD8 resp=0x000001aa (expect 0x000001aa)   # 卡活着！时钟/供电/pinmux/bus_hz 全 OK
sdmmc: CMD55 error rintsts=0x100 (RTO)            # 但 CMD55 卡不回
```

CMD8 正确回显 `0x1aa` = SD v2.0 卡正常应答。这说明**时钟、供电、pinmux、bus_hz 全都没问题**——卡一直在好好回话。那 CMD55 为什么 RTO？唯一可能是**发出去的命令本身是错的**。

定位到 [regs.rs](../../../../../drivers/block/src/sdmmc/regs.rs) 的 `cmd::indx`：

```rust
pub const fn indx(n: u32) -> u32 { n & 0x1f }   // ← 错：5 位
```

dw_mshc 的 `CMD` 寄存器命令索引在 `[5:0]`（6 位，因为第一个 flag 位 `RESP_EXP` 在 bit 6），掩码应是 `0x3f`。`& 0x1f` 把 **opcode ≥ 32 的命令索引截断了**：

| 命令 | opcode | `&0x1f` | `&0x3f` |
|------|--------|---------|---------|
| CMD8 | 8 | 8 ✓ | 8 ✓ |
| CMD55 | 55 | **23 ✗** | 55 ✓ |
| ACMD41 | 41 | **9 ✗** | 41 ✓ |

于是控制器向卡发了索引 23（不是 55），卡不认 → RTO。CMD8（8<32）没被截，所以一路正常。改成 `n & 0x3f` 后，卡完整枚举：`card ready sdhc=true rca=0x5048 capacity=243679232 blocks (118984 MiB)`——和 U-Boot 报的 116.2 GiB 完全一致。

> 对照上游：Linux `dw_mci_prepare_command` 是 `cmdr = cmd->opcode`，**直接写原值、不做掩码**（opcode ≤ 63 本就落在 6 位里）。本移植多加的 `& 0x1f` 既不在 `dw_mmc.h` 里，也不在 Linux 里——正是铁律 1（"硬件是真相，原样照搬"）的违反，也是 Phase 3 可追溯校验（skill ②）本该拦下的：画像声称"regs.rs 逐字照搬 dw_mmc.h"，实际 `indx` 偷偷偏离了。

### 走过的弯路（值得记的"合理但错"的假设）

定位到 `indx` 之前，先猜了两个根因，都**合理、都改了、但都不是真凶**：

1. **漏写 `PWREN`**（对照 U-Boot `dwmci_init` 第二行 `PWREN=1`、Linux `MMC_POWER_UP`）——加了。但卡从头到尾都正常回 CMD8，说明供电从来不是问题。
2. **`bus_hz` 硬编码**（DT 无 `clock-frequency`，只有 `max-frequency=150M`≠CIU 速率）——查 U-Boot 实测改 50 MHz。但 50 MHz 给出的 ID 时钟本来就对，walkthrough 预言的"bus_hz 错→卡识别失败"**并没有发生**。

这两个改动**仍然保留**：`PWREN=1` 是对照参考的正确写法（防御性，DT 也带 `vmmc-supply`）；`bus_hz=50MHz` 是 U-Boot 实测值，且 **clk-rate 缺口对高速模式是真的**（见 skill ①）。只是它们**不是这次的 blocker**。

> 教训：**别锚定在自己预言的假设上**。walkthrough 一开始就预言"bus_hz 会炸"，结果真机一炸就先往 bus_hz/PWREN 上想——但数据（CMD8=0x1aa）一出来就该知道时钟/供电没问题、问题在命令本身。让数据带路，别让预言带路。

### 可观测性是破局点

裸 `Io` 给不出任何信息。加上"`CMD8 resp=…` + `CMD{opcode} error rintsts=…`"后，**一条日志**就把范围从"时钟/供电/pinmux/命令"收窄到"命令本身"。这正是 skill ③⑤ 给 Phase 6 立"可观测性是前提"的原因——没有它，这次大概率还在 bus_hz 上打转。

### 已落地的修复

驱动侧（[regs.rs](../../../../../drivers/block/src/sdmmc/regs.rs) / [host.rs](../../../../../drivers/block/src/sdmmc/host.rs) / [mod.rs](../../../../../drivers/block/src/sdmmc/mod.rs) / [胶水层](../../../../../drivers/kdriver/src/driver_registry/block/sdmmc.rs)）：

1. **`indx`：`&0x1f` → `&0x3f`**（THE fix，命令索引 6 位）。
2. **可观测性**：`send_command` 失败打印 `CMD{opcode} error rintsts={..} status={..} ({RTO/RCRC/...})`；`CMD8 resp` 打印；`log_clock_state` dump `CLKDIV/CLKENA/PWREN/...`（对照 U-Boot）。
3. `PWREN=1`（防御性，对照 U-Boot/Linux）。
4. `bus_hz=50MHz` + probe 打印（clk-rate 缺口照报，高速需 clk 框架校准）。

> 遗留（非阻塞）：`fifo_depth` 读取在 U-Boot 预置过 FIFOTH 时会读到真实值的一半（`1+RX_WMark`，而 U-Boot 已把 RX_WMark 设成 depth/2-1）。不影响枚举/PIO 正确性（PIO 按 STATUS 的 FCNT 走），仅 watermark 偏小。后续可读 DT `fifo-depth` 或 HCON 的 fifo 字段修正。

skill 侧（本次同步改）：

- **①** clk 缺口拆 `clk-enable`(works-around) / `clk-rate`(取不到真值=smoke-blocker)，写明 Rockchip `max-frequency` ≠ CIU 速率（[gaps.md](../knowledge/gaps.md) + [api-mapping §8](../knowledge/api-mapping.md)）。
- **②** Phase 1 产出"编号参考序列清单"，Phase 3 加可追溯校验——**这次的 `indx` 错就是它本该拦下的**。
- **③⑤** 新增 **Phase 6 带机校验/triage**：可观测性前提 + 按设备类 smoke 验收（SD=完整枚举+读块）+ 症状→原因表 + 对照 U-Boot dump 差异。
- **④** gaps.md 新增"真机 bring-up 易踩的坑"（PWREN / CDETECT 不可信 / 多节点）。

### 这次复盘要带走的一句话

> **真机失败时，先上可观测性，别先上假设。** 这次 blocker 是一条 `&0x1f` 该写 `&0x3f` 的寄存器位宽错误——它只咬 opcode≥32 的命令（CMD0~31 全好），所以"前面都正常、到 CMD55 突然挂"是它的指纹。预言中的 bus_hz 风险反而是红鲱鱼。教训三连：(1) 每条错误路径必须 dump 状态寄存器（P6 前提）；(2) 别让预言锚定方向，让数据带路；(3) 铁律 1 的"原样照搬"要靠 Phase 3 可追溯校验兜底——`dw_mmc.h` 里没有 `&0x1f`，代码里就不该有。

---

## 复盘二：数据通路 bring-up——PIO 读不出栈 + 分区表（Orange Pi 5 Plus）

复盘一让卡识别通了（CMD0..7 全过、`card ready`）。但真机**一读块就挂**——数据通路第一次上真机，又踩了两层坑。这次复盘固化这两层。

### 症状演进（每层都靠可观测性收窄）

1. **fs_boot 第一次 `read_block` 卡死**（连错误都没有）。给 `pio_read`/`pio_write` 补上 `DATA_ERROR_FLAGS` 检查 + 超时 + `rintsts/status` dump（复盘一那条"每条错误路径必须 dump"的落实）后，变成有信息的超时。
2. **`drained 1024/1024`**：kext4 读超级块要 1024 字节（跨 2 块），驱动却写死 `BYTCNT=512`、只发 CMD17，`pio_read` 完成分支还按 `buf.len()` 去排空 FIFO（读到空 FIFO）。修：`read_block`/`write_block` **逐块循环**履行 `BlockDevice` 的"buffer 可跨多块"契约（[block/src/lib.rs](../../../../../drivers/block/src/lib.rs) trait 文档）；`pio_read` 改严格按 `FCNT` 排空（`drain_rx`，**绝不空读** FIFO）。
3. 逐块后变 **`drained 512/512` 仍 timeout**：单块 CMD17、`bytcnt=512 blksiz=512 cmd=...` 全对，但 `DATA_OVER` 永远不来、FIFO 满着。加"双偏移 pop-test"（卡空闲时读 `0x200`/`0x100` 看 `fcnt` 降不降）→ **两个偏移都不弹**。
4. 对照 Linux 源码（`dw_mmc.c`/`dw_mmc-rockchip.c`）定位：RK3588 dw_mshc 的 `HCON TRANS_MODE=0` = 内部 IDMAC，**U-Boot 和 Linux 都走 DMA、不走 PIO**；U-Boot 留下 `CTRL.USE_IDMAC`(bit25)/`DMA_ENABLE`(bit5) 置位，而 `ctrl_reset` 的 DMA_RESET 脉冲**不清这两个配置位** → FIFO 输出仍路由给未配置的 DMA 引擎 → PIO 读数据口永远不出栈、`DATA_OVER` 永远不来。修：init 显式清这两位（正是 Linux `dw_mci_submit_data` 进 PIO 时做的事）。
5. 读通后 **`InvalidMagic(0x0062)`**：读块已完全正确（`CTRL=0x0` PIO、FIFO 按序排出）。读 block 0 见 `MBR sig 0xAA55`、`p1 type=0xEE @LBA 1`——**GPT 保护性 MBR**，ext4 在分区里、不在 LBA 0，而 fs_boot 直挂裸设备。修：在 [fs/block/src/partition.rs](../../../../../fs/block/src/partition.rs) 加 GPT/MBR 解析 + `PartitionBlockDevice`（按起始 LBA 偏移读写），[kext4_vfs mount_bdev](../../../../../fs/bridges/kext4_vfs/src/fs.rs) 挂载前先找分区、包一层再挂。

### 已落地的修复

驱动侧（[regs.rs](../../../../../drivers/block/src/sdmmc/regs.rs) / [host.rs](../../../../../drivers/block/src/sdmmc/host.rs) / [mod.rs](../../../../../drivers/block/src/sdmmc/mod.rs)）：

1. **init 显式清 `CTRL.USE_IDMAC | DMA_ENABLE`**（THE fix of 数据通路；见 [gaps.md](../knowledge/gaps.md) "CTRL DMA 位"坑）。
2. **`pio_read`/`pio_write` FCNT-bound**：`drain_rx`/`fill_tx` 严格按 `FCNT`/剩余空间排/填，**绝不空读/空写 FIFO**；每轮查 `DATA_ERROR_FLAGS` + 超时，失败 dump `rintsts/status`。
3. **`read_block`/`write_block` 逐块循环**：履行多块契约，每块独立 `BYTCNT=512`、`DATA_OVER` 精确对齐。

FS 侧：

4. **分区解析**：`fs_block::partition` 提供 `find_root_partition_lba`（GPT 为主、MBR 兜底）+ `PartitionBlockDevice`；ext4 桥挂载前解析、按偏移挂。

### 这次复盘要带走的一句话

> **PIO 在"本该走 DMA 的 IP"上是带电作业。** 这颗 dw_mshc 的设计路径是 IDMAC，U-Boot 把 `CTRL` 的 DMA 位置上了就不撤；`ctrl_reset` 只复位 DMA 逻辑、不清配置位——于是 PIO 的 FIFO 读永远不出栈，且**不报任何错误**（数据照进 FIFO、`DATA_OVER` 就是不来）。指纹："命令/时钟/契约全对、数据在流、FIFO 满着、无错误位、无 `DATA_OVER`"。修法就一行：进 PIO 前显式清 `USE_IDMAC`+`DMA_ENABLE`，对照 Linux `dw_mci_submit_data`。教训：(1) 移植 PIO 驱动到"设计走 DMA 的 IP"时，**必须**清前任留下的 DMA 配置位，别假设 reset 会清；(2) "数据正确但位置不对"（`InvalidMagic` 而 `CTRL=0x0`、FIFO 正常）是分区/布局问题，不是驱动问题——dump 块内容 + 查 MBR/GPT 一步分清；(3) 复盘一的"每条错误路径必须 dump"这次又救了命——没有它，PIO 不出栈会是一个无声死循环。
