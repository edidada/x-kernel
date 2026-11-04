# RNG 全生命周期：Linux 本地源码职责参照

## 基线、用途与证据范围

- XKernel：`7f9798126aca8af461222d89a2cd72fc0c06d63a`。
- Linux：`/root/linux-stable`，HEAD
  `8d3ae59288f1e7d58d76558a6ee96d533bc5019f`。
- 下文 `Linux:` 路径均相对上述 Linux 树；`XKernel:` 路径相对本仓库。
  行号来自本次实际读取，不代表其他 Linux 版本。仅本地源码研究，未运行
  构建、设备、unbind 或模块卸载实验。
- 本文提供横向设计依据，不将 Linux 的完整功能集变成本框架新增验收项，
  不把“实现不同”直接认定为缺陷。特别不据源码排列宣称硬件 DMA 已安全。

## 1. 对象不是同一层：发现、transport、功能、能力

| 对象/名称 | 所属职责与证据 |
|---|---|
| `device_node` | OF 固件节点；不是 RNG 运行对象。`Linux:drivers/of/platform.c:151` 的创建路径先检查 available 与 `OF_POPULATED`，再分配设备 |
| `platform_device` / `platform_driver` | 固件资源对应的 platform 实例及其驱动；`platform.c:165` 调用 `of_device_alloc`，`:172` 设 platform bus，`:176` 注册。注意这里是 `drivers/of/platform.c` |
| `virtio_mmio_device` | MMIO transport 私有对象，内含 `virtio_device` 并保留 `pdev`；`Linux:drivers/virtio/virtio_mmio.c:572` |
| `virtio_device` / `virtio_driver` | transport 发布的 VirtIO 总线设备及功能驱动；不是 OF 平台设备的别名。`:582` 将其 parent 设为 `pdev->dev`；`Linux:drivers/virtio/virtio.c:526` 注册并分配 `virtio%u` 名称 |
| `virtrng_info` | 每次 RNG 驱动绑定的私有运行状态：hwrng、vq、完成量、缓存及移除标志；`Linux:drivers/char/hw_random/virtio-rng.c:21` |
| `struct hwrng` | 随机设备子系统的 provider 描述，含 read/cleanup/priv/name；不是通用 device 身份。`virtio-rng.c:168` |
| `current_rng` / `hwrng_fill` | hw_random core 的选源及喂熵线程；`Linux:drivers/char/hw_random/core.c:128`、`:509`。不能等同于驱动绑定关系 |

### 发现到绑定

1. `Linux:drivers/of/platform.c:443` 的 `of_platform_populate` 遍历节点；
   `:325` 的 `of_platform_bus_create` 检查 compatible/跳过表，创建 platform
   对象，并仅对匹配为 bus 的节点继续递归。`:361` 对 arm,primecell 转 AMBA。
   因此不是把整棵 DT 所有节点无差别注册为 platform 设备。
2. `Linux:drivers/base/platform.c:1368` 的 platform match 处理 override，
   再 OF、ACPI、id table、名字 fallback。MMIO transport 驱动的 OF 表是
   `"virtio,mmio"`（`drivers/virtio/virtio_mmio.c:786`），不是 RNG 类型。
3. transport probe 映射资源、读取 magic/version/device/vendor；支持版本
   1/2，device id 0 返回 `-ENODEV`，不注册 VirtIO 功能设备
   （`virtio_mmio.c:587–619`）。平台 slot 对象与功能设备存在性因此不同。
4. `virtio_mmio.c:642` 注册 VirtIO 子设备；`virtio.c:555–568` reset、ACK
   然后 device_add，触发 VirtIO 总线匹配。RNG id 表匹配 `VIRTIO_ID_RNG`
   （`virtio-rng.c:263`）。此时是正式 transport 创建，不是无副作用分类 API。
5. RNG probe 创建 runtime/队列并提交首个请求（`virtio-rng.c:151–188`）。
   hwrng 注册在 `virtrng_scan`（`:223`），由 VirtIO core 在 probe 成功和
   DRIVER_OK 之后调用（`virtio.c:347–358`）。probe 成功不等于 hwrng 注册成功：
   scan 是 void，失败时 `hwrng_register_done` 不置位。

**对照判断：** 当前 XKernel 在 platform 发现阶段只读分类，并直接把选中的
modern RNG 交给 `RngDriver`，不是上述两层总线完整复刻
（`XKernel:drivers/integration/kdriver/src/bus/platform_backend.rs:156`；
`drivers/integration/kdriver/src/driver_registry/virtio/rng_model.rs:38`）。
这是已验收单设备家族切片的结构差异。未来通用 MMIO 迁移应明确 transport
实例、功能设备和子对象 owner；不能仅因 Linux 有两层就立即新增空框架，
也不能把当前直接接线描述成已经实现通用 VirtIO bus。

