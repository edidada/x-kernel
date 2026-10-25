# Completion 统一实现与验收

状态：Completion 已通过 PR !1128 合入 main `11fd96a60`；驱动分支已 rebase。
本文下方 736/13 项测试记录是拆出独立分支前的历史证据，不是合入版本的最终
验收计数。合入版原语的验证说明以 `sync/kcompletion/docs/validation.md` 为准；
驱动分支 rebase 后另有 208 项定向回归通过。后续计划见 [主线推进清单](next-steps.md)。
组合管理操作的剩余分配另列，不宣称整个内核等待栈无分配。

## 需求与边界

用户授权：参考 Linux 和已有内核机制，重做统一 Completion，迁移现有使用者，
最后按需求测试验收。对应 `ticket01-requirements.md` C01–C11。
此前驱动模型已确认的架构快照不因此覆盖修改。

同步等待不能依赖 poll 登记扩容、登记 ID、Arc<Waker> 或 block_on。初始化后的
等待/通知及等待节点退出应无堆分配和释放。任务/请求/业务快照的创建不等同于
等待机制；迁移不能声称所有调用方业务流程都无分配。

## 模块职责

- `sync/kcompletion`：完成令牌、永久完成、重置契约；共享的调用方栈节点队列。
  不依赖 ktask 或 kpoll，不提供业务回调和动态多源 poll 登记。
- `ktask::completion`：验证睡眠上下文，持有当前任务强引用，栈上唤醒锁存器与
  调度器阻塞握手。准备和等待不构造 Arc<Waker>。
- `kpoll`：继续负责动态多源 readiness；pidfd 按退出状态查询加 PollSet 通知。
  删除原 Completion 状态机，不保留另一个同名实现。
- 各子系统：仍拥有完成的业务含义、操作快照、资源存续和取消规则。
  IRQ 完成不等于设备 DMA 完成，RNG 通知不等于读成功。

## Linux 对照与同步证明

参考本地 `/root/linux-stable/kernel/sched/completion.c`、`swait.c` 和
`include/linux/swait.h`：计数与等待队列受同一 IRQ 安全锁保护，普通完成提供一个
令牌并唤醒一个等待者，complete_all 保持终态。等待条目位于等待调用的栈上。
Linux poll 可单独分配登记存储，不要求同步 Completion 反向依赖 poll。

节点先固定地址，再在锁内链接；通知摘链并完成受约束的调度唤醒后才释放锁。
等待退出也取得同一把锁，因此不能与通知并发销毁节点。私有登记守卫不向
调用方暴露，避免 mem::forget 绕过栈节点撤销。park 在队列锁外执行，唤醒锁存器
与调度器的 blocked 发布握手闭合 wake-before-park 窗口。

仅隐藏登记守卫还不足以覆盖不展开栈的任务退出。底层 `wait_with`/`wait_until`
是 unsafe host 入口：调用方必须保证回调不能绕过清理后释放已登记栈，或在
这种异常退出时继续保有栈/源/唤醒对象。`ktask::CompletionWait` 对外提供安全
接口，其强任务引用保留内核栈；异常退出可能泄漏该引用，但不会回收在链节点。
正常返回和 unwind 都同步摘链。这一限制经专门内存安全审查确认。

条件广播复用节点队列，但条件闭包在队列锁外、登记之后执行；条件可以提交
设备操作，因此之后不能有可失败的登记。IRQ 侧通知不能保留栈引用、递归进入
队列或申请内存。锁序和完整调度路径须审查，不能只凭链表无分配推断全链无分配。

IRQ 在外层锁下发布完成、锁外唤醒时，延迟唤醒令牌持有已有 Completion 的 Arc
引用而不分配。描述符移除不使唤醒源失效。业务层保留在途计数复查；reinit
不得被泛化为可以任意覆盖旧等待者结果。

### 调度 tracing 前置条件

源码核对发现 EEVDF/RT/deadline 就绪索引已经使用实体内嵌链接；旧 tracing
的逐事件 Vec 复制、任务名字 String 克隆及命令缓存增长仍会分配。作为本轮
明确的前置修复，采集改用初始化时预留的有界定长记录，名字复制到记录内，
文本和解析缓存放在读取阶段构造。保留事件、顺序和 snapshot ABI，不在同步
等待中跳过 tracing；溢出/过长记录有明确计数。需另跑启用事件的等待回归。

## 迁移与验收矩阵

