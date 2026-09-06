# ktime_types - 设计文档

## 定位

`ktime_types` 提供内核内部统一的语义时间类型。它不读取时钟，也不编程硬件。
`khal::time` 提供单调硬件时钟，`ktime`（timekeeping 层）负责 monotonic 与
realtime 的关联。

## 范围

- `src/span.rs`：`TimeSpan` 非负时间长度。
- `src/instant.rs`：由 marker 类型区分的 `Instant<C>` 时钟域时刻。
- `src/system_time.rs`：带符号的 Unix 墙钟时间。
- `src/units.rs`：秒、毫秒、微秒和纳秒之间的固定单位换算常量。
- `src/timestamp.rs`：持久化时间戳的范围和粒度约束。
- `verification/`：独立验证 crate、证明与测试，位于业务 src 之外。
- `src/lib.rs`：稳定的 crate 根公开重导出，不承载具体类型实现。

## 架构

```text
hardware counter -> khal::time ----+-> MonotonicInstant
RTC sample ------------------------+-> SystemTime
                                      |
                                      +-> TimeSpan arithmetic
```

`MonotonicInstant`、`BoottimeInstant`、`ProcessCpuInstant` 和
`ThreadCpuInstant` 使用不同 marker，禁止跨时钟域比较或相减。`core::time::Duration`
只通过 `TimeSpan::from_core` 和 `TimeSpan::into_core` 显式转换。

## 调用约束 / 执行上下文

所有类型都是纯值类型，不分配、不阻塞，可用于中断、早期启动和普通任务上下文。
从整数、ABI 或硬件表示构造时间值应只发生在相应边界。
反向转换同样只允许发生在硬件寄存器、ABI、序列化或第三方接口边界；
`as_nanos_u64_saturating` 明确表达无法表示时钳位到 `u64::MAX` 的策略。

## 设计决策

- `TimeSpan` 不作为 `Duration` 的别名，避免依赖代码无意混用两套 duration。
- `Instant<C>` 的泛型时钟域在编译期阻止 realtime、monotonic 和 CPU time 混用。
- `SystemTime` 保存规范化的秒和纳秒，允许表达 Unix epoch 之前的时间。
- `SystemTime::{MIN, MAX}` 统一定义墙钟类型的可表示边界。
- `SystemTime` 的 checked 加减直接处理秒和纳秒分量，避免高频墙钟路径经过
  `i128` 总纳秒的乘除规范化。
- checked 和 saturating 运算供外部输入及非严格排序路径使用；运算符只用于已建立顺序和范围不变量的内部路径。

## 形式化验证

Kani 在 `verification/` 独立 crate 中通过 path dependency 编译真实生产库，不复制或替换实现。
当前范围包括 `TimestampLimits`、`TimeSpan`、`SystemTime`、`SystemTimeError`
及四个公有时钟域 `Instant<C>` 的显式数值 API。
`TimeSpan` 规格以 `u128` 总纳秒表达，最大时长的两倍仍可精确表示，
因此可以独立检查秒/纳秒实现的溢出决策和进位借位。
`new` 对可表示的非规范化纳秒执行进位；溢出时 panic。
`SystemTime` 规格以 `i128` 总纳秒表达，包括负时间戳、跨 epoch 运算和
最大正负时间差；误差方向由 `Result` 表达，误差幅度由 `TimeSpan` 表达。
`Instant` 的时钟域隔离另外通过独立的编译失败示例检查，
测试位于 `verification/tests/domains.rs`，由通用工具使用仓库工具链运行。
值 trait 检查包括 Clone/Copy、比较及 Hash 的精确字段写入序列。
clamp 对独立输入值和合法上下界检查精确钳位，对逆序上下界另有全域 panic 审计。
全域 panic 结论需要通用报告审计器审计新生成的报告，
不能只依据 Kani 的 `should_panic` 成功标记。
默认 Debug 已检查完整合法数值域上的非空输出、长度上界及容量受限写入器的
错误传播；循环界保留展开断言，不作为输入假设。
完整文本及任意格式选项仍未形成全域证明，另有独立格式化回归测试。
证明规格、前提和复现方式见
[contracts.md](../verification/contracts.md)。普通内核构建不编译证明入口。
核心契约的收尾范围与自动验收见 [验证说明](../verification/README.md)。
统一入口为 `python3 scripts/verification/run.py util/ktime_types`；生产源码变化需重新审查并更新验证清单。