## 2. hwrng 注册与读取：子系统不是硬件 stop owner

- `Linux:drivers/char/hw_random/core.c:562` 校验 provider、拒绝同名、加入
  rng_list，并按配置/quality 决定当前来源。这里的质量选源不是 driver-core
  的跨驱动匹配评分，两者不可混淆。
- `/dev/hwrng` 的 read 路径每轮取得当前 RNG 的引用，并在 reading_mutex 下
  调用取数（`core.c:240–263`）；get/put 路径在 `:162–179`。这种“读取时选择
  当前源”不能直接类比固定绑定代际的 XKernel `HwrngHandle`。
- 喂熵线程同样取得当前 RNG、串行取数、释放引用，随后调用
  `add_hwgenerator_randomness`（`core.c:509–556`）。Linux 有 quality/credit
  策略，不意味着 XKernel 必须复制它或已经验证真实 pool credit。
- RNG `random_recv_done` 先 `virtqueue_get_buf`，再发布 data_avail 并 complete
  （`virtio-rng.c:42–52`）。`virtio_read` 可等待；缓存耗尽会再次提交请求
  （`:70–141`）。probe 已预提交，注释明确“always have a pending entropy request”。
- `virtio_cleanup` 只唤醒 have_data（`:144`），不是 reset 或 queue teardown。
  不应把这个 cleanup 名字映射成 XKernel 的完整硬件退出 helper。

XKernel 的按需求提交、独立 entropy collector、唯一 HwrngRegistration、
旧 handle 永不转接新绑定，是不同的可接受职责划分。其 unregister 关闭准入、
撤销注册并等 in_flight，而不是等待所有外部 Arc 消失
（`XKernel:drivers/subsystems/hwrng/src/registration.rs:177`）。

## 3. remove、unregister、unbind 与 rebind 必须分层命名

| 操作 | Linux 语义 | XKernel 对照/注意 |
|---|---|---|
| 解绑一个 RNG 驱动 | sysfs unbind 调 `device_driver_detach`，没有 device_unregister；`Linux:drivers/base/bus.c:235`。最终执行 bus/driver remove 并清理绑定；`dd.c:1318` | `DeviceDomain::unbind_device` 保留注册，结束绑定；`XKernel:drivers/base/model/src/domain/lifecycle.rs:46` |
| `virtio_driver.remove` | 结束这次功能驱动绑定；`virtio.c:368` 调 RNG remove 后检查 status，再 ACK 存在的设备 | 对应 DriverOps::remove 的绑定清理，不等于删除设备身份 |
| RNG hwrng 注销 | 撤销能力 provider 并等待 cleanup_done；`hw_random/core.c:611–632` | 对应 HwrngRegistration::unregister，不删除 domain 设备或 transport |
| 解绑 platform transport | `virtio_mmio_remove` 注销其创建的 VirtIO 子设备；`virtio_mmio.c:653` | 子对象退出属于创建者责任；不应因此要求通用 domain 自动递归删除一切 child |
| 注销设备 | `unregister_virtio_device` → `device_unregister`；后者 device_del + put_device，最终内存释放等最后引用；`virtio.c:590`、`base/core.c:3999` | 当前 `DeviceDomain::remove_device` 实际是单设备注销（`domain/lifecycle.rs:62`），旧引用只留离线身份 |
| 再绑定 | Linux driver 的 bind 文件指定目标 driver，要求 match 后 attach；`base/bus.c:258` | XKernel `bind_device` 重新按注册顺序选候选（`domain/probe.rs:78`），不是“指定某驱动”API；差异不是错误 |

RNG unbind/rebind 可保留同一 `virtio_device`，但会销毁/新建 `virtrng_info`，
重建队列和 hwrng 注册。platform transport unbind 则销毁其 VirtIO 子设备；
transport 再 probe 会注册新子对象。不能把这两种 rebind 的身份保证混写。

**命名建议（未实施）：** 文档始终将 XKernel `remove_device` 标为“注销设备”，
将 DriverOps::remove 标为“解绑回调”。未来若统一公开 API 名称，可评估
`unregister_device`，但不能仅为追随 Linux 同名函数机械改名。Linux 自身的
remove 是绑定回调，而 unregister 可以指 device、driver 或 hwrng 三种对象。

## 4. reset/drain 的实际顺序，不能套用想象中的 Linux 模板

Linux RNG `remove_common`（`drivers/char/hw_random/virtio-rng.c:197–210`）：

1. 标记 hwrng_removed，清空缓存状态，唤醒阻塞读取。
2. 若注册成功，调用 hwrng_unregister。core 从列表移除、通知 dying、重选
   当前源或停止填充线程；锁外等待 cleanup_done（`hw_random/core.c:611`）。
   最后引用触发 cleanup work，工作调用 provider cleanup 再 complete
   （`:66–92`）。这不是简单“list_del 后立即 free”。
