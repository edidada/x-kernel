# 缺口报告模板

Phase 4（缺口报告）产出物。每个被移植的驱动**必须**配一份。报告目标是让人一眼看清：这个驱动在当前 x-kernel 上**能不能跑、卡在哪、要先补什么**。

> 阻塞程度定义与逐项现状见 [`gaps.md`](../knowledge/gaps.md)。**铁律 4：缺口必报、不静默绕过**——任何被 stub/删除/替代的依赖都要在这里写明。

---

## <DRIVER_NAME> 移植缺口报告

- **Linux 源**：`drivers/<...>/<file>.c`（commit / 版本，可选）
- **目标 compatible**：`<vendor>,<compat>`（DT）
- **目标 DeviceKind / bus**：`Block` / `PLATFORM`（或 Net/Char/PCI）
- **x-kernel 落点**：
  - 硬件逻辑层：`drivers/<class>/src/<name>.rs`
  - 胶水层：`drivers/kdriver/src/driver_registry/<bus>/<name>.rs`
- **移植结论**：`可完整工作` / `受限可用` / `阻塞（需先补子系统）`

### 依赖的 Linux 子系统清单

通读 Linux 驱动后列出它依赖的所有内核子系统（不论 x-kernel 有无）：

- [ ] MMIO 寄存器 ✅
- [ ] IRQ ✅
- [ ] DMA coherent ✅
- [ ] clk framework ❌
- [ ] reset controller ❌
- [ ] phy ❌
- [ ] regmap ❌
- [ ] …（按实际填）

### 逐条缺口

| # | 子系统 | 阻塞程度 | Linux 用法（文件:行/函数） | x-kernel 处置 | 建议 |
|---|--------|---------|--------------------------|--------------|------|
| 1 | clk | works-around | `clk_prepare_enable(clk)` ×2 (probe) | 删除，注释"firmware 已使能" | 长期补 clk/CRU provider |
| 2 | phy | blocker | `phy_power_on(phy)` (probe) | 留 TODO，return `Unsupported` | 需先补 phy 框架 |
| 3 | regmap | works-around | `regmap_update_bits` ×5 | 本地 `Io` RMW helper 替代 | 可接受 |
| 4 | udelay | works-around | `udelay(100)` (reset 后) | `monotonic_time_nanos` 忙等循环 | 可接受 |
| … | | | | | |

每条"处置"必须具体到**改了什么代码**（删除/替代/stub/TODO），不得写"已处理"。

### 阻塞项汇总（blocker）

> 若无 blocker，写"无——驱动可受限可用"。

- **phy**：probe 调 `phy_power_on` 无替代 → 驱动返回 `Unsupported`，当前**无法初始化**。前置任务：实现 phy 框架 + RK3588 相应 PHY 驱动。
- **…**

### 可编译性结论

- `cargo build` / `cargo clippy`（aarch64 rk3588 target）：`通过` / `未运行` / `失败（原因）`
- 若含 stub/TODO：列出每个 `TODO`/`unimplemented!`/`Unsupported` 返回点及其影响。

### 已映射 API 摘要（可选，证明铁律 2）

列出本次移植用到的 x-kernel API 及源码依据，便于复核：

| Linux 调用 | x-kernel 替换 | 依据 |
|-----------|--------------|------|
| `devm_platform_ioremap_resource` | `crate::iomap_first_mmio` | kdriver/src/lib.rs:93 |
| `readl/writel` | 裸指针 `read_volatile`/`write_volatile`（或 `Io`） | device-res/src/mmio.rs |
| `register_netdev` | `kclass::publish_net` | kclass/src/lib.rs:478 |

### 集成状态

- [ ] `firmware_specs.rs` compatible 已加
- [ ] `<bus>/mod.rs` DRIVER_FACTORIES 已加
- [ ] `Cargo.toml` feature 已加
- [ ] 平台 defconfig `KFEAT_…=y` 已加
- [ ] build + clippy 通过
- [ ] （可选）实机/QEMU 冒烟测试

### 后续行动（按优先级）

1. 补 `<最高优先级缺失子系统>` 以解除 blocker。
2. …
