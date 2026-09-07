# io/watchdog — 软/硬锁检测看门狗设计文档

## 定位

`io/watchdog` 提供 X-Kernel 的软锁（softlockup）与硬锁（hardlockup）检测：

- 软锁：每 CPU watchdog 任务周期性更新时间戳，定时器回调检查时间戳是否过期；
- 硬锁：依赖 NMI（`nmi` feature）周期检查本 CPU 定时器中断计数是否仍在前进；检测失败后通过语义化 `CpuBacktrace` IPI 主动唤醒已就绪 CPU，执行有界全局 rendezvous、收集快照并 panic。

依赖：`khal`（周期 NMI / percpu / 时间）、`kipi`（读取 CPU-backtrace readiness 与精确 mask 投递）、`ktask`（任务、定时器回调、快照）、`ktime_types`。backtrace 注册/每 CPU 启停由 runtime/kipi 独立管理。

## 背景

中断被长时间屏蔽或任务长时间不调度时，系统可能处于不可响应状态。软锁检测依赖调度与定时器本身（可能受影响）；硬锁检测依赖独立于普通 IRQ 的 NMI 源，能在普通中断完全停摆时仍然触发。X-Kernel 以 PMU 周期计数器作为 NMI 源（经 `kplat::NmiPeriodic`）。KVM 可在 vCPU 执行 WFI 时停止虚拟 PMU 计数，因此失败 CPU 不能只等其他 CPU 的下一次本地 PMU overflow；watchdog 通过 `kipi` 请求 `CpuBacktrace`，不选择裸 SGI。AArch64 GIC 后端拥有具体 SGI 分配并从 NMI 上下文精确投递 ready CPU mask，同时保留 10s 超时降级。

## 范围

- `src/init.rs`：`init_primary` / `init_secondary` / `init_common` / `init_nmi_watchdog` / `init_softlockup_detection`；
- `src/lockup_detection.rs`：每 CPU `LockupDetection` 状态、阈值、`WatchdogTask` 实现；
- `src/watchdog_task.rs`：`WatchdogTask` trait、每 CPU 任务队列、互斥死锁检查；
- `src/rendezvous.rs`：失败触发的全局 rendezvous 状态机；
- `src/lib.rs`：导出。

## 架构

```text
                  定时器中断                     NMI（PMU 周期源）
                     │                               │
        ┌────────────┴───────────┐          ┌────────┴────────┐
        ▼                        ▼          ▼                 ▼
  touch/check_softlockup     timer_tick   check_watchdog_tasks
  （watchdog 任务 + 定时器） （hardlockup 计数） （NMI 上下文）
        │                        │                │
        └────────────────────────┴────────────────┤
                        检测失败                    │
                             ▼                    ▼
                      软锁：log + dump       cause 发布 Triggered
                                                    │
                             CpuBacktrace IPI ──────┼──▶ ready peer CPU/WFI vCPU
                                                    ▼
                                      最多等待 10s，按 arrived_mask
                                      dump；missing CPU 单独报告 → panic
```

## 调用约束 / 执行上下文

| 入口 | 执行上下文 | 约束 |
|------|------------|------|
| `init_primary` / `init_secondary` | 主核 / 从核启动 | 每 CPU 一次 |
| runtime `init_interrupt` / `init_cpu_ipis` | 全局/每 CPU 启动期 | 独立于 watchdog 注册、使能 `CpuBacktrace`；watchdog 不管理 IRQ 生命周期 |
| `init_nmi_watchdog` | 每 CPU 启动期（NMI feature） | 检查 `mode()` 并注册检测任务、武装周期源；`None` 时禁用 hardlockup |
| `try_handle_cpu_backtrace` | IRQ/NMI 回调 | 仅消费活跃 terminal rendezvous；无 session 返回 false，交回 runtime 普通本地 backtrace |
| PMU / backtrace IPI 回调 | NMI 或 regular-IRQ fallback | NMI 路径只允许原子、快照、原子打印与无锁 IPI 发送；禁止普通 IRQ 自旋锁 |
| 定时器回调（`timer_tick` / 软锁检查） | 定时器中断，IRQ 关闭 | 每 CPU 原子访问 |
| watchdog 任务 | 每 CPU 固定核任务 | 只写软锁时间戳、睡眠 4s |

## 状态机

### hardlockup 检测（每 CPU）

`hrtimer_interrupts`（定时器中断递增）与 `hrtimer_interrupts_saved`（上次 NMI 检查值）比较：计数未前进且已初始化 → hardlockup 条件成立（`check_hardlockup` 返回 `true`；`WatchdogTask::check` 对其取反，健康任务返回 `true`）。

### 全局 rendezvous

```text
Idle ──try_trigger（首个失败 CPU 原子迁移）──▶ Triggered
Triggered ──cause 向 ready mask 发送 CpuBacktrace IPI──▶ peer CPU（可从 WFI 唤醒）
Triggered ──各 CPU NMI 中 publish_arrived──▶（全部到达或等待 10s）
Triggered ──cause CPU 收集/打印快照后 mark_dump_done──▶ DumpDone
DumpDone ──cause CPU panic（系统停止）──▶ 终态
```

