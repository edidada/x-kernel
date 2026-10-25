# Completion 合入后的主线推进清单

状态：2026-10-23 更新；用户确认独立推进 Ticket 02 验收，01A 完成不等于原 01 整票完成。
代码基线：`xiaohui/driver-base-hwrng` 的 `269fe04a2`，已 rebase 到包含
Completion PR !1128 的 main `11fd96a60`。需求仍以 requirements-v0.2 为准；
不因前置原语完成而宣称新驱动框架已经实现。

## 1. 已完成与尚未完成

| 项目 | 当前结论 | 后续动作 |
|---|---|---|
| Ticket 01A 内核同步等待（完成） | kcompletion、ktask adapter 及原有调用方迁移已合入 | 不重做原语、不为 IRQ 循环复用另造等待句柄 |
| Ticket 01B 可移植接入（未完成） | os-api 尚无 class 所需的等待契约及测试 adapter | 与 Ticket 03 一起做局部设计，先验收接入再验收 class；不能直接依赖 ktask/kirq |
| Ticket 02 IRQ 同步释放（验收补强完成，待 review） | 独立 worktree 已补实际阻塞释放与共享 action 集成证据，247 项通过，负向控制生效 | review 后独立提交；未改生产 IRQ 语义，不阻塞于 01B |
| Ticket 03 class 同步撤销 | 仍是旧的延迟 finalizer 实现 | 下一张主体开发票，先出局部设计再实现 |
| Ticket 04 及之后 | 未按目标模型实现 | 本轮不展开设备绑定、DMA、VirtIO 及真实 hwrng 迁移 |

Rebase 后 `make clippy` 通过；208 项定向 UT 覆盖 driver_model、driver_class、
driver_os_api、subsys_hwrng、kdriver、kirq、kcompletion。它们证明现有实现回归，
不代表 Ticket 03 的目标语义已验收。旧实验代码保存在 stash `0cf0f231c`，
不得重新覆盖 main 已合入的 Completion；设计文档恢复为本地未提交资料。

## 2. 紧接着做：Ticket 02 验收收口

独立分支 `xiaohui/irq-sync-acceptance`，worktree
`/root/codes/x-kernel-worktrees/irq-sync-acceptance`，从最新 main 建立；
不携带驱动模型分支及本目录的未提交设计资料。只补 IRQ 释放集成证据及必要修复。

本轮结果：上述独立 worktree 新增普通/共享 IRQ 两条 SMP 集成场景，观察实际
Blocked 状态、资源析构顺序、重复释放和共享 action 保留。`make clippy`、fmt
及 247 项定向 UT 通过；临时跳过释放等待时恰好两条新测试失败，其他 134 项
ktask 测试通过。恢复生产代码后 247 项再次通过。没有生产逻辑修复，尚未提交。
完整证据位于该 worktree 的 `arch/kirq/docs/irq-sync-acceptance.md`。

已有证据位于 `arch/kirq/ut-tests/runtime_manager.rs`：

- `test_free_irq_waits_for_in_flight_dispatch` 检查移除/等待路径。
- `test_free_irq_rejects_masked_irqs_before_action_removal` 检查非法上下文不摘除。
- `test_shared_irq_free_one_action_keeps_line_active` 检查其他共享 action 保持可用。
- `test_in_flight_completion_old_wake_does_not_complete_next_dispatch` 检查迟到唤醒。
- ktask 的 `test_kirq_sync_wait_provider_blocks_until_completion` 单独覆盖等待桥。

证据限制：上述 manager 在途释放测试使用测试 hook，在进入 provider 之前结束
模拟 dispatch；它与等待桥测试是分开的，不能当成已验证“真实 handler 在飞时，
释放任务确实阻塞，handler 退出后才返回”的单条集成证据。

本轮已按以下步骤补齐受控并发集成测试：

1. 通过实际 dispatch 路径进入 handler，持有测试资源并通知协调方。
2. 另一任务执行同步释放，验证它尚未返回、测试资源没有提前析构。
3. 允许 handler 返回，验证释放完成且之后不会再次调用被移除的 action。
4. 加入重复释放结果检查，并明确共享 line 使用现有整线 in-flight 语义。

handler 不得睡眠；测试握手须符合 IRQ 上下文和 SMP 条件，不能以普通 sleep
或单次 yield 冒充确定性阻塞证据。具体 fixture 先审查，不在本整理中规定算法。
验收通过后记录 Ticket 02 完成，不把 IRQ 等待性能优化作为本票新增阻塞项。

