# driver-class — 设计文档

## 定位与背景

通用 class 发布机制，独立于设备绑定模型、具体设备家族和 host 实现。
hwrng 定义具体设备能力，entropy 消费 `Publication`；二者无需依赖 probe 引擎。
设备模型通过 `PublicationOwner` 接入，不向 class 暴露内部 `DeviceCore`。

## 职责与拆分边界

class 管能力提供者与消费者之间的发布关系，不管理设备与驱动的绑定关系。
它拥有注册表、发布所有权、使用许可、在途计数和 finalizer；驱动匹配、probe
调度及设备拓扑由 model 管理，hwrng 等具体能力接口由对应子系统定义。

一个 class 可以接收不同设备的能力，同一设备也可以产生多个 publication。
因此发布状态与设备绑定状态分别建模。class 不复制设备在线状态，自己的
drain gate 只表示一项发布是否继续接受操作。

本轮保持注册表、使用计数与 finalizer 为同一 crate 的内部实现，不继续拆 crate。
当前共享 offline 协议不保证多项能力能独立撤销；是否改变该协议是后续 review
的行为议题。整体停止点见 [base README](../../README.md#当前停止点与后续工作)。

## 范围与架构

- `src/lib.rs`：模块组织、公开类型重导出和私有注册表。
- `src/publication.rs`：`PublicationId`、`Publication`、`InUse` 及私有共享状态，负责发布所有权、使用许可、在途计数和延迟 teardown。
- `src/class_api.rs`：`Class`、投影 hooks 与发布/撤销编排；实现模块私有，类型在根导出。
- `src/lib.rs` 内的私有 `owner` 模块：活身份契约与遗留发布者的私有合成身份。
- `ut-tests/class.rs`：发布、重复身份、撤销和 drain 测试及 detached 测试辅助实现。

依赖方向为 `driver-model → driver-class`。class 不依赖模型、总线或具体设备家族。
`Class::publish` 是模型集成入口；普通驱动仍经 `Device::publish` 获取拥有发布权的句柄。
同一设备必须传入同一个 `Arc` 身份 allocation，重复检测使用其指针身份与发布名称。
`attach_legacy` 暂为旧模型提供合成身份，随旧发布路径迁移而移除。

## 调用约束与并发模型

注册表使用 `SpinNoPreempt`，调用者处于任务上下文，不得从硬中断进入。
hooks、订阅回调和身份回调在注册表锁外调用，身份回调不得重入撤销流程。
模型持有共享设备身份的唯一可变状态；class 不缓存在线状态。
每个 publication 另外拥有自己的 drain gate 和在途计数，这不是第二份设备在线状态。

## 状态与算法流程

发布先验证 claiming driver，再运行投影 hook、插入注册表和通知订阅者。
失败路径撤销投影；重复身份返回 `AlreadyExists`，不替换原发布。

只有 `publish` 返回的 owning handle 在 Drop 时撤销。克隆、注册表和订阅者持有
borrow handle，不能通过 Drop 撤销发布。撤销从注册表移除对象，关闭使用 gate，
运行 removal 回调，再请求 owner `mark_offline`，最后安排投影 teardown。
该顺序保留拆分前共享设备退休的行为：同一设备的其他发布也会观察到 offline。

使用许可先增加计数，再重新检查 drain gate 和在线状态；失败时回退计数并在
必要时执行 finalizer。finalizer 安装与零计数检查受同一槽位锁保护，避免最后一个
使用者退出与安装 finalizer 交错而丢失 teardown。

## 设计决策与资源释放

使用四方法的 `PublicationOwner` 契约替代对模型私有结构的依赖，不复制设备状态，
也不拆出兜底 types crate。身份的实际修改仍由模型实现；class 仅持有受限的退休回调。
最后一个 `InUse` 释放后执行 deferred teardown，payload 由剩余 `Arc` 保活。
`with` 不检查可用性，仅供 teardown 和明确允许离线读取的操作；数据面使用
`with_available` 或持有 `InUse`。本次拆分不改变这些既有语义。
