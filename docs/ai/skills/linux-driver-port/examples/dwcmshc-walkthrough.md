# Demo 走查：Linux dwcmshc → x-kernel SD/MMC

这是 skill 的完整实例。对照项目**真实实现**：
- 胶水层 [driver_registry/block/sdmmc.rs](../../../../../drivers/kdriver/src/driver_registry/block/sdmmc.rs)
- 硬件逻辑层 [drivers/block/src/sdmmc.rs](../../../../../drivers/block/src/sdmmc.rs)

> **读这个 demo 要带走的头号教训**：Linux 的 SoC 驱动常建立在**子框架**上（这里是 sdhci/mmc-core）。x-kernel **没有这些框架**。于是"转写"往往不是逐函数翻译，而是"用 x-kernel 的设备类模式重新实现等价功能 + 把缺失的子框架作为缺口上报"。识别**框架级缺口** vs **API 级缺口**，是本 skill 的核心能力之一。

目标硬件：RK3588 eMMC / SD 控制器，DT `compatible = "rockchip,rk3588-dwcmshc", "snps,dwcmshc"`。

---

## Phase 0 — 采集

| 项 | 值 |
|----|----|
| Linux 源 | `drivers/mmc/host/sdhci-of-dwcmshc.c`（基于 sdhci-pltfm 框架） |
| compatible | `snps,dwcmshc`（项目 `firmware_specs.rs` 已声明，另含 `arm,pl18x`） |
| 设备类 | `Block`（Linux 这边是 mmc block，对应 x-kernel `BlockDevice`） |
| 落点 | 硬件层 `drivers/block/src/sdmmc.rs`；胶水层 `drivers/kdriver/src/driver_registry/block/sdmmc.rs` |
| 项目已存在？ | **是** → 本 demo 是"对照/说明"模式，真实代码即标准答案 |

`grep` 确认（实跑）：

```bash
$ grep -rn "dwcmshc" drivers/
drivers/kdriver/src/driver_registry/firmware_specs.rs:        dt_compatibles: &["snps,dwcmshc", "arm,pl18x"],
```

---

## Phase 1 — 分析（Linux dwcmshc 画像）

通读 Linux `sdhci-of-dwcmshc.c` 的结构（它是个 **sdhci-pltfm 派生驱动**）：

1. **probe 流程**（典型 sdhci 派生）：
   - `devm_clk_get` ×N（core/timeout/bus/bg/…）→ `clk_prepare_enable`
   - （rk3588）vendor 寄存器初始化、PHY 配置
   - `sdhci_pltfm_init`（分配 `sdhci_host` + `sdhci_pltfm_data`、`devm_ioremap`）
   - 填 `host->ops = &dwcmshc_ops`（含 rk3588 的 `set_clock`/`hs400_enhanced_strobe` 等 quirk）
   - `sdhci_add_host`（向 **mmc core** 注册：`mmc_alloc_host`/`mmc_add_host`，暴露 `/dev/mmcblk*`）
2. **依赖的子框架 / 子系统**：
   - **sdhci core**（寄存器约定、命令状态机）—— **框架级依赖**
   - **mmc core**（block 层、卡识别、协议状态机）—— **框架级依赖**
   - **sdhci-pltfm**（platform glue）—— **框架级依赖**
   - clk、phy、pinctrl、reset、regmap（vendor GRF）—— **子系统级依赖**
3. **寄存器操作**：大量在 `host->ops` 回调里对 SDHCI 标准寄存器（`SDHCI_CMD/ARG/BUFFER/DMA_ADDRESS`…）+ dwcmshc/vendor 偏移做 RMW。
4. **中断**：sdhci 标准 IRQ（`SDHCI_INT_STATUS`），top-half 在 sdhci core，下半部走 mmc core 工作队列。
5. **私有数据**：`struct sdhci_host` + `struct dwcmshc_priv`（PHY/clk 指针、vendor 状态）。
6. **设备类操作**：经 mmc core 的 `mmc_block` → 标准 block 请求（`read/write/flush` 语义）。

