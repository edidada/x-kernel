# 设备注册与驱动绑定生命周期分离：需求与设计草案

状态：待评审，尚未实现。基于提交 `491cb6e8d`；本文不是当前实现说明。
本草案暂不推进实施。整体需求重新整理见
[协作进度](../../../../docs/ai-collaboration/driver-model/README.md)，
后续整体设计须从获批需求重新推导，不能默认采纳本文全部机制与限制。
术语见 [模型术语](../CONTEXT.md)。下文接口和内部类型名称均为提案。

## 1. 问题与依据

当前 `DeviceState` 混合 Pending/Probing/Active/Deferred/Unclaimed 与
Removing/Removed。`remove_device` 同时关闭设备来源、调用驱动 remove、
销毁 Data、解除拓扑关联；probe 跳过 Removed。没有在保留设备注册身份的
前提下结束绑定、再显式发起新一轮匹配的操作。

本地 Linux 对照：`drivers/base/dd.c::device_unbind_cleanup` 释放 devres，
清除 driver/drvdata；`drivers/base/bus.c::{unbind_store,bind_store}` 对同一
已注册设备解绑和绑定。`drivers/base/core.c::device_unregister` 则执行
device_del 与 put_device。这里借鉴职责分离，不要求复制 Linux 全部接口。

## 2. 需求

| 编号 | 必须满足的要求 |
|---|---|
| R1 | 解绑不注销当前设备：保持对象身份、描述符、所属总线实例及该设备的父关联 |
| R2 | 解绑关闭本轮绑定的发布来源，调用驱动 remove，销毁本轮 Data；元数据引用不得阻止解绑 |
| R3 | 解绑完成后保持未绑定，不因新驱动注册或依赖 wake 自动重新绑定 |
| R4 | 显式重新探测仍使用同一 DeviceRef，重新选择候选驱动；不要求指定某个驱动 |
| R5 | 每次 probe 尝试具有独立且只关闭、不复活的发布生命周期，包括失败和 Deferred 路径 |
| R6 | 新绑定不能恢复旧 publication 的可用性，也不能改写旧 publication 的驱动来源 |
| R7 | 同一设备的 probe、解绑、注销互斥；回调、资源析构及 class hook 不在域锁内执行 |
| R8 | 注销仍允许结束绑定，但不得把设备恢复为可重新探测状态 |
| R9 | 不能安全解绑的关系必须在产生解绑副作用之前明确拒绝，不能静默破坏依赖 |
| R10 | 不新增公开设备 ID、绑定 ID 或第二份设备描述；状态与元数据各有唯一权威来源 |

非目标：驱动模块卸载、unregister_driver、强制选择驱动、runtime PM、
完整 supplier/consumer 图，以及在本次改动中重做 provider 注册协议。

## 3. 两条生命周期

设备注册状态建议为 Registered / Removing / Removed；表示注册关系，
不使用 Active 来同时暗示设备存在和驱动工作。

绑定状态建议为 Unbound / Pending / Probing / Bound / Deferred /
Unclaimed / Unbinding：

- Unbound：显式保持未绑定，不参加自动匹配与依赖唤醒。
- Pending、Probing、Deferred、Unclaimed：保留现有探测与失败回退语义。
- Bound：持有成功 probe 返回的 Data。
- Unbinding：独占退出本轮绑定，禁止并发重新探测。

主要转换：

```text
注册设备：Registered + Pending
探测成功：Registered + Probing -> Registered + Bound
显式解绑：Registered + Bound -> Registered + Unbinding -> Registered + Unbound
重新探测：Registered + Unbound -> Registered + Pending -> ...
注销设备：Registered -> Removing -> Removed
```

Deferred 与 Unclaimed 也可以显式解绑为 Unbound：撤销调度和等待，不调用
没有成功绑定时的 remove。Probing 中的解绑/注销返回 Busy，不强行取消回调。
注销中的暂存组合由内部状态机约束，不能直接对外修改枚举字段。

DeviceSnapshot 分别报告注册状态与绑定状态，不再存一份可独立修改的扁平状态。
调度器只消费注册状态为 Registered 且允许调度的绑定状态；Unbound 不入队。
注销的重试状态仍由设备注册生命周期管理；探测的旧结果校验仍由 domain
内部 generation 管理，不给 publication 引入数字身份。

## 4. 责任与所有权

| 对象 | 责任 |
|---|---|
| DeviceCore / DeviceRef | 设备描述、稳定对象身份及设备注册是否已关闭；不持有驱动 Data |
| domain 的绑定记录 | 当前驱动、探测/解绑状态、Data、候选历史和本轮发布来源 |
| 每次 probe 的发布来源 | 关联同一设备与该次驱动尝试，拥有不可逆关闭的可用性门禁 |
| DeviceAccess | 借用当前设备和当前尝试，资源请求仍使用同一 DeviceRef，publish 使用本轮来源 |
| Environment | 继续提供框架服务，不对驱动暴露绑定内部状态或伪装成设备资源所有者 |

每次 probe 创建独立的内部来源对象；成功后由绑定记录继续持有。
它通过引用查询 DeviceCore 的路径/描述，通过共享驱动记录查询名称，不复制
路径、描述或驱动元数据字符串。在线判断同时考虑设备注册与本次尝试是否关闭。
设备仍注册不意味着某次旧绑定仍提供能力。

失败、Deferred 和解绑都会永久关闭本次来源。下一次 probe 创建新来源，
不把旧来源的标志翻回在线。已有 class draining/在途许可与 Registration
协议保持不变；来源关闭不是等待在途操作退出的替代品。
驱动仍须在 remove 中 quiesce/join，或让在途 runtime 持有它需要的资源。

