# driver-class — 设计文档

## 发布生命周期模型

| 类型 | 持有者 | 权限与释放 |
|---|---|---|
| `Registration<T>` | 驱动 | 不可 Clone；Drop 撤销注册，不等待在途操作 |
| `Publication<T>` | 注册表、消费者、订阅者 | 可 Clone；仅保活共享对象，Drop 不撤销 |
| `InUse<T>` | 已获准的操作 | 不可 Clone；保活 payload 并延迟 class teardown |

`Class::publish` 返回 Registration；其 publication() 返回消费者句柄，不转移注册权。
注册退出顺序为：移出注册表、关闭本项发布的 gate、通知移除、安排 finalizer。
已有许可仍能访问 payload，最后一个许可退出后执行 finalizer；新许可返回 BadState。
消费者引用仅保证对象内存存活，不延长发布有效期，也不阻止 finalizer。
Publication 不再公开无条件 with，裸 payload 仅在 crate 内投影 hook 或持有许可时访问。

PublicationSource 仅提供只读身份与在线查询。撤销一项注册不改变来源在线状态，
不影响同来源的其他注册。model 在设备移除时关闭来源，阻止其所有发布的新操作。

泛型层的受许可闭包仍不能阻止调用方克隆 T 内的 Arc；具体子系统必须封装泛型
Publication，向普通消费者仅暴露设备操作。hwrng 使用不暴露 payload 的消费者句柄。

在途许可只直接保护 class finalizer，不自动等待驱动 Data 的其他字段析构。
驱动必须让 payload 拥有操作需要的资源，或者在 remove 中停止并等待操作退出后
释放独立资源。Registration 放在 Data 第一字段不能替代这个协议；当前 model
不提供跨所有 publication 的自动 drain/join。此项限制不得描述成完整热拔插保证。

## 职责与模块

class 管能力发布关系，不管理设备/驱动绑定、总线、硬件操作或宿主资源实现。
依赖方向是 model → class；hwrng 封装 class，entropy 仅依赖 hwrng 消费接口。
同一来源必须复用同一个 Arc 身份，重复检测使用来源 allocation 与发布名称。

- src/lib.rs：模块声明与公开重导出。
- src/class.rs：Class、ClassHooks、订阅接口及私有 ClassRegistry。
- src/publication.rs：Registration、Publication、InUse、共享 Inner 和 finalizer。
- src/owner.rs：只读 PublicationSource 契约及遗留合成身份。
- ut-tests/class.rs：注册权、消费者引用、许可、独立撤销及失败回退测试。

按完整生命周期聚合模块，不按公开类型数量拆文件或 crate。

## 状态与算法

发布先验证来源已绑定且在线，分配私有序号，再运行投影 hook、插入注册表。
失败时撤销该次投影，不改变来源状态；重复身份返回 AlreadyExists。
成功后创建唯一 Registration，再通知订阅者。注册表和订阅者仅持有 Publication。

发布 gate 单向变化：开放 → 关闭。is_available 同时查询 gate 和来源在线状态，
但仅是快照；必须 acquire_use 成功才能访问 payload。来源下线拒绝该设备所有
发布的新使用，而单项 Registration Drop 只关闭本项发布。

## 并发与执行上下文

注册表与 finalizer 槽使用 SpinNoPreempt，仅在任务上下文使用。
注册、撤销、使用许可和最后一个许可的 Drop 均不能从硬中断调用。
hooks、订阅回调在注册表锁外执行；回调可重入枚举，不能递归撤销自身。
来源在线查询可能在注册表锁内执行，必须非阻塞且不得重入 class。
Registration Drop 不等待许可归零，但会同步运行移除回调，且可能立即执行 hook；
最后一个 InUse Drop 也可能执行 hook，调用者必须满足 hook 的上下文约束。

使用许可先增加 uses，再以 SeqCst 检查关闭状态；撤销以 SeqCst 关闭 gate。
失败获取回退计数，避免撤销与准入交错导致已获准操作被提前清理。
finalizer 安装与归零检查受同一槽位锁保护；最后一次释放在锁外执行取出的
FnOnce，因此许可在安装前或安装后释放都不会漏掉或重复执行 teardown。

## 发布身份的作用域

每个 Class 实例的私有注册表在自身锁内分配单调序号，用于撤销和通知前检查。
序号不复用，溢出返回 BadState；hook 失败消耗的序号无需回收。
不同 Class 可以具有相同序号，不提供公开 find(id)、PublicationId 或裸数字构造。
hook 直接借用 PublicationSource 查询来源信息，不创建元数据副本；其他投影层负责自己的身份管理。
消费者用 same_publication 比较同一次发布的 allocation，旧句柄与同名新发布不同。

## 资源释放与限制

Class 不直接管理硬件；finalizer 是受信任子系统提供的清理行为。
InUse 保护该清理行为，Publication 的 Arc 保护共享对象内存，两者不能互换。
现有 Vec 注册表的成员检查和移除仍为线性扫描，本轮不改变索引结构。
attach_legacy 仅为旧设备模型提供合成来源，具体驱动热拔插不在本轮实现。

## Hook 的来源信息

publish、失败回退和 unpublish 均接收 &dyn PublicationSource，查询同一来源对象。
不再构造驱动名、物理路径的 String 副本。延迟 finalizer 从仍存活的 Inner 借用
runtime 和来源，不捕获 Publication，避免形成自引用环。
来源信息是执行 hook 时的实时视图，不保证与发布时相同；投影如需固定清理键，
应在自己的状态中记录发布时结果，不能用撤销时重新查询的名称推导旧键。