| 需求 | 实现/迁移 | 必验 |
|---|---|---|
| C01/C03/C04 | 唯一完成状态；计数/永久态/重置 | 早完成、累积消费、多等待者、重置 |
| C02/C11 | 栈节点与栈 parker | 已就绪/阻塞/虚假唤醒/重复等待分配观测；调度路径审查 |
| C05/C06 | 锁内登记/摘链、私有撤销 | 完成在登记前后/park 前后、退出后迟到通知 |
| C07/C08 | 上下文验证、IRQ 通知、同步锁 | 原子上下文拒绝、IRQ 唤醒、跨任务结果可见性 |
| C09 | pidfd 回归 readiness，其余迁移 | kpoll、ktask、kirq、workqueue、RCU、块设备、RNG |
| C10 | host 层适配 | 可移植 driver contract 不新增内核依赖 |

Work/Queue 永久完成收据可以按顺序等待，不能丢失 cutoff、取消和清理完成语义。
RCU 保留回调独立所有权、安装哨兵和等待后的内存栅栏。块请求保留终态的永久性；
RNG 保留多等待者广播和虚假唤醒复查。相关旧故障注入测试按已经删除的登记失败
面调整，不能以删除测试替代生命周期回归。

## 实际验收记录（2026-10-23）

平台使用 `platforms/kplat-aarch64/qemu_defconfig`，执行 `make defconfig`；
AArch64 QEMU TCG、4 vCPU、本 worktree 独立 `disk.img`，关闭网络/vsock。
不是性能测量；未预约或占用性能独占窗口。

| 检查 | 结果 |
|---|---|
| `make build`（最终非 UT 构建） | 通过 |
| `make clippy` | 通过，包括依赖、版权头、测试布局及 lint 入口检查 |
| `make check_test_layout check_host_tests check_lints` | 通过 |
| 相关内核回归 | 736 passed，0 failed，0 ignored；覆盖率文件成功持久化并生成报告 |
| 生产原语宿主测试 | 13 passed；正向分配计数对照及目标区间分配/释放均为零 |
| 同一宿主测试 Miri | 13 passed；覆盖原始指针、跨线程、unwind 摘链 |
| 定向逻辑/资源复审 | 未发现尚未处理的具体问题；异常退出栈存续边界已补充 |

内核回归命令：

```sh
make unittest UNITTEST_CRATE=kcompletion,ktask,kpoll,kirq,kworkng,kworkqueue_ng,kworkerpool_ng,krcu,kdriver,kprocess,ktracing,subsys_hwrng,virtio,block,device_res_xkernel XKMAKE_ARGS='--no-net --no-vsock'
```

宿主探针从 `/tmp` 运行，避免继承内核生成的 Cargo target 配置：

```sh
cargo +1.95.0 test --manifest-path /root/codes/x-kernel-worktrees/driver-base-hwrng/sync/kcompletion/tests/host/Cargo.toml
cargo +nightly-2026-03-08 miri test --offline --manifest-path /root/codes/x-kernel-worktrees/driver-base-hwrng/sync/kcompletion/tests/host/Cargo.toml
```

736 项包含真实任务阻塞、IRQ 唤醒、多个等待者、早通知、虚假唤醒、非法上下文、
IRQ 同步释放、任务 join、Work/Queue 截止与清理、RCU、pidfd readiness、块请求、
RNG 多读者及启用 tracing 的 timer IRQ completion 唤醒。宿主计数不测量 ktask
调度器；调度器无分配结论来自实现审查与当前不安装过滤器的 tracing 路径，
不能扩展到任意外部 tracing callback/过滤器。没有做其他架构或性能验收。

本地日志：`/tmp/completion-build-final.log`、`/tmp/completion-clippy.log`、
`/tmp/completion-ut-final.log`、`/tmp/completion-host-tests.log`、
`/tmp/completion-miri.log`、`/tmp/completion-hygiene.log`。
构建保留上游库 future-incompatibility 提示及现有依赖审查警告，未修改无关代码。

## 明确的剩余分配边界

统一 Completion 不等于所有同步管理操作都无分配。kwork 的 Work/Queue 快照
仍创建独立完成观察收据和容器；这些确实是等待资源，不能改称业务 payload
来掩盖分配。普通 Work 每次观察 0/1/2 个状态分别可能准备 0/3/4 个堆对象；
最终所有者回收若仍需 drain 活跃状态，也可能进入该路径。详细计数见
`task/kworkng/crates/kworkqueue_ng/docs/design.md`。RCU 的独立回调标记仍需准备。

本轮验收区分“Completion 原语、任务 parker、登记/通知/退出无分配”与
“整个 Work/Queue drain、RCU synchronize 生命周期无分配”。后者尚未满足，
需要另行收敛完成观察快照的存储生命周期，不能据原语测试通过宣布已经解决。
