# kipi — 安全与可靠性分析

## 信任模型

`kipi` 不是用户态边界模块，它的主要调用者是内核中的页表、I-cache 和
跨 CPU 协调逻辑。信任关系如下：

```text
内核调用者
   │
   │ safe API: run_on_cpu / run_on_each_cpu / run_on_each_cpu_via_ipi
   │           page_table::TlbFlushIf / karch::IcacheFlushIf
   │
   v
┌───────────────────────────────────────────┐
│                   kipi                    │
│  ┌──────────────┐   ┌──────────────────┐  │
│  │ IPI 队列路径 │   │ TLB 协议状态路径 │  │
│  └──────────────┘   └──────────────────┘  │
│       CPU-backtrace handler/readiness      │
└───────────────────────────────────────────┘
   │
   │ kiface / raw per-cpu refs / IRQ backend
   v
平台中断控制器与 CPU 本地执行上下文
```

默认信任前提：

- 调用者在正确的执行阶段使用 `kipi`，即目标 CPU 的本地队列已初始化且投递已使能；
- 平台 `send_ipi()` 实现满足 publish-before-notify 契约，且不获取跨 CPU 共享锁；
- `CpuBacktrace` backend 分支可从 NMI 调用且不会分配、记录日志或获取锁；
- `ipi_handler()` 能在目标 CPU 上被正常投递和执行。

## 外部边界 / 攻击面

`kipi` 不直接接收用户态输入，但它确实跨越了若干高风险边界：

1. **中断控制器边界**  
   通过 `kirq::send_ipi(IpiKind, TargetCpu)` 与 GIC/APIC/SBI 等后端交互，
   依赖其语义向量映射、IPI 投递和发布顺序正确。调用方不拥有 raw IRQ/SGI 号。

2. **per-CPU 原始引用边界**  
   使用 `remote_ref_raw()` / `current_ref_mut_raw()` 访问每 CPU 队列，
   依赖“目标 CPU 的本地队列已经初始化”这一前提。

3. **页表一致性边界**  
   `tlb.rs` 与 `page_table`、`memspace` 的一致性协议耦合，
   一旦请求发布、ack 或重试逻辑出错，可能导致旧 TLB 项继续生效。

4. **中断上下文边界**  
   `ipi_handler()` 和 TLB shootdown handler 运行在不可睡眠上下文中，
   不能持有会阻塞的锁，也不能执行长路径。

5. **回调边界**  
   广播/单播回调由其他 CPU 提供并在目标 CPU 上执行。`kipi` 只保证投递，
   不验证回调逻辑本身是否适合在 IPI 上下文运行。

6. **NMI 诊断边界**
   `send_cpu_backtrace()` 可由 watchdog NMI 调用。`kipi` 只读取原子 readiness
   并传递精确 `KCpuMask`；irqchip 必须兑现无锁投递，consumer 必须有限等待。

本模块不直接涉及：

- 用户内存或用户指针；
- DMA 缓冲区；
- 网络/文件系统外部输入；
- FFI。

## unsafe 代码清单

### 1. `IPI_EVENT_QUEUE.remote_ref_raw()` / `current_ref_mut_raw()`

位置：

- `arch/kipi/src/lib.rs:133`
- `arch/kipi/src/lib.rs:185`
- `arch/kipi/src/lib.rs:254`

不变量：

- 目标 CPU 必须存在于逻辑 CPU 映射中；
- 目标 CPU 的本地 IPI 队列必须已经初始化；
- 当前 CPU 访问 `current_ref_mut_raw()` 时只能操作自己的本地槽位。

防护：

- 对外 API 在远程入队前显式检查 `IPI_QUEUE_READY`；
- 当前 CPU 路径只从本地 handler 访问当前槽位。

### 2. `LAST_HANDLED_PENDING_EPOCH` 的原始 per-CPU 访问

位置：

- `arch/kipi/src/tlb.rs:362`
- `arch/kipi/src/tlb.rs:368`

不变量：

- 只能访问当前 CPU 的本地 slot；
- 不能把别的 CPU 的 epoch 缓存错误地写入当前 CPU 本地状态。

防护：

- helper 只暴露“当前 CPU 读/写”接口，不提供跨 CPU 写路径。

### 3. 平台 `send_ipi()` 中的内联汇编屏障

位置：

- `drivers/platform/irq/src/gicv2.rs`
- `drivers/platform/irq/src/gicv3.rs`
- `drivers/platform/irq/src/riscv.rs`

不变量：

- 屏障必须覆盖“请求状态已写入普通内存”到“IPI 被目标 CPU 观测”之间的顺序；
- 否则目标 CPU 可能先响应 IPI，再看见旧 request 状态。