`reset()` 提供返回 Idle 的路径；只能在 `mark_dump_done()` 且所有 responder 已观察释放之后使用。

## 算法流程

### NMI hardlockup 检查（每次 NMI）

1. `check_watchdog_tasks()`：遍历本 CPU 队列（`HardLockupDetection`、`MutexDeadlock`），失败则返回任务名；
2. 有失败：`ktask::snapshot::nmi_begin()` 成功则 `rv::try_trigger()`（CAS Idle→Triggered，成功者成为 cause CPU）；cause 从 `kipi::cpu_backtrace_ready_mask()` 取得一次性快照，排除自身后调用 `send_cpu_backtrace(mask)`；
3. 其他 CPU 可由 backtrace IPI 或自身周期 PMU NMI 进入 `handle_nmi_rendezvous`；先通过 `try_join()` 认领本 CPU 唯一写者，再执行 `nmi_collect_local()`，最后以 Release 语义 `publish_arrived`。普通 IRQ fallback 被 PMU NMI 抢占时，嵌套进入者立即返回，不重写、不提前发布、不等待外层 writer；
4. cause CPU 仍以所有 present CPU 计算 `expected`，不因 IPI 未就绪而将 CPU 移出等待集；因此注册/使能失败的 CPU 仍有机会通过周期 PMU NMI 响应。先打印首条故障信息，再调用 `wait_all_arrived(expected, 10s)`；全部到达则立即继续，否则超时打印 `arrived_mask` / `missing_mask`；
5. cause 只把实际 `arrived` 位图交给 `nmi_dump_all`（未响应 CPU 由 snapshot 标记 `NOT RESPONDING`），然后 `mark_dump_done`、panic；其余 responder 自旋等待 `is_dump_done`。终态不调用 `nmi_finish()` 清槽，因为超时边界上可能仍有已观察 Triggered、尚未发布 arrived 的迟到 writer；panic/power-off 前保持 session active 可避免并发清理竞态。

### 软锁检测

- watchdog 任务（每 CPU，固定核）每 4s `touch_softlockup`；
- 定时器回调每 tick `timer_tick()` + 检查 `now - soft_timestamp > 20s`，超限则 log + `dump_sched_stats` + `dump_cpu_tasks`，并按阈值限速。

## 并发模型

- `LockupDetection` 为每 CPU 状态，字段为原子量（`AtomicU64` / `AtomicU32` / `AtomicBool`），NMI 与定时器中断并发访问安全；
- `WATCHDOG_TASK_QUEUE` 为每 CPU `Vec`，注册仅在 per-CPU init（迁移不可能），NMI 中只读遍历；
- rendezvous 用全局原子（`PHASE` / `CAUSE_CPU` / `CLAIMED_BITMAP` / `ARRIVED_BITMAP`）+ 有界自旋，NMI 上下文中不取锁；认领位与到达位分离，确保一个 CPU 的快照只写一次且写完后才可读；
- `kipi` 在启动期完成 backtrace handler 注册、每 CPU NMI 属性配置/使能与 readiness 发布；故障路径在 `Triggered` 发布后由 GIC 后端执行 DSB，再按精确 affinity 写 `ICC_SGI1R_EL1`，不获取普通 GIC 锁；
- NMI 路径绝不触碰普通 IRQ 自旋锁（避免 pseudo-NMI 抢占持锁路径造成同 CPU 自死锁）。

## 设计决策

- **阈值**：软锁 20s、硬锁 10s；watchdog 任务 4s 触碰一次，20s 阈值内 5 次机会，避免误报；
- **主动 + 有界 rendezvous**：像 Linux NMI backtrace 一样，cause 主动向其他 CPU 发送 NMI 请求，并最多等待 10s；诊断完整性不能以把所有 responder 永久困在 NMI 为代价；
- **IPI 失败可降级**：NMI 提升失败时保留 regular-IRQ fallback；平台无语义向量或每 CPU enable 失败时 readiness 不发布，投递失败由 cause 原子输出；周期 NMI 仍可自然加入 rendezvous，最终受 10s 上限保护。平台声称支持却全局注册失败属于 runtime 启动错误；
- **IRQ/IPI ownership**：watchdog 只使用 `CpuBacktrace` 语义；`kipi` 拥有 handler/readiness 生命周期，irqchip 拥有实际 SGI/vector，避免配置层裸 IRQ 号泄漏到诊断逻辑；
- **NMI 机制不可用时显式降级**：启动日志提示 hardlockup 关闭，不静默失败；
- **快照重入保护**：`nmi_begin()` 排斥并发 session；`try_join()` 另行排斥同 CPU 的 IRQ/NMI 嵌套 writer；
- **软锁报告限速**：每阈值周期最多报告一次。

## Drop / 资源释放

每 CPU 状态与内核同生命周期，无运行期释放；`reset()` 仅在需要重启 rendezvous 时使用。
