# PR #938 TLB gather：vm-scalability 对比

日期：2026-10-22。此次只比较 #938 原始 HEAD 与其 gather 改进，不混入独立 MM
first-touch 并发 PR。

## 配置与方法

- 基线：`d00fb9494142eb339fc4353b5a64874c9cf15f52`。
- gather：`d65c6c2ab24b7f6e8929d919a80eabb44c7298fa`。
- Harness：`c31d42618376c983e374069f55449178dfb1d48e`，既有 daily vm-scalability 用例。
- AArch64 QEMU TCG，4 CPU，1 GiB RAM；`xkernel-glibc` rootfs。
- rootfs SHA256：`f91e25157dfc8b7e05f71d6fb2a27c31ec8fbd7db91d79dde41ec4f0341476fd`。
- 相同 16 MiB 总预算，4 KiB 映射，1/2/4 worker，各 3 个样本，8-byte touch stride。
- 两版本顺序运行，无并行构建或其他本任务 QEMU，未添加临时内核探针。
- 基线日志：`logs/daily/20261022-110140/`；候选日志：`logs/daily/20261022-111030/`。
  路径相对外部 starry-test-harness。原始 JSON、case log 的本地副本在
  `target/pr938-validation/vm-scalability/`，不提交大体积产物。

执行命令，分别设置 `XKERNEL_REMOTE` 为基线和候选工作树：

```bash
CASES=vm-scalability-anon-w-seq-mt,vm-scalability-anon-w-rand-mt \
ARCH=aarch64 JOBS=1 make daily-test run
```

## 测量结果

总耗时为 harness `USEMEM_SUMMARY` 的每组 3 样本中位数，包含 setup、执行和 wait/reap；
进程 summary 之后最终退出的剩余销毁不包含在其中。释放耗时取 workload 自报的
`usecs to free memory`，在相同 worker 数的所有线程/样本值上取中位数；它不是整个
case 的拆映射总耗时，也不是单独 TLBI 指令耗时。两种数据不可相减推导精确阶段成本。

| 用例 | worker | 基线总 ms | gather总 ms | 总耗时变化 | 基线释放 ms | gather释放 ms | 释放变化 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `anon-w-seq-mt` | 1 | 500.455 | 441.898 | -11.7% | 83.311 | 43.538 | -47.7% |
| `anon-w-seq-mt` | 2 | 439.873 | 382.211 | -13.1% | 42.587 | 22.604 | -46.9% |
| `anon-w-seq-mt` | 4 | 515.692 | 497.505 | -3.5% | 22.288 | 11.936 | -46.4% |
| `anon-w-rand-mt` | 1 | 1674.691 | 1583.081 | -5.5% | 85.401 | 47.615 | -44.2% |
| `anon-w-rand-mt` | 2 | 949.483 | 902.523 | -4.9% | 46.295 | 24.200 | -47.7% |
| `anon-w-rand-mt` | 4 | 648.159 | 629.015 | -3.0% | 25.995 | 13.079 | -49.7% |

两用例均通过。该小规模 TCG 场景中，显式释放区间耗时中位数约减少 44%～50%，整体
耗时也下降，支持将 gather 作为解决当前批处理退化的候选。它不能证明所有 workload
更快：未测实机、更多 CPU、更大内存、稀疏映射、热 TLB 的额外重填，或 x86 实际
shootdown 延迟。不要把这些百分比作为通用性能承诺。

## 正确性与代价

独立内核回归覆盖受管理映射边界、mixed raw、失败回退和先失效后归还。原实现运行
新增稳定 API 回归在 32 页场景失败；gather 在 AArch64 158/158、x86_64 147/147 通过。
正常连续 1 GiB 的接口调用计数按当前策略为 1024 Full；这是状态机计数，和上面的
16 MiB workload 实测分开陈述。

额外代价是超出 16 个内联引用后尝试分配可复用 spill，HAL 容量请求 5760 字节
（不含 allocator rounding），总暂存上限 256 持有；失败时先处理当前批次继续推进。
范围过宽会更早选择 Full，可能增加其他热翻译重建；该权衡需要后续更广场景测量。

## 集成主线

性能测量后通过 merge 引入主线 `8316a7d2b8debae8c1b3d6cab59445f7fe53e1bb`，保留
#938 原提交历史，避免源码基线检查因主线分叉而提前停止。集成不会改变本报告中
测量的版本身份。集成后 AArch64 SMP=4 相关单测再次 158/158 通过，merge 提交的
格式、测试布局及普通 Clippy 检查通过；x86_64 147/147 是 gather 改动集成前的验证。