来源可以强持有设备与共享驱动记录，但不能持有 Data、domain 或 DeviceAccess。
若 DeviceCore 为公开 driver_name 查询关联当前来源，只使用 Weak；不能形成
core -> 来源 -> core 的强引用环。domain 的绑定记录是当前来源的权威所有者。
建议将 DeviceRef::driver_name 明确调整为当前有效 probe/绑定来源的名称，
失败及解绑后返回 None；不再把“上一次尝试”作为当前身份信息。
这是公开查询语义变更，必须单独在文档和测试中确认。

## 5. 管理接口与流程

建议增加 `unbind_device(&DeviceRef)` 与 `reprobe_device(&DeviceRef)`，
保留 `remove_device` 作为注销入口。接口名称及错误类型在实施前定稿。

### 解绑

1. 锁内核对 domain 与 Arc 身份、注册/绑定状态和可解绑条件。
2. 对 Bound 原子认领 Unbinding，移出 driver/Data/来源；关闭该轮来源。
   从这一阶段起也禁止新孩子及控制器实例挂接，防止检查后出现新依赖。
3. 锁外调用 remove(dev, env, data)，随后销毁 Data，按原协议撤销注册。
4. 锁内完成为 Registered + Unbound，不自动入队，不改变父关联及设备身份。
5. 对已是 Unbound 的请求幂等成功；并发 Unbinding/Probing/Removing 返回 Busy。

Pending/Deferred/Unclaimed 路径锁内撤销 pending/deferred 项及相关重试状态，
转为 Unbound；保持未绑定与已经注销的设备在错误返回上可区分。

### 重新探测

只接受 Registered 的 Unbound/Unclaimed/Deferred 设备。Bound 先显式解绑，
不把“重探测”隐藏成一次可能有副作用的资源拆除。清理本轮候选失败记录和
旧等待记录，进入 Pending；保留避免丢失唤醒所需的域依赖 generation 协议。
重新探测走原有匹配与 probe 泵，不保证匹配成功，也不保证更换成另一个驱动。

### 注销

先认领 Removing 并冻结拓扑，继续现有 children-first 及桥接实例级联。
内部复用绑定退出步骤，而不是在已经 Removing 时调用只允许 Registered 的
公开 unbind_device。完成后是 Removed，不能经过可重新探测的 Unbound 窗口。

## 6. 第一阶段安全范围

先支持没有活动下游拓扑、没有不可撤销 provider 发布的设备；这是明确限制，
不是完整 Linux unbind 的实现。

- 有仍注册的孩子、仍注册的桥接实例成员，或本驱动所建实例尚不能安全退休时，
  拒绝公开解绑。不从 parent 关系猜测哪些孩子由本轮绑定拥有，也不一律注销孩子。
- 成功调用现有静态 provider 注册的绑定暂不允许解绑。框架需在 provider
  注册成功处记录本次回调的副作用，用于解绑准入判断，不能只靠驱动口头保证。
  该审计不等于取得 provider 所有权，不新增虚假的撤销语义。
- 首阶段不支持驱动通过框架之外的入口发布可影响解绑安全的资源；这类外部
  副作用必须由具体驱动契约另行处理，框架不声称能自动检测任意代码行为。
- 原有 provider 在 probe 失败/设备注销时的静态注册限制仍存在，本文不宣称
  修复该问题；不能借新的解绑接口扩大其可触发范围。
- 已发出的在途许可不因解绑强制失效；允许旧 runtime 的安全收尾与新绑定并存，
  前提是旧运行资源确实独立持有或已 quiesce，不能只保留裸硬件地址。

受限原因需要可诊断，区分生命周期 Busy、下游拓扑阻挡、静态 provider 阻挡。
禁止因解绑被拒绝而自动退化成 remove + add。

## 7. 验收

1. 解绑前后 same_device 为真、descriptor/路径/父关联不变，资源只释放一次。
2. 解绑后 is_online 的设备注册语义保持为真，绑定状态为 Unbound；设备注销后为假。
3. 新驱动注册、依赖 wake 不唤醒显式 Unbound；reprobe 才重新进行匹配。
4. 失败候选历史不阻止显式 reprobe；Deferred 的唤醒竞态回归仍通过。
5. 保留旧 Publication 跨越解绑和重绑定，旧句柄始终不可获取新许可；新发布可用。
6. 旧 publication 的路径仍指同一设备，驱动来源不变成新驱动；无强引用环。
7. probe 失败或 Deferred 后保留的发布句柄永不复活，Data/来源销毁不在域锁内。
8. 在途许可按原协议完成，注销/解绑不提前释放它依赖的资源。
9. 并发解绑、解绑与注销、解绑与挂接孩子只有一个合法认领者；拒绝路径无部分清理。
10. 非叶子/静态 provider 设备的解绑在回调及资源释放前拒绝，原绑定仍可用。
11. 对象外域、同键不同对象、已注销、未匹配、重复解绑均有明确结果。
12. 既有 domain/class/os-api/hwrng/entropy UT 全部回归。

## 8. 实施顺序

1. 分离注册/绑定状态与快照查询，先保持现有操作行为，补齐状态机测试。
2. 引入每次 probe 的不可复活发布来源，覆盖失败/Deferred/class 生命周期。
3. 建立解绑准入检查与 provider 副作用审计，再增加叶子设备解绑与显式 reprobe。
4. 注销复用内部绑定退出流程，补齐并发、拓扑和在途操作验收。

目录重构、公开 API 行为变化与新增功能应分开提交；每一步同步实现文档，
不得把本草案中的目标语义直接写成已经实现。
