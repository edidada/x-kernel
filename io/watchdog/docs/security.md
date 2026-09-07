# io/watchdog — 安全与可靠性分析

## 信任模型

本 crate 是内核可信组件，输入来自硬件事件（定时器中断、NMI/PMU 溢出）与内核内部状态（任务调度、互斥锁）。无用户输入。

## 外部边界 / 攻击面

- 定时器中断与 NMI（PMU 溢出、语义化 CPU-backtrace IPI）事件：事件频率与正确性依赖硬件/仿真及 IRQ backend；
- 内核内部：`ktask` 快照、任务状态、互斥锁状态（`check_mutex_deadlock`）；
- 触发后的输出：`kprint_atomic`（原子打印，NMI 安全）。

## unsafe 代码清单

### `init.rs`

- `init_softlockup_detection` 的定时器回调：`LAST_SOFTLOCKUP_REPORT.current_ref_raw()`（读上一次报告时间戳）与 `current_ref_mut_raw()`（写入本次报告时间戳）。不变量：定时器回调在 IRQ 关闭且不可迁移的上下文执行，per-CPU 原始指针不会与迁移竞争；写路径由同一非迁移回调独占更新。安全入口：`init_softlockup_detection` 注册的定时器回调。

### `lockup_detection.rs`

- `touch_softlockup` / `timer_tick` / `check_softlockup` / `register_hardlockup_detection_task`：`LOCKUP_DETECTION.current_ref_{mut_}raw()`。不变量：watchdog 任务固定核 + 抢占禁用；定时器回调 IRQ 关闭；均不迁移。安全入口：`init_softlockup_detection` 注册的回调与固定核任务。

### `watchdog_task.rs`

- `register_watchdog_task` / `check_watchdog_tasks`：`WATCHDOG_TASK_QUEUE.current_ref_mut_raw()`。不变量：注册仅在 per-CPU init（迁移不可能）；检查在 NMI 上下文（不迁移）。安全入口：`init_nmi_watchdog` / `register_watchdog_task`。

## 内存安全不变量

- 每 CPU 状态只能被所属 CPU 访问；
- 每个 rendezvous 中，`try_join()` 必须先认领该 CPU 的唯一 snapshot writer；
  同 CPU 的 PMU NMI 抢占 regular-IRQ fallback 时必须立即返回，不能重写 slot、
  发布尚未完成的 arrival 或等待被自己打断的 writer；
- NMI 回调中引用的 `&'static` per-CPU 指针与内核同生命周期；
- 原子量排序：`Release` 写 + `Acquire` 读保证软锁时间戳初始化可见；`AcqRel` 用于 rendezvous 状态迁移；backtrace IPI 前由 irqchip 执行 DSB，保证 target 处理中断前可见已发布的 `Triggered` 状态；每 CPU 先写 snapshot slot 再 Release 发布 arrived，cause Acquire 后才读取该 slot。

## 线程安全

- NMI 与定时器中断可能并发访问同一 CPU 的 `LockupDetection`，字段均为原子量；
- rendezvous 全局原子跨 CPU 可见，`try_trigger` 用 `compare_exchange` 保证唯一 cause CPU；
- watchdog 只消费 runtime 已安装的 CpuBacktrace 回调；无活跃 rendezvous 时返回 false，
  不创建/重置 snapshot session。普通诊断由 runtime 输出本地 trap-frame backtrace，
  不依赖 watchdog 初始化，也不写共享快照槽；