防护：

- `IntrManagerIf::send_ipi` 现在把该契约写成接口要求；
- AArch64 / RISC-V 后端已在发送路径显式补 barrier。

### 4. GICv3 无锁发送与终止屏蔽

`drivers/platform/irq/src/gicv3.rs::notify_cpu()` 为两类 IPI 共用，直接读
`ICC_CTLR_EL1`、写 `ICC_SGI1R_EL1`。不变量：

- SGI INTID 由 GIC backend 的语义映射提供且 `< 16`；
- logical CPU 必须先通过启动期映射转换为 raw MPIDR affinity；
- Aff0 超过 15 时，只有 `ICC_CTLR_EL1.RSS` 与启动期缓存的 `GICD_TYPER.RSS`
  同时支持 range selector 才编码，防止 RS 被忽略后误发到其他 CPU；
- 路径不得触碰普通 `GIC` lock、allocator 或 logger；
- DSB 位于第一次 SGI 写之前，ISB 位于最后一次写之后。

任一目标无法表达时跳过该目标并返回 `false`，由调用方的确认协议报告缺失。
CpuBacktrace 只接受精确 mask；Generic 保留 all-but-self 广播。

`karch::prepare_cpu_stop()` 的 AArch64 实现在 `aarch64/irq.rs` 全掩 DAIF，并只在
本 CPU `allint_active()` 为真时写 ALLINT。该标志由启动期硬件能力检查发布；
终止后不恢复中断，因此无需修改共享 GIC 配置即可阻止迟到的 backtrace NMI。

## 内存安全不变量

以下不变量对 `kipi` 的健壮性和内存安全都关键：

1. **每 CPU 队列必须先初始化再被远程访问**  
   `remote_ref_raw()` 没有运行时恢复能力，前置条件错了就是逻辑错误。

2. **TLB 请求必须先 publish，再允许目标 CPU 看到 IPI**  
   否则目标 CPU 可能跳过 request，发起 CPU 永久等不到 ack。

3. **同一发起 CPU 的 request slot 在 active 生命周期内不可被另一个任务复用**  
   这由 `ActiveShootdownSlot` 的 no-preempt guard 维持。

4. **TLB shootdown 不能在 flush 路径内重建 residency 状态**  
   residency 由调度切换路径维护；flush 路径只允许消费一个保守快照。

5. **pending epoch 只能作为 fast path gate，不能替代真实 ack 协议**  
   epoch 只决定“要不要扫描”，真正的完成条件仍是 `acked_seq_by_cpu`。

6. **CPU-backtrace readiness 必须晚于本 CPU vector enable**
   `CPU_BACKTRACE_DELIVERY[cpu]` 用 Release 同时发布 mode/readiness，发送方用 Acquire
   形成精确 mask。终端停机先屏蔽 CPU 本地中断，再发布 `Unavailable`；旧 sender
   仍可能发出请求，handler 必须检查本地 delivery，不能把 readiness 当作同步完成证明。

7. **最终停机不得进入普通 IRQ 控制平面**
   在已经失去响应的 CPU 可能持锁时，发送和本地清理都不能等待该锁。
   终止清理只使用 CPU 屏蔽、周期源停止与 readiness 原子撤销；不调用持有
   IRQ 管理锁的控制接口。
   `khal::power` 的外层抢占保护覆盖 provider 返回后的平台终点。

8. **终端撤销 readiness 不允许回收存储**
   Generic sender 可能在撤销 readiness 后完成已经通过校验的入队；队列、handler、
   映射与 NMI 属性保留到内核终止，撤销后不再恢复投递。
   per-CPU IRQ 启用不修改共享 disable depth；`NoPreemptIrqSave` 排除本地迁移和
   普通 IRQ 启用重入，不支持 NMI 内的控制操作或完整 CPU hotplug 回收。
   readiness 撤销不会把 CPU 移出 scheduler/mm target set，仅用于最终停机协议。

## 线程安全

### 通用 IPI 队列

- 每个目标 CPU 的队列是多生产者、单消费者模型；
- 生产者通过 `SpinNoIrq` 串行化入队；
- 消费者只在目标 CPU 的 IPI handler 上 pop。

### TLB shootdown

- `REQUEST_SLOTS` 按发起 CPU 分片，避免 initiator 间共享单个全局 pending 状态；
- `acked_seq_by_cpu` 按目标 CPU 维度记录确认进度；
- `PENDING_EPOCH_BY_CPU` 只是降低无关 IPI 的扫描成本，不改变协议正确性；
- `needs_retry_full_flush` 用于同 CPU 嵌套请求的兜底补发，避免直接重入复用 slot。

