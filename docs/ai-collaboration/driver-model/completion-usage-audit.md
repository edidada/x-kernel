# Completion 使用者的分配失败审查

状态：当前 worktree 的源码静态审查，未做分配失败注入或运行验证，未修改实现。
范围为检索到的 kpoll::Completion 直接生产使用链，并对同名 RNG 条件等待作区分。
不是整个内核分配器/所有 poll 用户的全面正确性证明。

## 判断标准

区分两类失败：PollSet/PollRegistrations 的 try_reserve 可返回错误；Arc::new、
Box::new、Vec::with_capacity 等仍遵循当前全局致命 OOM 策略，不能假定它们
将自动变成 Result。登记表有内嵌容量，所以不应声称每次 register 都分配。

检查的重点是失败时是否已经启动不能直接释放的活动，以及谁仍持有资源。
有分配并不自动构成错误；可安全失败的创建/准备与必须完成的排空不是同一场景。

## 1. kirq：已确认不可逆移除后仍可能等待失败

- `arch/kirq/src/runtime/manager.rs:600` 的 free action 路径先移除 action、开始
  teardown，再等待在途 dispatch；`:662` 对等待错误直接 panic，源码有对应 TODO。
- `task/ktask/src/irq_wait.rs:20` 每次 poll 通过 PollRegistrations 登记；可失败
  登记没有被放到移除前。多个等待者超过内嵌容量时可需要扩容。
- `try_synchronize_irq` 则可直接返回等待错误；调用者不得把 Err 当同步成功。

结论：不满足低内存下可靠完成 IRQ 释放。当前是 panic，未据此发现提前释放
回调资源的 UAF；二者不可混称。是 Ticket 02 接入新同步能力的明确动机。

## 2. ktask join：失败时 yield 重试，而非可靠睡眠

`task/ktask/src/task.rs:481` 的 join 在登记失败时调用 yield_now 并重试。只要
目标仍未退出且登记持续失败，就会反复调度/重试；目标退出后状态检查仍可返回。
这不是紧循环自旋，也不能断言必然死锁，但低内存下不能保持正常阻塞行为。
进入 block_on 时仍有 KWaker 的前置分配，致命 OOM 不经过这个重试分支。

结论：逻辑上不提前报告任务退出，但不满足新的无分配同步等待目标。应列为
completion 同步能力的实际迁移/回归候选，而不只为驱动提供另一套接口。

## 3. kwork：提交无逐次分配，不代表同步管理和最终回收无分配

- `task/kworkng/crates/kworkqueue_ng/src/work/wait.rs:34` 的 flush 准备 task
  waker、Vec 和 CompletionObserver，再 capture；登记错误在 capture 前返回。
- 同模块的 disable_sync/cancel_sync 在准备完成后才 disable/capture；队列
  close_and_drain 先登记所有 receipts，再 close_admission；队列 flush 同样
  在发布本次 cutoff 前登记。成功进入等待后使用持久终态，不再登记。
- `task/kworkng/crates/kworkqueue_ng/src/work/observers.rs:32` 每次准备新 Arc
  receipt，内部 Completion 自身还有 PollSet 初始化，故仍存在真实分配。
- `task/kworkng/src/owned.rs:111` 的拥有型 shutdown 先关闭自身提交入口再 drain；
  若 drain 返回错误，owner 仍保留存储，不能认为错误返回代表已排空或恢复开放。
- 同文件 `:122` 的最终回收也调用 drain。新私有 receipt 的单个登记有内嵌容量，
  不能简单说此处必然可能因登记表扩容失败；但 waker/receipt/容器创建仍可能
  走致命 OOM，不能把最终回收描述成无分配。
- workerpool 的 SourceDetachment 自身负责完成状态；队列关闭路径在发起 detach
  前登记其 completion。不能把“发布完成的对象”与“执行等待的调用方”重复算成缺陷。

结论：现有预准备保护了大部分同步管理路径，不是与 free_irq 相同的事后登记
错误。低内存下可能无法开始同步管理，最终回收仍有分配依赖；若要求这些阶段
也可靠无分配，需要独立核对 observer 存储，不会仅替换 Completion 就全部解决。

## 4. 块设备请求：提交后的完成等待已有保护

`drivers/integration/kdriver/src/block_completion.rs:42` 在准备阶段建立 task
waker 和 admission/terminal completion，并预登记 terminal。`:84` 的
wait_completion 只检查永久终态与 park，不再登记。admission 的可失败等待
位于提交前，`drivers/devices/virtio/src/blk/requests.rs` 失败时撤销 pending
请求并交接队首；提交成功后才进入不能失败的完成等待。

结论：未发现与 free_irq 相同的提交后登记失败缺口。准备仍可能 OOM，但这是
启动请求之前的资源准备，不应为证明重构必要性把正确的前置处理判成错误。
新机制可以减少准备成本，必须保留提交后 buffer 生命周期和永久终态保证。

## 5. hwrng：相邻的条件等待风险，不是直接使用 kpoll::Completion

`drivers/integration/kdriver/src/rng_completion.rs:29` 在第一次 ready 检查后
才登记；`drivers/devices/virtio/src/rng.rs:264` 的 ready 闭包可能启动 DMA。
因此等待表扩容失败可能发生在请求启动之后，read 返回错误但不取消 DMA。
请求 buffer 由设备内部对象持有而非调用者的 read 切片；Pending 析构还使用
显式保留资源策略，不能据此声称等待失败就产生了 DMA UAF。

结论：等待错误不代表请求已结束；不满足后续新模型的可靠退出目标，但当前
接口已说明该限制。它需要条件等待/停机协议的适配，不应直接换成消费完成令牌。

## 对 Ticket 01 的影响

1. 机制缺口不只影响新驱动，至少 IRQ、任务 join、work 管理都是真实使用者。
2. 区分“消除登记失败”“减少等待辅助分配”“消除调用者 observer/request 分配”，
   三者不是同一修改；不能承诺只重构 Completion 就使所有系统无分配。
3. 成本收益比较必须纳入现有调用者的简化收益及兼容成本，不能默认另加驱动专用版。
4. 后续设计给出逐类迁移矩阵；具体修复归属和票依赖需单独调整，本次调查不自动
   授权重写 kwork/块设备/RNG 或扩大 Ticket 01 的实现范围。
