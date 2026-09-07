# khal::irq - 设计文档

## 定位

`khal::irq` 是 IRQ trap-entry adapter。通用 IRQ core
位于 `kirq`；外部调用方不应再通过 `khal::irq::*` 使用 generic IRQ API。

本模块现在只拥有 trap-entry 相关职责：

- 注册 `IRQ` / `NMI` trap handler；
- 在进入 `kirq` 前建立 `NoPreempt` 执行约束。

## 范围

```text
arch/khal/src/
├── irq/
│   ├── mod.rs       # adapter helper exports only
│   ├── manager.rs   # trap handlers
│   └── nmi.rs       # khal::irq::configure_nmi（每 CPU 中断线提升，委托平台 NmiDef）
└── lib.rs           # khal::nmi / khal::pmu 机制中立 facade（re-export kplat）
```

generic IRQ state、descriptor、dispatch、NMI table、lifecycle hooks 和
`IntrManagerIf` contract 由 `arch/kirq` 文档描述。`khal::nmi` 只做机制中立
facade，机制细节归 `kplat::nm_irq` 与平台实现。

## 架构

```text
kcpu trap dispatch
    |
    `-- khal::irq
            |-- #[register_trap_handler(IRQ)]
            |-- #[register_trap_handler(NMI)]
            |-- NoPreempt guard
            `-- kirq::{handle_irq, handle_nmi}
```

普通 IRQ API 调用路径应直接进入 `kirq`，不经过 `khal::irq` facade。

NMI 机制 facade：

```text
watchdog / perf 消费者
    |
    `-- khal::nmi（re-export kplat::nm_irq）
            |-- early_init / late_init   # 机制探测 + 每 CPU 使能
            |-- mode / info              # 运行时机制查询（Pseudo / Hardware / None）
            |-- enable_periodic_nmi      # 周期 NMI 源（当前为 PMU）
            |-- quiesce_nmi               # 终止路径停靠本地周期源
            `-- khal::irq::configure_nmi # 每 CPU 提升中断线
```

## 调用约束 / 执行上下文

- `irq_handler()` 与 `nmi_handler()` 只能由架构 trap dispatch 调用。
- 两个 handler 都先创建 `kspin::NoPreempt`，再调用 `kirq`。
- `irq_handler()` 调用 `kirq::handle_irq()`；hardirq lifecycle enter/exit 和
  IRQ completion 顺序由 `kirq` 负责。
- `nmi_handler()` 调用 `kirq::handle_nmi()`；NMI raw-hwirq dispatch 语义由
  `kirq` 负责。
- `khal::irq` 自身不保存 `IRQ_STATE` 或 `NMI_TABLE`，避免两个活跃 IRQ core。
- `khal::nmi::early_init()`：GIC 初始化后、任何 NMI 使能前调用一次（平台
  early_driver_init），探测机制并写入运行时 `NmiMode`。
- `khal::nmi::late_init()`：主核 `final_init` 与每个从核 `final_init_ap` 各调用
  一次（每 CPU）；hardware 模式在此开启 `SCTLR_EL1.NMI` / ALLINT 使能。
- `khal::nmi::enable_periodic_nmi(period_ns, cb)`：每 CPU 调用；返回 `false`
  表示本 CPU 无法武装（机制不可用或资源冲突），消费者应显式降级。
- `khal::quiesce_nmi()`：仅供终止停机路径调用；以 `NoPreempt` 钉住当前 CPU，
  停掉已武装的本地周期源但保留回调和 IRQ 配置。没有 `nmi_pmu` source 或本地
  source 未初始化时为 no-op，调用后不得重新武装。
- `khal::irq::configure_nmi(hwirq)`：PPI 需每 CPU 调用，SPI 幂等；返回 `false`
  表示该线无法提升为 NMI（例如机制已降级为 `None`）。
- `khal::nmi::mode()`：返回 `NmiMode::{Pseudo, Hardware, None}`，供消费者
  （如 watchdog）判断是否启用依赖 NMI 的功能。

## 并发模型

`khal::irq` adapter 只有短生命周期的 `NoPreempt` guard，不持有全局 IRQ state。
IRQ shared state、NMI table 和 lifecycle hooks 的锁模型都在 `kirq` 内部维护。

## 设计决策

### 为什么保留 `khal::irq`

trap handler registration 依赖 `kcpu::excp::register_trap_handler`，并且进入
`kirq` 前需要由 HAL adapter 建立 `NoPreempt` 执行约束。因此 `khal::irq`
继续存在，但不再承载 generic IRQ API 或 x86 MSI-X vector helper。

### 为什么 x86 MSI-X helper 不在 `khal::irq`

MSI-X vector allocation、APIC destination selection 和 MSI message composition
属于 IRQ core/backend 边界。当前由 `kirq::MsiBackendIf` 定义 backend contract，
由 `drivers/platform/x86-apic` 实现；`khal::irq` 不再暴露 APIC id 或裸 CPU vector。

### 为什么跨 CPU NMI 请求不属于 `khal::nmi`

周期源的机制探测、武装与停靠属于 NMI HAL；跨 CPU 请求则是 IPI 的消息类型、
descriptor 和目标集合问题。调用方经 `kipi` 使用 `CpuBacktrace` 语义，`kirq`
把它交给 irqchip 映射成实际 SGI/vector。这样 `khal::nmi` 不再暴露裸 SGI 号，
NMI-safe 的精确 mask 发送契约也留在真正拥有 IPI 硬件编码的 IRQ backend。

### 最终电源终点的执行上下文

`power::halt()` / `power::power_off()` 持有外层 `NoPreempt` 直到不可返回的
平台终点，provider 返回时不会因其内部 guard 释放而触发调度。SMP stop
provider 自行建立协调期间的中断约束；返回后，电源入口统一屏蔽 CPU 本地
中断，再进入平台终点。这也覆盖 UP 的 no-op provider 和直接请求固件断电、
不经过 `stop_cpu()` 的路径。终止后的 readiness 撤销和本地屏蔽不参与可逆的
IRQ 启停生命周期；完整流程见 `docs/power.md`。