**关键观察**：几乎**所有**逻辑都封在 sdhci/mmc-core 框架里，驱动本体主要是"填 ops + 接线 clk/phy"。x-kernel 没有这两个框架 → 这是**框架级 blocker**，不能逐函数翻译。

---

## Phase 2 — 映射

### 命中（API 级，可直接替换）

| Linux | x-kernel | 依据 |
|-------|----------|------|
| `devm_ioremap` / `sdhci_pltfm_init` 的 ioremap | `crate::iomap_first_mmio(dev,"sdmmc")` → `(NonNull<u8>, size)` | kdriver/src/lib.rs:93 |
| `readl/writel`（SDHCI 寄存器） | 由 `simple_sdmmc` 库内部对 vaddr 做 volatile 读写 | block/src/sdmmc.rs 经 SdMmc |
| `register_blkdev` / `mmc_add_host`（→ block 暴露） | `impl BlockDevice` + `kclass::publish_block` | block/src/lib.rs；kclass/src/lib.rs:495 |
| 自旋串行化硬件访问 | `SpinNoIrq<SdMmc>` | block/src/sdmmc.rs:20 |
| `dev_err/dev_info` | `log::error!/info!` | api-mapping §7 |

### 缺口（框架级 + 子系统级）

| # | 依赖 | 类型 | 程度 | 处置（项目实际做法） |
|---|------|------|------|---------------------|
| 1 | **sdhci core / mmc-core / sdhci-pltfm 框架** | 框架级 | blocker（逐行翻译不可行） | **不复用 Linux 框架**：改用项目自带的 `simple_sdmmc` 协议库实现 SD/MMC 命令序列，上层只做 `BlockDevice` 适配。缺口照报（见 P4）。 |
| 2 | clk framework | 子系统级 | works-around | 删除 `clk_get/prepare_enable`；RK3588 MMC 时钟由 bootloader 配好，内核继承。选频（hs200/hs400）路径缺失→当前仅基础速率。 |
| 3 | phy | 子系统级 | works-around（MMC）/ blocker（PCIe/USB 的 phy） | dwcmshc 的 PHY 配置：rk3588 走 vendor 寄存器，`simple_sdmmc`/驱动按固定配置处理，不走 phy 框架。 |
| 4 | pinctrl | 子系统级 | works-around | 删调用，pinmux 由 firmware 配置。 |
| 5 | regmap（vendor GRF） | 子系统级 | works-around | 若需配 vendor 寄存器，用 `Io` RMW helper（api-mapping §替代）。 |
| 6 | 中断下半部（mmc core workqueue） | 子系统级 | works-around | `simple_sdmmc` 当前用轮询/阻塞式命令完成，不经 workqueue。 |

> **映射纪律体现**（铁律 2）：上面没有一条"编造的 x-kernel API"——`iomap_first_mmio`/`publish_block`/`SpinNoIrq` 全部在源码里。sdhci/mmc-core 找不到对应物，就如实记为框架级缺口（铁律 4）。

---

## Phase 3 — 生成（对照真实代码）

### 硬件逻辑层 `drivers/block/src/sdmmc.rs`（项目真实实现节选）

