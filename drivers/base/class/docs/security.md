# driver-class — 安全分析

## 范围与信任边界

分析 `src/lib.rs`、`src/publication.rs`、`src/class_api.rs` 及 `src/lib.rs` 内的私有 `owner` 模块。本 crate 不包含 unsafe。
模型提供受信任的 `PublicationOwner`，具体子系统提供 hooks 和 runtime payload。
不直接解析固件或用户输入，也不实现 MMIO、DMA 或 IRQ 操作。

模型必须为同一设备复用相同的 `Arc` 身份 allocation，返回稳定路径、实时在线状态，
并实现幂等且不可逆的 `mark_offline`。trait 不允许 class 修改驱动名或拓扑；私有
`DeviceCore` 仍由模型拥有。遗留合成身份是独立旧设备的状态，不是模型身份的副本。

## 内存与并发安全

`Arc` 保证身份和 payload 的存活；使用计数保证在途操作结束前不执行 class teardown。
`Send + Sync` 约束跨执行上下文共享的对象。注册表锁不跨越外部回调；任务上下文
限制避免硬中断重入不关中断的 `SpinNoPreempt` 锁。

## 失败分析与限制

| 风险 | 处理与限制 |
|---|---|
| 未绑定设备发布 | claiming driver 为 `None` 时返回 `BadState` |
| 同身份同名称重复发布 | 返回 `AlreadyExists`，回退新投影，保留旧发布 |
| 撤销与获取许可并发 | 增加计数后重新检查 gate，失败回退；最后一个使用者执行 finalizer |
| 使用者尚未退出 | teardown 延迟至最后一个 `InUse` Drop |
| 外部身份实现不遵守契约 | 无法通过类型系统验证，属于受信任模型集成的审计责任 |
| 一项发布撤销影响同设备其他发布 | 保留原有共享 offline 协议；改变此行为应另立行为提交 |
| `with` 绕过可用性检查 | 仅用于 teardown 或明确允许离线读取的操作，调用者负责语义 |
| 遗留发布者无模型域 | `attach_legacy` 仅作迁移适配，不用于新设备模型路径 |

路径和驱动名用于设备投影及诊断，可能暴露硬件拓扑；用户可见投影的权限控制由
对应子系统和 host integration 负责。此处不增加用户态访问入口。

## 审计与验证

class UT 覆盖 owning/borrow handle、重复发布及 drain；model 的 domain UT 验证
跨 crate 身份实时可见、重复身份保持、共享退休及在途 payload 保活。
修改接口时检查锁外回调、单一状态归属、错误回退和最后使用者 teardown 协议。