- `ARRIVED_BITMAP` 位操作按 CPU id 写入，`usize::BITS` 以上 CPU id 被忽略（平台最多 64 核前提）。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | NMI 回调获取普通 IRQ 锁 | 高（同 CPU 自死锁） | pseudo-NMI 抢占持锁的普通 IRQ 路径 | NMI 路径只用原子与自旋；代码审查约束 |
| T-02 | NMI 未按预期周期到达 | 中（误报 hardlockup） | TCG 仿真下 NMI 延迟 / 丢失 | hardlockup 计数需先初始化（`current != 0`）；NMI 不可用时启动期禁用检测 |
| T-03 | 某 CPU 永远无法进入 NMI | 高（诊断路径再度挂死） | KVM WFI 停止 vPMU、目标 CPU/中断控制器故障 | `CpuBacktrace` IPI 主动唤醒 ready CPU；cause 最多等待 10s，打印 missing mask 后继续 dump/panic |
| T-04 | 快照重入 | 中（快照损坏/死锁） | NMI 打断已有快照流程 | `nmi_begin()` 失败跳过 dump；`kprint_atomic` 原子输出 |
| T-05 | 软锁误报刷屏 | 低（日志风暴） | watchdog 任务饥饿但定时器正常 | 每阈值周期限速一次报告 |
| T-06 | backtrace IPI 与普通 IPI 冲突或越界 | 高（错误 handler / 无法唤醒） | irqchip 向语义类型映射了重复/非法向量 | 裸号由 GIC backend 单点拥有并 const assert；watchdog/Kconfig 不再选择 SGI |
| T-07 | rendezvous 超时清槽与迟到 CPU 写 slot 竞争 | 高（UnsafeCell data race） | target 在超时前观察 Triggered、超时后才完成 collect | arrived 只在 slot 写完后发布；cause 按固定 mask 读取；终态不调用 `nmi_finish()`，保持 session active 直到 panic/power-off |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | NMI 机制不可用 | GICv2 / 无 FEAT_NMI | hardlockup 关闭 | 系统失去硬锁检测能力（软锁检测无法覆盖硬挂起） | 3 | 启动日志 + `mode()` 检查后直接返回 |
| F-02 | 周期 NMI 武装失败 | PMU 不可用 / 武装返回 false | 本 CPU hardlockup 关闭 | 单 CPU 失去硬锁检测 | 3 | `enable_periodic_nmi` 失败记录 error |
| F-03 | watchdog 任务饿死 | 调度问题 | 软锁误报 | 日志风暴 + dump | 3 | 4s 触碰 + 20s 阈值 + 限速 |
| F-04 | 定时器中断停摆 | 中断屏蔽/硬件故障 | `hrtimer_interrupts` 不前进 | NMI 判定 hardlockup → rendezvous → panic | 2 | 这是硬锁检测的预期行为 |
| F-05 | rendezvous 中 cause CPU panic | 检测到任务失败 | 系统停止 | 停机（保留 dump 输出） | 1 | 有意设计：宁可停机也要输出诊断 |
| F-06 | backtrace IPI 不支持/提升/本地使能失败 | 平台无语义向量、拒绝 NMI 属性或 enable 失败 | 无法以 NMI 主动唤醒部分 CPU | 快照可能缺 CPU，但不会永久卡住 | 2 | runtime 独立安装能力；仅投递 ready CPU，等待仍覆盖所有 present CPU，保留周期 NMI fallback 与 10s 上限；全局注册冲突在启动期快速失败 |
| F-07 | NMI 内精确 mask 投递失败 | raw CPU affinity 缺失、GIC 不支持目标 range selector | 部分 peer 不被主动唤醒 | 依赖自然周期 NMI，超时后继续 | 2 | NMI-safe 原子日志 + 有界等待 |

## 故障管理

- 软锁：记录日志、dump 调度统计与 CPU 任务，不停止系统；
- 硬锁：向 ready mask 主动发送 `CpuBacktrace` IPI → 最多等待 10s → 收集已响应 CPU 快照并标记 missing CPU → cause CPU panic 停机；
- 启动期失败（NMI 不可用 / 武装失败）：记录并禁用对应检测，不阻塞启动。

## 隐私分析

不处理用户数据；dump 输出可能包含内核任务名等内部状态，不面向用户。

## 已知限制

- 10s rendezvous 上限会牺牲完全失联 CPU 的快照，但保留 `NOT RESPONDING`、`arrived_mask` 和 `missing_mask` 证据；
- hardlockup 依赖 NMI 周期精度（平台 PMU 后端当前按固定 2.5GHz 折算周期阈值，见 `platforms/kplat-aarch64/src/peripherals/pmu.rs`；代码 TODO 为改读 DT OPP 频率）；
- `usize::BITS` 位图限制：CPU 数 ≥ 64 时 `publish_arrived` 直接忽略（`all_arrived_mask` 处理到位宽上限）；
- `reset()` 与仍在 NMI 自旋的 CPU 并发时语义需谨慎（注释已说明）。

## 审计清单

- [ ] NMI 回调路径是否存在普通 IRQ 自旋锁 / 阻塞调用？
- [ ] per-CPU 状态访问是否都有迁移保护（固定核 / IRQ 关闭 / NMI）？
- [ ] rendezvous 状态迁移是否全部使用正确原子序？
- [ ] NMI 内跨核请求是否仍为无分配、无日志锁、无 GIC 普通锁的寄存器发送？
- [ ] 所有等待路径是否仍有 10s 上限并按实际 arrived mask dump？
- [ ] session 是否有 `nmi_begin()` 保护，同 CPU 嵌套 writer 是否有 `try_join()` 保护？
- [ ] NMI 不可用 / 武装失败路径是否都显式记录并降级？
