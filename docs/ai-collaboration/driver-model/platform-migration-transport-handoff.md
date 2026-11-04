# 平台迁移设计（二）：transport 能力交接

状态：接口方向评审稿，未批准开发；本次只修改文档。
源码基线：`a4ba1a2bd`。承接 [对象与生命周期](platform-migration-object-design.md)。

## 1. 结论先行

建议把 **总线提供给一个设备的运行能力** 与设备描述、驱动绑定 Data 分开。
这不是新的设备注册表，也不是另一个 host services bundle。

- 描述回答“这是什么设备、资源约束是什么”，可由旧 DeviceRef 长期保留。
- 总线运行能力回答“本设备的功能驱动怎样操作上游 transport”。
- 功能绑定 Data 保存队列、请求和子系统发布，并负责本次功能退出。
- 父 transport 绑定负责创建/注销 child，最后回收物理映射。

建议 model 仅增加注册前安装、回调内借用、注销时释放的通用承载点；
具体能力类型与操作属于 VirtIO bus，不把 VirtIO 方法写进 model。
这是一项明确的前置机制，当前 API 尚不支持，不应在接入驱动里绕过去。

## 2. 为什么不直接复用现有接口

`domain/registry.rs` 的 `add_device` 在返回前调用 `probe_pending`，因此能力必须
与 child 注册一起安装，不能先拿到返回的 DeviceRef 再补关联。
父 probe 此时尚未返回 Data；通过 `with_data` 查父对象既不满足 Active 条件，
又可能反向获取父管理锁。child 回调不得通过这条路径操作 transport。

`DeviceServices` 是宿主 MMIO/IRQ/DMA 等能力合同；`ExtensionRegistry` 存储
boot-static 引用。直接存设备运行能力需修改其寿命合同；静态服务也可以另外管理
动态对象，但又要设计关联和撤销机制，并混淆“宿主提供能力”和“上游总线提供
设备操作”。不是技术上不能实现，而是不建议本次这样扩展。

## 3. 两个接入方案

| 方案 | 同步 probe 前关联 | 生命周期与代价 | 判断 |
|---|---|---|---|
| 每设备 services 注入，并扩展动态 extension | 注册参数带入 | 要修改静态 extension 合同，功能驱动经宿主服务寻找上游设备能力 | 不选，职责不清楚 |
| 总线定义运行能力，model 的设备条目持有它 | 与描述在同一次注册中安装 | 扩展注册/回调 seam，但不另设表；绑定和注销仍由原 domain 编排 | 推荐方向 |

第二项不是把裸 `Any` 查询表开放给驱动。候选形式是由 `Bus` 关联类型限定
运行能力类型，`DeviceAccess<B>` 只借用该类型；无需运行能力的总线使用空类型。
具体关联类型名、注册参数和类型擦除实现仍待接口评审，不在此伪装成已有 API。
通用承载点不负责 reset、租借计数或硬件故障恢复。

## 4. 谁持有什么

| 所有者 | 持有内容 | 退出责任 |
|---|---|---|
| parent transport 的 Data | 下游注册关系、transport 运行对象的所有权 | 禁止新功能会话，注销 child，确认功能退出后释放映射 |
| child 的 domain 条目 | 同一个 transport 运行对象的受控能力引用 | 在 probe/remove 回调提供访问；注销结束后释放引用 |
| child 的 DriverOps::Data | 独占的 transport 会话、队列及功能资源 | 停止准入、reset 确认、IRQ drain、回收队列、结束会话 |
| child DeviceRef / descriptor | 设备身份、parent 和匹配事实 | 不持有运行对象，不延长 MMIO/队列寿命 |

表中的受控引用不是另建一份 raw transport。也不意味着仅靠 Arc 引用计数
决定设备何时关闭。物理映射的实际存储位置须保证会话存续期间不可释放；
父退出必须在锁外完成硬件等待，不能拿着能力对象的短锁等待 child。

## 5. 对接按值持有 Transport 的 vendor

vendor 的 RNG/block 对象按值持有 `T: Transport`，不能假装借用父 Data 就足够。
候选 adapter 是 **不可 Clone 的独占会话**，由功能对象按值持有；运行对象在
同一时刻最多发出一个会话。会话实现 vendor Transport，而不是创建第二个 MMIO
映射或可任意复制的 raw transport。

生命周期合同必须满足：

1. parent 在注册 child 前准备运行对象，child 的立即 probe 可以取得会话。
2. child 仅解绑时，完整功能关闭后才允许下一个绑定取得会话；parent 映射保留。
3. parent 退出先永久关闭该运行对象的会话入口，再注销 child。父重绑创建新的
   运行对象和 child；旧能力不能恢复到可取得会话的状态。
4. 会话 Drop **不能证明**设备已 reset、IRQ 已 drain 或 DMA 已停止；不能仅因
   Drop 把运行对象标成可再次绑定。显式功能关闭和构造失败回滚必须分别证明。
5. 不在每个 transport 方法上重新获取 parent 管理锁；不增加通用操作计数框架。

这仍是候选，不是已完成的安全实现。vendor `Transport::notify`、`queue_set`
等操作没有错误返回；不能靠“关闭后所有方法返回 Err”实现任意句柄撤销。
因此必须维持独占会话和功能层停止准入的合同，而不是公开可复制操作句柄。

**尚待闭合的关键点**：会话如何交还 raw transport；构造函数消费会话后失败时，
是否已经暴露队列/DMA、如何保留并安全回滚；关闭成功如何与会话释放汇合。
须逐一核对 RNG/block 的实际构造和析构路径，再定 adapter 接口。
如果只能依靠 vendor 改动解决，应单独提出前置需求及授权，不能悄悄修改 vendor。
在这些点关闭前，不能声称功能 rebind 已设计完成。

## 6. 与 Linux 对齐的部分

本地 Linux `drivers/virtio/virtio_mmio.c` 的 `virtio_mmio_probe` 在调用
`register_virtio_device` 前设置 parent、config ops 和映射；remove 调用
`unregister_virtio_device`。这支持“先准备总线运行能力，再注册功能设备”的顺序。

Linux 通过内嵌 virtio_device 和 config ops 关联 transport，而不是查询父驱动的
私有 Data。这里借鉴同一职责划分；Rust 的元数据引用和按值 Transport 所有权
不同，所以不能声称独占会话是 Linux 的原样实现。

## 7. 前置机制验收与下一步

通用承载点将来需验证：同步 probe 可见、类型不匹配注册被拒绝、注册失败不遗留
能力、无匹配驱动时保留能力、功能解绑保留能力、设备注销释放能力、旧 DeviceRef
不保活资源。model 不承担 VirtIO 硬件停机测试。

VirtIO adapter 将来需验证：双重获取被拒绝、父关闭后无法重新获取、子解绑后
安全重绑、父重绑隔离旧引用、构造每个失败阶段回滚、完整 IRQ 尾部之后释放队列。
这些是待实现验收项，不是本次已执行测试。

下一步先闭合第 5 节的 vendor 所有权与失败路径，再定通用承载接口；随后继续
平台人口、串口/block 接入与 PR 切片。设计验收前不生成开发 tickets。