## 威胁分析

| 编号 | 威胁 | 触发条件 | 影响 | 缓解 |
|------|------|----------|------|------|
| T-01 | 目标 CPU 在观察到 request 状态前先处理 IPI | 平台 `send_ipi()` 缺少发布屏障 | TLB shootdown 丢失，发起 CPU 自旋等待 | 在 IRQ backend 中定义并实现 publish-before-notify 契约 |
| T-02 | 同一 CPU 上两个任务重入复用同一个 request slot | `flush_remote()` 生命周期内允许抢占 | request 状态被覆盖，可能死锁或漏 flush | `ActiveShootdownSlot` 绑定 no-preempt 与 active slot |
| T-02a | 两核 `flush_remote` 互等 ack | 等待时 IRQ 关闭，不处理对方 shootdown | libc-test 等并行 unmap 整机卡住 | 等待循环轮询 `handle_shootdown()`，不依赖本核已进 IPI handler |
| T-03 | 普通 IPI 共用同一向量导致无意义全表扫描 | 每次 IPI 都无条件扫描 TLB slots | 高频无关 IPI 造成 O(CPU_NUM) 热路径开销 | per-target `pending_epoch` fast path |
| T-04 | 调用者把不适合中断上下文的逻辑作为回调广播到其它 CPU | 回调内部睡眠、拿大锁或跑长路径 | IPI handler 延迟、锁顺序问题、可用性下降 | `kipi` 文档约束回调必须适合该执行上下文；模块不兜底 |
| T-05 | 队列未初始化前被远程入队 | AP 尚未执行 `init()`，但已经成为 IPI 目标 | 通过原始 per-CPU 引用访问未就绪槽位 | API 显式检查 `IPI_QUEUE_READY`，失败返回 `TargetCpuNotReady` |
| T-06 | 停机 IPI 无法到达关中断自旋的 CPU | 目标 CPU 长期屏蔽本地中断，永远不进入 `ipi_handler` | 该 CPU 未停靠，终点前仍在变更共享状态 | 1s 有界超时后告警并继续（对齐 Linux `smp_send_stop`），不阻塞终点 |
| T-07 | NMI 看门狗唤醒停靠 CPU 或误判故意停机为 lockup | park 后周期 NMI 或 backtrace IPI 仍开启 | 停靠 CPU 被反复唤醒；看门狗 panic 后走平台断电，`halt` 变成 `power_off` | ACK 前停掉周期源、撤销 Generic/CpuBacktrace readiness 并禁用本地投递；orchestrator 返回前同样清理 |
| T-08 | 停机编排者任务在协议中途被迁移 | 从可抢占的 syscall 上下文进入终点，认领 id 与广播/等待基于过期的 CPU id | 被离开的 CPU 被当作 orchestrator 永不 park，协议必然吃满超时并留下运行中的 CPU | `stop_other_cpus` 全程持有 `NoPreempt` 守卫，把编排者钉在认领的 CPU 上 |
| T-09 | 等待集包含 IPI 未使能的 present CPU | AP 尚未执行 `kipi::enable_ipi()` 即触发停机 | 该 CPU 无法及时 ack，每次停机固定白等 1s 超时并误报告警 | 只等待发布时 Generic 已就绪的 CPU；保留 `AllButSelf` 广播，使晚启动 CPU 仍可处理 pending stop IPI |
| T-10 | watchdog/consumer 选择裸 SGI，与 Generic IPI 冲突 | raw IRQ 号越过 IPI ownership boundary | handler 错配、无法唤醒或普通 handler 在 NMI 重入 | consumer 只见语义类型；所有架构移除 `IPI_IRQ`，由 backend 固定保留并复用映射 |
| T-11 | CPU-backtrace NMI 发送获取普通 GIC lock | NMI 抢占持锁的普通 IRQ/IPI 路径 | 同 CPU 自死锁 | GICv3 共用 SGI1R 编码路径无锁、无日志、无分配；backtrace 入口只接受精确 mask |
| T-12 | 共享启用状态跳过其它 CPU 的本地配置 | 将 per-CPU 启用替换为普通 IRQ 启用接口 | 部分 CPU 不能接收 IPI | 专用 per-CPU 启用每次配置本地 backend；readiness 独立发布，拒绝非 PER_CPU descriptor |
| T-13 | 停机发送或清理等待失去响应的 CPU 所持有的锁 | peer 卡在 IRQ/GIC 控制或 console 锁内 | 超时后仍无法进入平台终点 | SGI 无共享锁发送；CPU 本地终止屏蔽与 readiness 撤销；原子输出超时诊断 |
| T-13 | 关闭 watchdog 连带丢失 backtrace 能力 | handler 注册依附 PMU/watchdog 初始化 | 普通诊断无法唤醒其他 CPU | runtime 独立注册/每 CPU 使能；无 NMI feature 时验证 RegularIrq 投递 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 局部影响 | 系统影响 | 检测/缓解 |
|------|----------|----------|----------|-----------|
| F-01 | 目标 CPU IPI 队列未初始化 | `run_on_cpu` / 广播失败 | 相关跨核操作无法执行 | 返回 `KipiError::TargetCpuNotReady` |
| F-02 | IPI 队列塞满 | 当前回调无法投递 | 局部控制流失败，上层需自行恢复 | 返回 `QueueFull` |
| F-03 | 目标 CPU 长时间不 ack TLB request | 发起 CPU 自旋等待 | 页表修改路径停顿，可能表现为系统卡住 | 超时后记录 warning；等待中轮询 `handle_shootdown` 避免 IRQ-off 互等 |
| F-04 | pending epoch 未同步更新 | 目标 CPU fast path 直接返回，不扫描 slots | 单测或协议调用错误时出现假阴性 / 卡住 | 正式路径由 `flush_remote()` 封装；单测通过 helper 同步更新 |
| F-05 | callback panic | 当前回调失败 | 其它已排队回调仍继续处理 | `ipi_handler()` 继续消费队列，不让单个回调中断整个队列 |
| F-06 | backtrace vector 无法提升为 NMI | 平台缺能力或 per-CPU 属性配置失败 | 仅能打断开普通 IRQ 的 CPU | `register_nmi` regular action fallback + delivery 枚举；watchdog 10s 有界等待 |
| F-07 | 精确 mask 仅部分发送成功 | raw CPU 映射缺失或 RSS 不支持 | 部分 CPU 快照缺失 | backend 返回 `false`；watchdog 保留 expected mask 并在超时报告 missing CPU |

