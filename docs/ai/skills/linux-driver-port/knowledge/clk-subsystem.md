# clk subsystem — x-kernel 时钟框架与消费/提供者用法

> 本文是 `data/api-mapping.json`（`取设备时钟` 条目等）与 SKILL.md 引用的
> Tier-0 clk 知识页。数据为源，本文为人类可读的实现指南。

## 1. 分层

```text
 consumers (设备驱动)                    ns16550 / dw-mmc / …
        │  clocks = <&phandle N> (DT)
        ▼
 device-res 契约        ClkSpec / ClkResource / ClkProviderDesc / AssignedClock
        │  ResourceDesc::Clk / ClkProvider / AssignedClock
        ▼
 kdriver 平台总线       provider-first 枚举(probe 顺序保证)、devm_clk_get、
                        iomap_first_mmio、clk 资源 → ClkHandle
        ▼
 kclk (core/kclk)       ClkHwOps(provider ops)、ClkCore(节点树/缓存/两阶段门)、
                        provider 注册表(ProviderId = DT phandle)、
                        register_table(事务性批量注册)、PRE/POST notifier、
                        clk_summary / clk_disable_unused
```

Linux 对应关系：

| Linux | x-kernel |
|-------|----------|
| `drivers/clk/clk.c`（核心） | `core/kclk/src/core.rs` + `provider.rs` |
| `include/linux/clk-provider.h` 的 `clk_ops` | `kclk::ClkHwOps` |
| `of_clk_add_provider` + `of_clk_src_onecell_get` | `kclk::ClkTable` + `kclk::register_provider` |
| `__clk_lookup`（按名全局查找） | `kclk::clk_get_by_name`；`register_table` 解析父名失败时的全局回退 |
| `of_clk_set_defaults`（assigned-clock-rates） | `FirmwareDevice.assigned_clocks` → `ResourceDesc::AssignedClock` → 控制器 probe 末尾 apply |
| 基本 `clk-gate/mux/divider/composite` | `kclk::basic::{Gate, Mux, Divider, Composite, FixedRate, FixedFactor}` |
| `clk_fractional_divider` + rockchip frac branch | SoC 驱动内 Custom ops（rk3588_cru::FracBranch） |
| `clk-cpu.c`（cpuclk） | SoC 驱动内 Custom ops（rk3588_cru::ArmClk） |
| `GATE_LINK` + `gate-link.c`（pm_clk 持链） | gate + probe 常使能 linked clock（无 runtime PM 的等价） |

## 2. 消费者用法（驱动侧）

```rust
// DT: clocks = <&cru 187>, <&cru 172>; clock-names = "baudclk", "apb_pclk"
let spec = device.clk_by_name("baudclk").or_else(|| device.first_clk())?;
let clk = crate::devm_clk_get(device, spec)?;   // → ClkHandle
clk.prepare_enable()?;                          // 两阶段:prepare(可睡) + enable(原子)
let rate = clk.get_rate();                      // 编波特率/CLKDIV 的输入
clk.set_rate(target)?;                          // 同值短路,不会重跑硬件序列
```

- `clock-names` 是消费者本地连接名，由平台层解析回 `(phandle, index)`；
  不要把连接名当作 SoC 时钟名去 `clk_get_by_name`。
- probe 依赖 provider 未注册时错误链为 `ClkError::NoProvider` →
  `WouldBlock` → `ProbeOutcome::Deferred`，总线会在后续轮次重试。

## 3. 提供者用法（控制器驱动）

1. 平台 probe：`devm_iomap` → 取 `clk_provider().provider_id()`（DT phandle）。
2. `Box::leak(ClkTable::new(phandle))`，`kclk::register_table(table, reg, branches)`
   —— 支持静态 `const` 表与运行时 Custom ops 混合（Clone）。
3. 父钟三种引用：同批名字（Local）、跨 provider `(pid, index)`（External）、
   兜底走全局名注册表（Linux `__clk_lookup` 语义）。解析不到 = 报错回滚
   （孤儿不放行；驱动可为已知无 provider 的外部输入显式注册 0 速率占位）。
4. `kclk::register_provider(table)` 发布；最后应用 `device.assigned_clocks()`。

## 4. RK3588 实战经验（clk-rk3588 移植）

- **DT 绑定 id 编号是历史沉淀**：OrangePi 5 Plus 的 DTB 按 Rockchip 5.10 BSP
  头编译（SCLK_UART2=187、CCLK_EMMC=314、SCLK_SDMMC_DRV=706），与 mainline
  （175/300/685）不同。注册索引必须跟 DT 的编号走，寄存器/位段跟 mainline 走；
  两个数据域各自钉死参考树（`bsp-opi5plus` + `linux`）。
- **mode mux 是时钟**：Linux 每只 PLL 注册两个钟（`pll_gpll` 实体 + `gpll`
  mode mux）。mode mux 需要 `SET_RATE_PARENT | NO_REPARENT`——没有
  NO_REPARENT 时 kclk mux 会在 determine_rate 里横向换父，PLL 永远不会被改频。
- **同值 set_rate 必须短路**：bootloader 已按 DT assigned 配好的钟
  （ARMCLK/PPLL/GPLL…）在内核 apply assigned 时不得重跑换父/掉电序列
  （Linux `clk_core_set_rate_nolock` 同款短路）。
- **传播通知**：cpuclk/fracmux 的“换父舞步”挂在父钟（PLL/frac）的 notifier 上，
  框架在 SET_RATE_PARENT 传播改父速率时必须对父也发 PRE/POST，否则舞步不执行。
- **SCMI 钟不归 CRU**：sdmmc 的 ciu（cclk_sd）与 drv/sample 相位的父在 SCMI
  固件侧；CRU 驱动只注册自己的输出。SCMI 消息头位段教训见 gaps.md（VERSION
  会骗人，验证位段必须用带 payload 的命令）。
- **HIWORD 写**：Rockchip CRU 每字高 16 位是写使能掩码。gate 用单次写；
  mux/divider 也要单次写（HIWORD_MASK flags），否则 RMW 窗口与 gate 快路径
  （不同的锁）并发时会丢更新。
```

## 相关

- gaps.json `clk-framework`（status: partial）与需求 `req-clk-framework`
- 移植实例：`examples/rk3588-cru/`（portrait + gap report）
- `ldp lookup clk_prepare_enable` / `ldp scan <驱动源码>` 验证用法