3. 调 virtio_reset_device，然后 transport del_vqs，最后释放 index/runtime。

必须保留以下源码限定：

- `Linux:drivers/virtio/virtio.c:253` 仅在
  `CONFIG_VIRTIO_HARDEN_NOTIFICATION` 下先 break_device + synchronize_cbs，
  随后调用 transport reset。不能写成所有配置无条件先 IRQ drain。
- MMIO `vm_reset` 写 status 0（`virtio_mmio.c:251`）；这个函数没有 status
  读回轮询。不要将 XKernel“显式 poll_stopped”的实现细节投射到 Linux。
- MMIO `vm_synchronize_cbs` 调 synchronize_irq（`:339`）。
  `vm_del_vqs` 先逐队列删除，再 free_irq（`:328`）；modern queue 删除中有
  QUEUE_READY 清零及读回（`:311`）。这也不是“所有资源先 free_irq 再释放”。
- `vm_interrupt` 遍历 vqs 调回调（`:285`）；`virtio_device_for_each_vq` 本身是
  list 遍历宏（`include/linux/virtio.h:221`）。`vring_del_virtqueue` 的 list
  修改锁和内存释放可见于 `drivers/virtio/virtio_ring.c:3516`。
  本文不从这些片段单独推出所有配置/控制器的并发安全证明，更不据此放宽
  XKernel 已批准的同步 drain 合同。

XKernel 的实际 binding shutdown 依次 close_reads → hwrng unregister/drain
→ finish_stop → MMIO 释放（`drivers/integration/kdriver/src/driver_registry/virtio/rng_model.rs:156`）。
finish_stop 取出资源，在锁外 reset/确认、释放同步 IRQ guard、取消尚 pending
的原 token、释放队列（`drivers/devices/virtio/src/rng.rs:435`）；通知在 request
锁外（`:541`）。正常完成的 token 已先回收，尾部 drain 保护的是仍存活的
queue/资源；不能再次 cancel。这种更显式的顺序不是“比 Linux 多所以过度设计”。

## 5. 驱动注销与拓扑退出：当前范围差异

- Linux `unregister_virtio_driver` → driver_unregister → bus_remove_driver
  （`drivers/virtio/virtio.c:469`、`drivers/base/driver.c:195`）。bus 先撤销
  driver 列表/接口，再 driver_detach 处理其绑定，最后释放 driver 对象
  （`drivers/base/bus.c:818`）。注销驱动不等于注销总线上所有设备。
- RNG 使用 `module_virtio_driver`（`virtio-rng.c:278`）；MMIO transport 独立
  注册/注销 platform_driver（`virtio_mmio.c:815`、`:820`）。卸载哪一层，
  决定仅解绑 RNG 还是同时注销 transport 的功能子设备。
- OF depopulate 只递归销毁它负责 populated 的节点，保留不归它创建的对象
  （`drivers/of/platform.c:613–650`）。这是具体 population owner 的职责，
  不证明任意 device parent 关系都应由通用核心递归退出。
- 当前 XKernel model 源码没有 unregister_driver 入口（本次搜索
  `drivers/base/model/src`）；不支持动态模块完整卸载是已批准范围边界，
  不是当前 RNG bind/unbind/rebind 的遗漏。将来若要求驱动可卸载，需单独
  设计候选撤销、正在 probe 的引用、各绑定排空和最后代码引用，不能只加
  Vec::remove。

## 研究结论与下一步问题

1. 可立即澄清的是名称：设备注销、驱动解绑、能力注销、驱动注销分别指明对象；
   “remove RNG”不足以表达操作。PublicationSource 是绑定来源元数据，不是
   Linux bus 或驱动状态的镜像，也不是新的硬件使用许可。
2. 扩展通用 MMIO 前需决定 transport/功能对象是否分层、谁拥有空 slot、
   谁撤销子注册；这是真实设计问题，而不是复制 Linux 类型名称的任务。
3. Linux 的 scan 分阶段发布、质量选源、预提交、模块/PM、sysfs bind 指定驱动
   都有具体用途；没有当前需求时不能据此要求增加相同框架机制。
4. 当前 RNG 的显式资源 bundle、注册权、retained handle 与同步尾部排空
   各自保护不同生命周期。判断是否冗余应检查是否重复拥有同一事实，不能以
   Linux 用 C 内嵌结构/引用计数替代 Rust owner 就认定 XKernel 过度设计。
5. 本文没有发现凭上述 Linux 差异即可成立的当前实现缺陷，也不是完整安全
   审计；特别未验收硬件晚到 DMA、IRQ/EOI、动态卸载或未来 platform 迁移。