## 故障管理

- 通用回调的入队前校验以 `Result` 返回错误；已经入队后的通知保持原有
  fire-and-forget 语义，不把无法安全撤销的回调伪装成未提交；
- TLB shootdown 的主要失败模式不是 `Result`，而是“目标 CPU 不 ack”，因此采用
  自旋等待 + 超时 warning 的诊断模型；
- 同 CPU 嵌套 TLB 请求不直接 panic，而是设置 `needs_retry_full_flush` 并在当前请求结束后补发；
- 对平台 IPI 顺序语义的要求被写入接口契约，避免隐式依赖散落在调用点。

## 隐私分析

`kipi` 不处理用户隐私数据，也不直接接触用户缓冲区、文件内容或网络载荷。  
它处理的内容主要是：

- CPU ID；
- 回调对象；
- TLB shootdown 协议状态；
- 诊断日志中的地址和 request 序号。

需要注意的是，诊断日志可能包含虚拟地址或 CPU 拓扑信息，因此仍应视为内核内部调试信息。

## 已知限制

- 通用 IPI 回调和 TLB shootdown 共用一个 IPI 向量，设计上要求 handler 顺序稳定；
- TLB shootdown 在等待 ack 时采用自旋，不适合极长延迟目标；
- `kipi` 只保证跨 CPU 投递，不保证回调本身适合在 IPI 中断上下文运行；
- CPU-backtrace 的 NMI-safe 主动投递目前只由 AArch64 GICv3 backend 提供；
- CPU-backtrace / Generic 只提供每核启用和终端停机清理，不提供运行期间的可逆禁用、
  handler unregister、队列回收或完整 CPU hotplug；
- LoongArch 的 secondary boot mailbox 不等于 runtime IPI，当前显式限制为单核；
- `pending_epoch` 只是性能优化门槛，不是完整协议状态，因此测试若直接操作内部 slot，
  必须同步更新该 gate。

## 审计清单

- 检查所有新增 `send_ipi()` 调用点是否依赖 publish-before-notify 语义。
- 检查是否有任何路径在 `ActiveShootdownSlot` 生命周期内进入可能睡眠的代码。
- 检查 `flush_remote` 等待 ack 时是否轮询 `handle_shootdown`（IRQ 关闭下两核互等）。
- 检查任何新测试若手工构造 request slot 状态，是否同步更新 `pending_epoch`。
- 检查新的广播回调是否真的能在 IPI handler 上下文执行，而不会睡眠或长时间占用 CPU。
- 检查新平台 IRQ backend 是否兑现 `IntrManagerIf::send_ipi` 的顺序契约；
  `CpuBacktrace` 分支还必须保持 NMI-safe 且不扩大精确目标 mask。