```rust
pub struct SdMmcDriver {
    inner: SpinNoIrq<SdMmc>,   // simple_sdmmc 协议库；自旋锁串行化
    num_blocks: u64,            // 缓存容量
}

impl SdMmcDriver {
    /// # Safety
    /// `base` 必须是该 SD/MMC 控制器寄存器块的合法映射地址，且无并发访问。
    pub unsafe fn new(base: usize) -> Self {
        let sdmmc = SdMmc::new(base);          // ← 寄存器协议在 simple_sdmmc 内
        let num_blocks = sdmmc.num_blocks();
        Self { inner: SpinNoIrq::new(sdmmc), num_blocks }
    }
}

impl Device for SdMmcDriver {
    fn name(&self) -> &str { "sdmmc" }
    fn device_kind(&self) -> DeviceKind { DeviceKind::Block }
}

impl BlockDevice for SdMmcDriver {
    fn num_blocks(&self) -> u64 { self.num_blocks }
    fn block_size(&self) -> usize { SdMmc::BLOCK_SIZE }
    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        let (blocks, rem) = buf.as_chunks_mut::<{ SdMmc::BLOCK_SIZE }>();
        if !rem.is_empty() { return Err(DriverError::InvalidInput); }
        let mut dev = self.inner.lock();
        for (i, block) in blocks.iter_mut().enumerate() {
            dev.read_block(block_id as u32 + i as u32, block);
        }
        Ok(())
    }
    // write_block / flush 同理
}
```

**这段代码说明了什么**：
- Linux 把"寄存器命令序列"放在 sdhci core 的 `host->ops` 里；x-kernel 把它放进 `simple_sdmmc` 库，`SdMmcDriver` 只做 **BlockDevice 适配 + 并发控制 + 缓冲校验**。
- `read_block` 里的"块对齐校验 + 循环"是 Linux mmc block 层的逻辑下沉到了这里。
- `unsafe fn new` 的 `# Safety`、`SpinNoIrq` 串行化，都符合 [code-guidelines](../../code-guidelines/SKILL.md)（铁律 3）。

### 胶水层 `drivers/kdriver/src/driver_registry/block/sdmmc.rs`（项目真实实现全文）

```rust
use alloc::{boxed::Box, sync::Arc};
use driver_base::DeviceKind;
use kdevice::{BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject};
use crate::driver_registry::{BoxedDriver, firmware_specs::SDMMC};

struct SdmmcDriver;

impl DeviceDriver for SdmmcDriver {
    fn name(&self) -> &'static str { "sdmmc" }
    fn device_kind(&self) -> DeviceKind { DeviceKind::Block }
    fn bus_types(&self) -> &'static [BusTypeId] { &[BusTypeId::PLATFORM] }
    fn matcher(&self) -> &dyn DeviceMatcher { &SDMMC }

    fn probe_device(&self, device: Arc<DeviceObject>) -> driver_base::DriverResult<()> {
        let (vaddr, _) = crate::iomap_first_mmio(device.as_ref(), "sdmmc")?;
        let vaddr = vaddr.as_ptr() as usize;
        // SAFETY: iomap_first_mmio 返回该设备独占映射的内核虚拟地址，
        // 生命周期随 device；SdMmcDriver::new 据此访问 SD/MMC 寄存器。
        let dev = unsafe { block::sdmmc::SdMmcDriver::new(vaddr) };
        kclass::publish_block(device, Box::new(dev))
    }
}

pub(super) fn descriptor() -> BoxedDriver { Arc::new(SdmmcDriver) }
```

**这就是 [driver-skeleton.md](../templates/driver-skeleton.md) 里"vaddr 模式"的真实版**：`iomap_first_mmio` → `new(vaddr)` → `publish_block`，四步一气呵成。

> **诚实标注**：当前 `drivers/block/src/lib.rs` 里 `pub mod sdmmc;` 处于注释状态（`// #[cfg(feature = "sdmmc")] pub mod sdmmc;`），说明该路径的启用方式仍在调整。这恰好印证铁律 4——任何"未完全接线"的状态都要在缺口报告里写明，而不是假装它已可用。

---

## Phase 4 — 缺口报告（实例）