## 3. 下一张主体票：Ticket 03 class 单项同步撤销

已开始 [01B/03 局部设计](class-withdrawal-design.md)。已确认显式退出与最终析构分开，
未退出就 Drop 属于 bug；不增加后台退出兜底、通用 payload 容器或解绑重试事务。

### 当前差距

- `Registration` 只有非等待 Drop，没有显式完成撤销的公共入口。
- `Class::withdraw` 关闭后安装 `finish_or_defer`；在途操作存在时可以先返回。
- 最后一个 `InUse::drop`，甚至 acquire 失败的计数回退路径，会内联 finalizer。
  清理上下文因此由消费者决定，而非显式撤销方决定。
- `Inner<T>` 直接保有 runtime；旧消费者句柄存活会保留 payload，不能据此保证
  活动资源已与旧句柄脱离。
- 准入同时依赖 publication gate 和 `PublicationSource::is_online`，来源查询
  仍参与运行权限控制，与已确认的只读来源职责不符。

### 需求与交付边界

1. 唯一注册权属于发布方；消费者句柄与操作许可不获得撤销权。
2. 关闭与 acquire 有明确线性化顺序；关闭后新操作失败，已准入操作仍受保护。
3. 显式同步撤销完成时：在途许可归零、class hook 完成且只执行一次、本次发布
   的退出责任已履行；长期保留旧句柄不能阻止这一过程。
4. 许可释放只更新状态及必要通知，不内联可能睡眠的清理。
5. class 表锁不覆盖等待、外部回调或 hook；退出不临时分配等待登记资源。
6. 子系统先使阻塞操作能够退出，再等待排空。class 不负责猜测 DMA 停止条件，
   也不自动取消设备请求；hwrng 不可依赖未来熵 IRQ 才能完成撤销。
7. PublicationSource 只提供只读来源；单项发布管理自己的准入与撤销。
8. 独立 class 发布方可用；旧 model 仅作保持构建与行为安全的最小接线。

### 局部设计必须先回答

- 关闭、停止业务、等待和 hook 的调用次序；组合与分步入口是否确有必要。
- 保留已停止 runtime 的旧句柄如何与新绑定隔离；具体驱动显式解除资源的责任。
- 未显式撤销就 Drop 的 bug 诊断方式，以及合法 probe 失败回滚的调用迁移。
- os-api 最小等待契约、宿主 adapter 和可控测试 adapter 的职责、上下文及
  失败策略；对象创建失败与关闭之后的不可失败等待应分开说明。
- 移除 source 在线门禁后，旧 model 如何保持安全；绑定权限全面改造留在 04。

本地 Linux 参考：`drivers/char/hw_random/core.c` 的 `cleanup_rng` 将工作交给
workqueue，`cleanup_rng_work` 在 cleanup 完成后通知，`hwrng_unregister` 在
释放 rng_mutex 后等待 cleanup_done；`virtio-rng.c::remove_common` 先唤醒读者
再注销。它们支持“清理完成可等待、不能依赖任意消费者上下文”的要求，但
不等于通用 class 必须照搬 hwrng 的工作队列或引用计数实现。

### 验收场景

无在途立即撤销；关闭/acquire 竞争；多个在途许可；最后许可在非睡眠上下文
归还；hook 可睡眠且仅一次；旧句柄保留已停止对象但不阻碍显式资源解除；新发布不复活
旧句柄；重复关闭/收尾；非法等待上下文；发布失败回滚；独立来源发布；阻塞
消费者先被唤醒退出再完成排空。同步使用受控握手，不靠时间延迟猜测先后关系。

## 4. 执行顺序与停止条件

1. Ticket 01A 已完成；在独立 main worktree 收口 Ticket 02 的实现—测试映射和缺失集成证据。
2. 回驱动分支，提交 Ticket 01B 与 03 的最小局部设计，附所有权图、退出次序、Drop 规则及 Linux
   对照；先评审未决点，不直接按历史准备关联类型/整树方案编码。
3. 设计确认后先验收 01B 等待接入，再验收 03 单项撤销；机械拆分与行为修改分开提交。
4. 完成 class 验收后再开始 Ticket 04 的设备注册/解绑/重绑。

若再次发现宿主机制缺口迫使 class 过度设计，先报告缺口与独立前置方案，
不自行增加事务框架、通用执行器或组级准入层。本清单不授权提交或推送。
