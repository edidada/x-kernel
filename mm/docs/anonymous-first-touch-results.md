# 私有匿名 first-touch 准备与提交分离：验证记录

日期：2026-10-22。本改动是 MM 并发系列的第一阶段：数据页分配与清零移出 MM mutex，
提交仍持锁重验并串行修改 PTE/对象。没有增加 RCU VMA 查找、无锁 PTE 或细粒度页表锁。

## 版本与正确性

- 基线：`8316a7d2b8debae8c1b3d6cab59445f7fe53e1bb`。
- 测量候选：`fce55693c`，前置纯重构为 `d56903f9e`。
- AArch64 4CPU：`memspace,anon,kuaccess,kprocess` 179/179 通过。
- x86_64 TCG 4CPU：相同选择范围 172/172 通过。
- AArch64 普通 `make clippy`、固定 nightly fmt 和测试布局检查通过。
- 新增 13 个内核测试，覆盖锁外准备、unmap/remap ABA、权限撤销、VMA split、
  重复 fault、分配失败、过期 OOM、discard、地址空间清空、fork、已有对象页及 COW。
- 可睡眠 kuaccess adapter 回归保持通过：backing kind 不能替代实际 runtime variant，
  自定义 adapter 继续走原 fault policy。异常中的用户原子访问保持原锁内入口。
- 两次独立只读审查及 fallback 修复的增量审查完成；不能替代运行测试。

内核日志本地保存于 `/tmp/mm-fault-unittest-aarch64-slim.log`、
`/tmp/mm-fault-unittest-x86-slim.log`，Clippy 为 `/tmp/mm-fault-clippy-slim.log`。
产物与覆盖率留在各隔离工作树的 `target/`，不提交大体积生成文件。

外部 `mmap-semantics` 回归通过，日志为 harness 的
`logs/ci-test-iter/20261022-114601/`，覆盖部分解除、权限变更、私有文件写及重叠拒绝。
外部 `mmap-tlb-flush-semantics` 也通过，日志为 `logs/ci/20261022-114739/`，覆盖
跨进程 truncate 的 SIGBUS 和同 mm 跨 CPU 的 PROT_NONE、只读、munmap 可见性。

发布前将分支变基到 `97bac30f6`（相对测量基线仅一处 TEE 参数修复），MM 改动内容
未变；性能表仍严格对应下面列出的测量快照。

## vm-scalability 方法

- 外部 harness：`c31d42618376c983e374069f55449178dfb1d48e`。
- AArch64 QEMU TCG，4 CPU，1 GiB RAM，`xkernel-glibc`。
- rootfs SHA256：`f91e25157dfc8b7e05f71d6fb2a27c31ec8fbd7db91d79dde41ec4f0341476fd`。
- 相同 16 MiB 总内存预算、4 KiB 映射、8-byte touch stride；1/2/4 worker，
  每组 3 样本。COW 用例按 harness 定义的 work_units 计算，不能跨用例直接比吞吐。
- 基线、候选顺序运行；测量期间没有并行的本任务构建或其他 QEMU。
- 最终基线日志：`logs/daily/20261022-105146/`。
- 最终候选日志：`logs/daily/20261022-114023/`。
  路径相对外部 harness；JSON 和 case log 副本在 `target/mm-fault-validation/vm-scalability/`。

分别设置 `XKERNEL_REMOTE` 为基线和候选隔离工作树，运行既有用例：

```bash
CASES=vm-scalability-anon-w-seq-mt,vm-scalability-anon-w-rand-mt,vm-scalability-anon-cow-seq-mt \
ARCH=aarch64 JOBS=1 make daily-test run
```

各组耗时取 `USEMEM_SUMMARY` 的中位数，覆盖 setup、执行及 wait/reap；summary 之后
进程最终退出的剩余销毁不包含在内。下表吞吐变化为相同用例和 worker 数的候选/基线，
正数表示吞吐提高。所有用例均通过。

| 用例 | worker | 基线 ms | 候选 ms | 吞吐变化 |
| --- | --- | --- | --- | --- |
| `anon-w-seq-mt` | 1 | 410.070 | 428.224 | -4.2% |
| `anon-w-seq-mt` | 2 | 351.225 | 334.506 | +5.0% |
| `anon-w-seq-mt` | 4 | 419.267 | 375.646 | +11.6% |
| `anon-w-rand-mt` | 1 | 1576.536 | 1591.254 | -0.9% |
| `anon-w-rand-mt` | 2 | 778.500 | 930.456 | -16.3% |
| `anon-w-rand-mt` | 4 | 606.261 | 538.559 | +12.6% |
| `anon-cow-seq-mt` | 1 | 396.455 | 393.383 | +0.8% |
| `anon-cow-seq-mt` | 2 | 422.606 | 400.614 | +5.5% |
| `anon-cow-seq-mt` | 4 | 549.499 | 516.463 | +6.4% |

## 结论与限制

- 4 worker 的顺序/随机写吞吐约提高 12%；COW 对照没有观察到回退。
- 单 worker 顺序写约慢 4%，2 worker 随机写约慢 16%。不能据此宣称通用性能提升，
  也不能只选择 4 worker 的结果作为验收结论。
- 2 worker 随机写基线三个样本为 902.500/778.500/648.791 ms，候选为
  930.456/931.778/919.336 ms；存在明显差异，仍需更多轮次和真实硬件定位。
- 新路径增加一次 mutex 重入和重验，用分配/清零并行换取这一成本；PTE/对象提交及
  中间表分配仍串行，分配器也有独立同步。没有 lock hold-time 测量，不能把全部差异
  直接归因于某一个锁或把 acquisition 次数当 contention。
- 早期版本的 COW fallback 重复 VMA/PTE 查询已移除；最终候选复用同一锁内观测，
  并精简为必要的 VMA 标量字段和一份 backend owner。这一调整没有放宽提交重验。
- 本阶段适合作为准备/提交 API 与生命周期边界的基础供审查；若性能门槛要求所有
  线程数均不回退，应继续定位并重测后再合并。后续细粒度提交和 RCU 查找需要独立设计。
- 未测实机、8CPU、多 VMA 布局、冷文件 fault 或 pin/迁移；不承诺这些路径的收益。

## 分支与后续

PR #938 的 TLB gather 改进独立实现和发布，没有并入本分支或这组测量。
新 MM 工作应在该准备/提交协议上继续拆分 VMA 结构写侧、VMA 稳定性和 PTE 提交同步。
RCU 只保护存活；不能替代映射权限重验、写侧串行和硬件退休。
