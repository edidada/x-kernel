# 缺口报告模板

Phase 4（缺口报告）产出物。每个被移植的驱动**必须**配一份。报告目标是让人一眼看清：这个驱动在当前 x-kernel 上**能不能跑、卡在哪、要先补什么**。

> 阻塞程度定义与逐项现状见 [`gaps.md`](../knowledge/gaps.md)。**铁律 4：缺口必报、不静默绕过**——任何被 stub/删除/替代的依赖都要在这里写明。

---

## <DRIVER_NAME> 移植缺口报告

- **Linux 源**：`drivers/<...>/<file>.c`（commit / 版本，可选）
- **目标 compatible**：`<vendor>,<compat>`（DT）
- **目标 DeviceKind / bus**：`Block` / `PLATFORM`（或 Net/Char/PCI）
- **x-kernel 落点**：
  - 硬件逻辑层+绑定：`drivers/drv/<family>/src/<name>.rs`
  - 类契约(若需新类)：`drivers/subsystems/<class>/`
  - 资源后端桥：`drivers/host`(对照 DeviceServices 现有能力清单)
- **移植结论**：`可完整工作` / `受限可用` / `阻塞（需先补子系统）`

### 依赖的 Linux 子系统清单

通读 Linux 驱动后列出它依赖的所有内核子系统（不论 x-kernel 有无）。

> **提示**：跑 `ldp scan <Linux源码路径>` 可自动揪出这份驱动用到的、且当前是缺口的子系统符号（铁律 4 自动化），把它的输出填进下表。

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
| `devm_platform_ioremap_resource` | `dev.res().iomap(0)` | drivers/base/src/device.rs:327 |
| `readl/writel` | `IoMap::read32`/`write32` | drivers/base/src/devres/iomap.rs:94 |
| `register_netdev` | `dev.publish(&subsys_net::class::NET, …)` | drivers/subsystems/net/src/class.rs:20 |

### 集成状态

- [ ] 驱动 crate 建立（`drivers/drv/<family>`），`static IDS` + `declare_driver!` 完整
- [ ] 组合根（`drivers/init`）调用 register() 已接线
- [ ] Kconfig/defconfig 选中对应 feature
- [ ] `make build` + `make clippy`（含 check_boundaries）通过
- [ ] `ldp check-data` / `check-layout`（若新增位段声明）通过
- [ ] （可选）实机/QEMU 冒烟测试

### 后续行动（按优先级）

1. 补 `<最高优先级缺失子系统>` 以解除 blocker。
2. …