```
sdmmc (dwcmshc) 移植缺口报告

- Linux 源: drivers/mmc/host/sdhci-of-dwcmshc.c
- compatible: snps,dwcmshc (RK3588: rockchip,rk3588-dwcmshc)
- 目标 DeviceKind/bus: Block / PLATFORM
- 落点: drivers/block/src/sdmmc.rs + driver_registry/block/sdmmc.rs
- 移植结论: 受限可用（基础 SD/MMC；高速/HS200/HS400 模式未支持）

依赖子系统：MMIO✅ IRQ(轮询)✅ DMA(库内)✅  sdhci-core❌ mmc-core❌
           clk❌ phy❌ pinctrl❌ regmap(vendor)❌

逐条缺口：
#1 sdhci/mmc-core/sdhci-pltfm 框架 | blocker(框架级) | 不复用，改用 simple_sdmmc
#2 clk                              | works-around   | 删 enable；选频缺失(仅基础速率)
#3 phy                              | works-around   | vendor 寄存器固定配置，不走 phy 框架
#4 pinctrl                          | works-around   | firmware 已 mux
#5 regmap(vendor GRF)               | works-around   | 需要时用 Io RMW helper
#6 mmc-core workqueue 下半部        | works-around   | simple_sdmmc 轮询/阻塞完成

阻塞项汇总：无"完全阻塞"项——通过放弃 sdhci/mmc-core 框架改走自研协议库，
驱动可在基础速率下工作。HS200/HS400 等高速模式受 clk/phy 框架缺失限制。

可编译性：见 Phase 5（注：drivers/block/src/lib.rs 中 sdmmc 模块当前为注释状态，
启用方式待接线——gap，需确认 feature/发布路径）。
```

---

## Phase 5 — 集成（对照真实改动）

### ① compatible — `firmware_specs.rs`（已存在）

```rust
#[cfg(feature = "sdmmc")]
pub(crate) const SDMMC: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "sdmmc",
    dt_compatibles: &["snps,dwcmshc", "arm,pl18x"],
    acpi_ids: &[],
};
```

### ② 工厂注册 — `block/mod.rs`（已存在）

```rust
#[cfg(feature = "sdmmc")]
mod sdmmc;

const DRIVER_FACTORIES: &[crate::driver_registry::DriverFactory] = &[
    // …
    #[cfg(feature = "sdmmc")]
    sdmmc::descriptor,
];
```

### ③ Cargo feature + defconfig

`drivers/kdriver/Cargo.toml` 的 `sdmmc` feature，以及 `platforms/kplat-aarch64/rk3588_defconfig` 的 `KFEAT_…=y`（按 [build-workflow](../../build-workflow/SKILL.md)）。

> **校验**：`make` / `cargo clippy`（aarch64 rk3588 target）；启动日志查 `registered driver: sdmmc`；DT 命中后 `probe_device` 被调用，`publish_block` 后块设备可用。

---

## 教训总结（这个 demo 要教会 AI 什么）

1. **框架级缺口是最常见、最容易被忽略的 blocker**。Linux SoC 驱动几乎都挂在子框架上（sdhci/mmc-core/usb/net/libata/…）。看到 `xxx_alloc_host`/`xxx_add_host`/`xxx_register_*` 这类"向子框架注册"的调用，先判断 x-kernel 有没有该框架；没有就走"自研/重新实现 + 缺口上报"，**不要**逐行翻译那些框架调用。

2. **"转写"的正确粒度**：寄存器序列、bit 位、时序 → 忠实搬运（这里交给 `simple_sdmmc`）；框架胶水（注册/回调接线）→ 按设备类模式重写。

3. **缺口报告要说清"放弃了什么、换来了什么、限制了什么"**：本例放弃了 sdhci/mmc-core 框架 → 换来基础 SD/MMC 可用 → 限制是高速模式。这种诚实的取舍说明，比"假装全功能移植"有价值得多。

4. **每个 `unsafe`、每个被删的依赖、每个未接线的模块，都要留痕**（铁律 3、4）。

下一个练手建议：用一个**子框架依赖更少**的驱动（如 `drivers/watchdog/dw_wdt.c`，`snps,dw-wdt`）完整跑一遍，体会"API 级缺口"（clk 算 timeout）和"框架级缺口"的差异。
