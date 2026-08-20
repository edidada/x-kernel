# timer-driver — 设计文档

## 定位

`timer-driver` 为 X-Kernel 的各架构提供单调时钟和本地定时器后端。平台初始化代码把该
crate 接入 `khal::time::MonotonicTimerIf`，时间子系统据此读取 tick、换算时间跨度并设置
下一次本地定时器中断。

## 背景

x86_64 使用 TSC 作为单调计数器，TSC 频率决定全部 tick 与纳秒换算。CPUID 叶 0x16
报告处理器标称基频，该数值与 TSC 频率没有等价关系。虚拟化环境还会暴露经过缩放的
TSC。x86_64 后端只在 CPU 报告 invariant TSC 后才采用 CPUID 叶 0x15 或 PIT
实测频率；未报告该能力时直接使用平台静态频率，避免把变速计数器装成固定比例单调时钟。

## 范围

- `src/lib.rs`：跨架构频率来源类型和模块入口。
- `src/x86_lapic_tsc.rs`：x86_64 TSC 校准、时间换算和 LAPIC one-shot 定时器。
- `src/arm_generic.rs`：AArch64 generic timer 后端。
- `src/riscv_sbi.rs`：RISC-V SBI timer 后端。

本次频率校准逻辑仅影响 x86_64。

## 架构

```text
platform early_driver_init
          │ TimerConfig
          ▼
  x86_lapic_tsc::early_init
          │
          ├─ no invariant TSC → platform fallback
          ├─ CPUID 0x15 TSC ratio
          ├─ PIT channel 2 calibration (cross-check / fallback)
          └─ platform fallback
          │
          ▼
 TSC frequency + fixed-point ratios
          │
          ├─ MonotonicTimerIf::now_ticks / ticks_to_span
          └─ LAPIC one-shot deadline programming
```

## 调用约束 / 执行上下文

- `early_init` 由 bootstrap CPU 在早期启动阶段调用一次，此时其他 CPU 尚未并发使用
  PIT channel 2。
- 报告 invariant TSC 后执行三轮约 50 ms 的 PIT 忙等待，路径不能睡眠，也不依赖任务
  上下文。未报告该能力时跳过 PIT。
- 每轮 PIT 等待受 TSC tick 阈值限制，失效设备不会永久阻塞启动。
- `init_primary` 在 bootstrap CPU 的 LAPIC 初始化完成后调用。
- `init_secondary` 在当前 secondary CPU 的 LAPIC 初始化完成后调用。
- `now_ticks`、`freq`、时间换算和 `arm_timer` 只能在 `early_init` 成功完成后调用。

## 算法流程

### TSC 频率选择

1. 检查 CPUID 叶 0x80000007 的 invariant TSC 能力。未报告该能力时记录警告，并直接
   使用 `TimerConfig` 中的平台频率。TCG qemu64 等环境会省略 invariant TSC，此时
   0x15 或 PIT 仍可能给出范围内的频率，但会把变速计数器装成固定比例时钟，使
   `CLOCK_MONOTONIC` 相对历史 4 GHz 回退明显变快。
2. 读取 CPUID 叶 0x15，通过 crystal frequency、numerator 和 denominator 计算 TSC
   频率。频率必须落在 1 MHz 到 100 GHz 范围内才视为有效。
3. 无论叶 0x15 是否有效，都把 PIT channel 2 设置为 mode 0，并装入 59,659 个
   PIT tick。该区间接近 50 ms。
4. 在 PIT output 由低变高的区间读取 TSC 差值，按
   `delta_tsc * 1_193_182 / 59_659` 计算频率。
5. 尝试三轮测量，跳过超时或无效的单轮样本，并采用其余样本中的最小值，降低中断、
   VMM 调度和固件停顿造成的正向误差。
6. 0x15 与 PIT 都有效且相对误差不超过 10% 时采用 0x15；相对误差超过 10% 时采用
   PIT，并记录警告。只有一侧有效时采用该侧。两侧都失败时使用平台频率。

### 时间换算

选定频率以 Hz 保存，同时取 kHz 构造 `int_ratio::Ratio`。kHz 精度让 3 GHz 级 TSC
保留亚 ppm 量级的换算分辨率，也满足 `Ratio` 的 `u32` 参数范围。

- tick 转纳秒使用 `1_000_000 / frequency_khz`。
- 纳秒转 tick 使用 `frequency_khz / 1_000_000`。
- `INIT_TICK` 保存早期初始化末尾的 TSC 值，运行期对外暴露相对 tick。

## 并发模型

`early_init` 单次写入 `TSC_FREQUENCY_HZ` 和 `INIT_TICK`，两个换算比例由
`klazy::Once` 发布。运行期读取使用 relaxed atomic，初始化顺序由平台启动状态机保证。
每个 CPU 的 LAPIC 寄存器通过 `x86_apic::with_local_apic` 在本地锁保护下访问。

## 设计决策

### 未报告 invariant TSC 时不校准

没有恒定速率保证时，把 0x15 或 PIT 测到的瞬时速率写成固定换算比例，会让超时、
调度和用户态 `CLOCK_MONOTONIC` 整体漂移。平台静态频率与历史 x86 回退一致，也让
TCG qemu64 保持原先的时间尺度。KVM 或 `-cpu host` 通常报告 invariant TSC，仍走
0x15 / PIT。

### CPUID 叶 0x15 表示 TSC 频率

叶 0x15 的 numerator、denominator 和 crystal frequency 共同描述 TSC 比率，适合直接
建立时间换算。叶 0x16 的 base frequency 描述处理器运行频率，动态频率和 TSC
计数速率可以不同，因此不进入 TSC 频率选择。该叶与 PIT 相差超过 10% 时视为不可信。

### PIT channel 2 作为交叉校验和早期启动后备

PIT 是 x86 PC 启动环境中的独立固定频率计数器，QEMU 和常见 x86 虚拟机均提供该
接口。channel 2 不占用系统时钟中断通道，校准结束后恢复 0x61 端口原值。invariant
TSC 存在时始终测量 PIT：与 0x15 交叉校验，并在该叶缺失时提供实测频率。

### 平台静态值保留为最终后备

缺少 invariant TSC，或 CPUID 与 PIT 都不可用时，系统仍能继续启动。日志明确标记
静态来源，便于发现时间精度退化。

## Drop / 资源释放

PIT channel 2 只在 `early_init` 中临时使用，每轮测量结束均恢复 speaker 和 gate
控制值。TSC 与 LAPIC 属于 CPU 固有资源，不参与动态释放。
