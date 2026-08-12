# timer-driver — 安全文档

## 信任模型

`timer-driver` 信任 CPU 架构提供的计数器单调性、平台本地中断控制器和早期启动调用
顺序。虚拟化环境提供的 CPUID、TSC、PIT 和 LAPIC 均属于 VMM 控制的外部输入，错误
频率会影响超时、调度和网络协议计时。

## 外部边界 / 攻击面

- CPUID 叶 0x15 的 TSC 比率和 crystal frequency。
- PIT channel 2 的数据、命令、gate 和 output 状态。
- 平台静态 `TIMER_FREQUENCY_HZ` 配置。
- LAPIC timer 寄存器和中断向量。
- 多 vCPU 之间的 TSC 同步与迁移行为。

## unsafe 代码清单

### TSC 读取

`early_init` 和 `read_tsc_ticks_raw` 调用 x86_64 `_rdtsc` intrinsic。调用点限定在
x86_64 模块，CPU 架构保证该指令存在。`INIT_TICK` 在运行期读取前完成初始化。

### PIT port I/O

`measure_tsc_with_pit` 访问端口 0x42、0x43 和 0x61。安全依据包含以下条件：

- 调用发生在 bootstrap CPU 的早期启动阶段；
- 其他执行路径尚未使用 PIT channel 2；
- 端口宽度均为 8 bit；
- channel 2 的原始 speaker 和 gate 值在测量结束前恢复；
- 超时路径同样执行恢复操作。

### LAPIC 访问

`init_primary`、`init_secondary` 和 `arm_timer` 通过
`x86_apic::with_local_apic` 修改当前 CPU 的 LAPIC。平台启动顺序保证 LAPIC 已初始化，
helper 持有本地 LAPIC 锁并限制可变访问范围。

## 内存安全不变量

1. `early_init` 在任何时间换算 API 前执行一次。
2. `TSC_TICKS_TO_NANOS_RATIO` 和 `NANOS_TO_TSC_TICKS_RATIO` 使用同一选定频率初始化。
3. PIT 控制端口在正常完成和超时返回前恢复原值。
4. LAPIC 编程仅作用于当前 CPU 已初始化的本地寄存器。

## 线程安全

- 早期 TSC 和 PIT 初始化限制在 bootstrap CPU，过程不可重入。
- 频率和基准 tick 在初始化后只读，atomic 负责无数据竞争访问。
- 换算比例由 `klazy::Once` 单次发布。
- LAPIC 状态属于当前 CPU，访问由 `with_local_apic` 串行化。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | VMM 提供错误但处于范围内的 CPUID TSC 频率 | 高 | CPUID 叶 0x15 内容与实际 TSC 速率不一致 | 记录来源和频率；运行环境需要保证虚拟 CPU 架构契约 |
| T-02 | PIT 永久不置 output 位 | 高 | VMM 缺失 PIT 或设备状态损坏 | TSC tick 阈值和最大轮询次数共同终止等待，随后使用平台值 |
| T-03 | PIT 测量受到 VMM 调度停顿 | 中 | vCPU 在测量区间被抢占 | 执行三轮并采用最小有效频率，降低正向误差 |
| T-04 | 多 vCPU TSC 不同步 | 高 | VMM 未提供同步 TSC，任务跨 CPU 迁移 | 当前依赖平台同步 TSC 契约；部署环境需要保证 invariant TSC 和 vCPU 同步 |
| T-05 | PIT channel 2 被并发使用 | 中 | 后续调用方绕过早期启动约束 | API 保持私有，平台只在 bootstrap CPU 早期调用一次 |

## 故障模式与影响分析

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | CPUID 叶 0x15 缺失 | CPU 或 VMM 未实现该叶 | 无架构频率值 | 进入 PIT 校准 | 4 | 自动切换到 PIT |
| F-02 | PIT 校准超时 | PIT 缺失、output 不变或 port I/O 故障 | 无实测频率 | 使用平台静态值，时间精度由配置决定 | 2 | 有界等待并记录警告 |
| F-03 | PIT 样本频率越界 | 设备异常或测量区间失真 | 当前样本丢弃 | 所有样本失效时使用平台值 | 3 | 只接受 1 MHz 到 100 GHz |
| F-04 | 平台静态频率错误 | Kconfig 配置与运行环境不一致 | tick 换算比例错误 | 超时、调度和协议计时整体漂移 | 2 | 优先使用 CPUID 或 PIT，fallback 日志暴露来源 |
| F-05 | LAPIC timer 编程早于初始化 | 平台启动顺序错误 | 无效寄存器访问 | 启动失败或定时器中断缺失 | 1 | BootHandler 在 LAPIC 初始化后调用对应入口 |

## 故障管理

- CPUID 字段缺失和 PIT 失效均进入后备路径。
- PIT 超时和平台 fallback 记录 warning，最终频率以 info 日志输出。
- 平台 fallback 超出 1 MHz 到 100 GHz 范围或换算比例超出表示范围时触发启动期断言。
- LAPIC 编程错误属于平台初始化不变量破坏，当前没有运行期恢复路径。

## 隐私分析

该模块不处理用户数据。日志只包含计时源、频率和设备校准状态。

## 已知限制

- x86_64 后端尚未使用 KVM pvclock 或 Hyper-V reference TSC page。
- 当前依赖所有 vCPU 的 TSC 同步和恒定速率，尚未执行跨 CPU 偏移校验。
- 合法范围只能过滤明显错误的 CPUID 频率，无法识别处于范围内的恶意频率。
- PIT 校准增加约 150 ms 的 x86_64 启动时间，仅在 CPUID 叶 0x15 无法给出频率时发生。

## 审计清单

- 每个 port I/O、TSC intrinsic 和 LAPIC unsafe 块均有完整 `SAFETY:` 依据。
- PIT 新增退出路径均恢复 0x61 端口原值。
- 频率来源修改同步更新换算比例、日志和 fallback 流程。
- 新增平台调用点保持 bootstrap CPU 单次初始化约束。
- 多 CPU 计时改动验证 TSC 同步、迁移和 suspend/resume 行为。
