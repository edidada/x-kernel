# 真实接入契约：完成等待、DMA 与 VirtIO 退出

状态：设计候选；本稿限定实际 hwrng 接入所需变更，不授权改代码或依赖版本。
承接 [结构设计](architecture-v0.2.md) 与 [退出协议](exit-protocol-v0.1.md)。
目标是明确每层传递什么、谁持有资源、何时可以释放，避免“由 adapter 处理”
成为未实现机制的代称。

当前评审方向：第 9 节核对 Linux 与宿主等待实现。第 1 节的 prepare_wait、
整树准备，以及第 8.3 节的前置等待准备是待比较机制，不是已确定的公开接口。
优先在宿主同步等待层解决登记问题，暂停向 DriverOps 增加通用 prepare_remove。

最新工作约定：第 11 节汇总当前已识别前置能力。用户允许在这些能力成立的假设下
继续框架设计，具体实现先后留到拆票；不因前置尚未实现而停止设计，也不将假设
写成已验证事实。与早期具体准备算法冲突时，以能力契约及结构设计第 12 节为准。

## 1. 一次性完成 Interface

建议在 driver-os-api 的 completion mod 定义能力契约，不实现任务调度。
以下为操作语义而非最终 Rust 签名：

| 操作 | 返回/效果 | 前置条件与错误 |
|---|---|---|
| 创建 completion | 一个初始未完成的共享完成对象 | 宿主已准备好，分配失败按宿主既有策略处理 |
| is_complete | 终态快照，不消费结果 | 不睡眠；不能代替等待登记 |
| complete | 单调置为已完成并通知；重复调用无额外效果 | IRQ-safe、通知路径无分配；在业务状态锁外调用 |
| prepare_wait | 当前任务专用的已登记等待凭证 | 可睡眠任务上下文；失败返回且不改变业务 gate |
| prepared.wait | 直到 completion 完成才返回 | 不再次分配、不重新登记；虚假唤醒重查；不可跨任务使用 |
| 凭证 Drop | 取消当前等待登记 | 不取消业务退出，不消费 completion；允许已脱离登记的迟到唤醒 |

不提供 reset/reinit：每次操作或作用域持有独立完成对象，避免旧等待者被
下一次绑定的重置影响。普通 wait 返回不是设备工作成功，只是对应完成事实成立；
业务结果由操作记录提供。

### 宿主实现方案

复用 kpoll::Completion 的 complete_all/is_completed、等待登记，以及
ktask::PreparedTaskWait。任务先 prepare，再检查终态、登记、复查；wait 只在
未完成时 park，虚假唤醒不丢弃尚未通知的登记。完成方仅在业务终态成立时
complete_all，不能把普通进度更新也发送到这个 completion。

接口中不暴露 ktask 对象、任务 ID 或可复用调度器。等待凭证不声明 Send，
生命周期绑定当前任务；具体实现继续验证上下文。准备阶段有副作用的仅是
通知登记，不是撤销资源。不可逆退出所需凭证由退出计划统一准备。

### 多对象退出的准备

不能宣称“准备一个父设备 waiter 就保证整棵树退出不再失败”。管理层先读取
稳定对象引用和关系版本，短锁内复核并认领涉及的管理操作，再在锁外准备
本次计划需要的凭证。认领阻止所属集合继续变化，但不关闭消费者数据面；
准备失败取消临时认领，没有不可逆设备变化。具体协议见退出协议第 10、11 节。
驱动内部 IRQ/发布等待也必须纳入其停机准备协议，不能在模型关闭后才临时
申请一个可能失败的新等待者。计划根据注册所有权收集，不由静态 parent 字段
单独推导；子设备退出期间父控制器服务仍需可用。

## 2. 每设备 DMA 能力的传递

### 事实核对

- `arch/khal/src/firmware/devices.rs` 的 FirmwareDevice 当前只有 firmware_id、
  source、MMIO 和 IRQ，没有 DMA 配置。
- 新 DeviceDescriptor 有 Option<DmaInfo>；None 表示尚未配置，不代表无 DMA，
  当前模型会拒绝该设备的 DMA 分配。
- 旧 VirtIoHalImpl 使用静态 Hal 方法和全局 provider；它不会根据新 descriptor
  自动改变地址转换、mask 或一致性处理。
- kvirtiodrivers 0.13.4 的 Dma、VirtQueue 均直接调用 H::dma_alloc/share/unshare，
  不能仅给最外层 VirtIoRngDev 增加一个 DeviceRef 就使底层操作具备设备语义。

### 目标关系

```text
固件/平台发现事实
  → 注册设备的 DMA 配置（唯一事实来源）
  → 宿主为该设备创建 DMA 能力实例
  → 具体驱动持有实例
  → VirtIO queue / ring / request 通过该实例分配和映射
  → 分配/映射凭证记录配对释放所需事实
```

model 的 host 接口增加“为当前设备取得 DMA 能力”的入口，输入是已有受约束
的设备访问上下文，输出为可持有的 os-api 能力。能力不暴露宿主类型，也不要求
os-api 依赖 model；宿主实现内部关联 DeviceRef 或不可变设备配置。

这是实例能力，不是“全局 DmaOp + 每次随意填写设备 ID”。驱动不可通过换一个
编号让已分配资源被另一个设备释放。能力保活支持配对回收，不授予越过停止
状态继续提交 DMA 的权限，硬件提交仍由驱动运行状态约束。

### 分配与映射所有权

ring allocation、请求 buffer 的 streaming mapping、间接描述符 mapping 都必须
通过同一个实例后端。每次成功分配/映射返回唯一释放义务；失败不遗留已发布给
设备的 descriptor。CPU 地址、设备地址和长度是释放该次分配所需事实，不是另一份
可独立修改的设备 DmaInfo。不得从设备 bus_addr 猜测原分配的 CPU 地址。

建议将现有可 Copy 的 DmaAllocation/DmaMapping 保留为底层描述或调整为非拥有
返回值，与独占释放 guard 区分；不能把可复制描述当成可以多次释放的凭证。
必须校验完整区间和溢出，不只检查起始地址是否满足 mask。

## 3. VirtIO 库的实例后端扩展

推荐增加实例化 DMA 后端入口，由 RNG、queue 和 ring 逐层持有/传递，不使用
TLS、不使用一个全局“当前设备”，也不按设备实例生成全局 Rust 类型。

对现有静态 Hal 路径提供兼容适配，旧构造入口继续得到旧后端；新 RNG 路径
选择显式传入实例后端的构造入口。内部 queue 分配、buffer share/unshare
改为走后端，而不是复制一套 RNG 专用 virtqueue 实现。
具体泛型默认参数或内部表示由库补丁确定，不能承诺零内部改动，但未迁移
设备的外部构造/行为需保持兼容，并跑对应 queue/device 回归。

新实例后端必须能将申请/映射失败传播为错误；不能沿用“返回 dangling 指针
或零地址后假装成功”。准备 descriptor 失败时撤销本次已经完成的映射，只有
整组资源就绪才更新 avail ring。设备一旦可见请求，则改由完成/取消协议回收。

数据映射回收凭证应保存在对应请求/descriptor 的私有记录，不新增跨设备全局
映射表来恢复上下文。库不依赖 X-Kernel 或 driver-model；具体 os-api 到库契约
适配位于具体驱动/宿主接入的自然边界。

当前 vendor 的 DmaBackend 返回裸地址，并在完成/回滚时以原 tuple 释放，不能
直接存放 os-api guard。本轮采用设备实例私有的适配记录保存 guard，backend
clone 共享同一记录；记录只保存释放凭证，不复制设备配置，也不跨设备共享。
释放校验完整 tuple，锁内摘出、锁外 Drop；不得覆盖仍有效的记录。这是在现有
库接口上的所有权适配，不新增全局 DMA 注册表，也不另造 queue 生命周期。
元数据遵循当前内核的不可恢复分配失败策略，不承诺无分配或固定延迟。

回滚继续调用普通 unshare，无需新增 vendor 撤销接口。DeviceDma 必须保证未发生
设备写入时 map→release 保留原内容，部分写入时保留其余字节。宿主 RX bounce
因此预拷贝原 buffer，而不是仅清零；本地 Linux kernel/dma/swiotlb.c 的
swiotlb_tbl_map_single 同样无条件预拷贝，以保持 bounce 对部分写入透明。
这不允许在硬件仍可能访问时释放已发布请求，reset/quiescence 责任不变。

ACCESS_PLATFORM 不能从“没有 IOMMU”推导为可忽略。当前适配构造要求调用方证明
held capability 对 platform-DMA 和 bypass 两种协商模式都返回设备实际可用的
地址，并完成平台共享；目前计划的 QEMU identity DMA 路径满足这一条件。
任意非 identity window 或未来 IOMMU 路径不能自动沿用，需先补显式模式能力，
不能为了兼容 false 就跳过平台共享或绕开 descriptor 的 DMA 约束。

### 固件配置的边界

补充 firmware discovery 输出到设备描述的转换；固件解析层保持宿主/固件
词汇，不反向依赖 driver-model。平台默认必须来自已核对的平台规则，不把
缺少属性无条件解释成全地址、零偏移、一致性 DMA。

当前 DmaInfo 只有单 offset/mask/coherency，不能宣称表达任意多段 dma-ranges。
本阶段支持范围须依据真实 QEMU MMIO 路径证明；遇到无法表达的配置明确拒绝，
不得静默丢弃约束。如果真实迁移路径需要多段能力，先完善表示再进入实现。

## 4. 未完成请求取消的 Interface

取消不是伪造一个 used entry，也不等于丢掉读者的等待凭证。
推荐在库内把以下两个动作分开：

| 动作 | 权限与证明 | 结果 |
|---|---|---|
| 停止 transport/queue | 驱动先阻止新提交，transport 确认该设备不再访问队列与 buffer | 该次队列进入不可再提交的停止状态 |
| 回收停止队列的 outstanding token | 原请求/token/映射仍由队列持有，不依赖设备 used ring | 配对解除映射、回收间接表及请求资源，不报告正常读取成功 |

外层不能手写 trusted=true 跳过硬件停止。候选让停机操作在库内产生对该队列
的停止状态/私有凭证，只有该状态允许取消 outstanding 请求；不把它做成公共
可构造的全局“已停机”令牌。正常完成和取消都原子地消费同一请求所有权，
重复取消/迟到 IRQ 不能再访问已经回收的映射。

RNG 的 read 路径在停止后返回终止结果，不把取消的 buffer 当作随机数据。
IRQ handler 在停止状态不再操作被取走的队列。硬件确认完成之后，仍需同步
已有 handler；同步时不能持该 handler 要取得的请求锁。

reset 失败/无法确认停止时保留资源所有权，不允许 rebind 或报告正常退出。
当前无需求指定强制恢复时限，不新增模型级 timeout；transport 定义自己的
完成/失败语义，model 不能替它推断硬件已停。

## 5. IRQ 同步释放补丁的边界

前置修正位置仍是 kirq，不把 IRQ dispatch 状态复制进 class/model。
需要把可能失败的等待准备移到 action 移除之前，准备后在控制锁内重新校验
原 action/token 和描述符状态，再开始不可逆移除。移除后等待已经取得快照的
handler，完成才释放回调所有权。共享线不能因为退出一个 action 而无条件关闭。

特别注意：当前 in_flight completion 属于 IRQ 描述符，不能未经证明将它等同于
新 class 的一次性 completion。等待前后可能有其他共享 action 的 dispatch，
具体 kirq 补丁必须证明重置/重检查和等待资源复用，不机械套用 class 的单调 gate。
新 os-api 同步释放接口提供与此一致的上下文/成功保证；预检错误必须返回时
仍保有未释放资源，调用方不能继续 drop 正在使用的运行对象。

## 6. 枚举和启动接线补充

不只要在旧 device_desc_add 之前分流，还要保持发现入口有效：目前 platform
枚举的 firmware_match_specs 来自旧驱动注册表。若移除旧 RNG 驱动注册却不
补发现条件，新 RNG 可能根本不会进入枚举结果。

宿主装配应汇总新旧路径需要的固件匹配条件，进行一次发现，再按设备事实
决定唯一注册目标；匹配条件不能通过创建旧影子设备获得。其他 VirtIO 类型
仍沿原路径；新路径失败不能自动落回旧 probe，避免双重管理。

provider 安装顺序须先于新模型 probe/class 发布。模型 UT、宿主等待 UT 和
真实启动分别验证，不能用未安装 worker 的启动阶段调用 DeferredArc::new
后假定其准备一定成功。

## 7. 评审与验收清单

- 两个实例注入不同 DMA 视图，并发分配/映射的地址和释放回调不串用。
- mask/区间溢出、分配/映射失败不向设备提交不完整 descriptor。
- Pending 请求在无进一步设备完成通知时也可通过已证实停机后的取消安全回收。
- 正常完成与取消交错，只回收一次；取消结果不被当作有效熵。
- 退出等待的 prepare 失败发生在不可逆操作前，虚假唤醒不触发再次分配。
- 撤掉旧 RNG 注册后仍可枚举新路径，其他 VirtIO 设备不受影响。
- 库补丁按正式源码/依赖版本交付；不修改 Cargo registry 缓存作为最终方案。

本稿收敛的是接口边界及依赖补丁要求，尚未执行依赖原型、编译或运行验证。
具体库补丁兼容性、固件实际 DMA 配置及 kirq 共享线等待算法仍须在开发票
开始前完成设计核对；不能把这些项仅写成 TODO 就宣布完整设计可实施。

## 8. 本轮核对与方案收敛

### 8.1 实际 QEMU 设备树

使用本机 QEMU 10.0.11 导出设备树，机器/CPU 对齐当前 aarch64 TCG 入口的
`virt,gic-version=3` / `cortex-a76`，128 MiB 内存，仅添加 RNG；没有启动内核，
没有接磁盘/网络/vsock，没有性能测量。命令：

```sh
qemu-system-aarch64 -machine virt,gic-version=3,dumpdtb=/tmp/driver-model-dtb.tiREZq/virt-gic3.dtb -cpu cortex-a76 -m 128M -nodefaults -display none -object rng-random,id=host_rng0 -device virtio-rng-device,rng=host_rng0
```

定向解析 FDT 得到：根节点及全部 32 个 virtio_mmio slot 均有 dma-coherent；
slot 的 reg 窗口大小为 0x200，compatible 为 virtio,mmio；本次树中未见
dma-ranges、iommus 或 iommu-map。RNG 的具体设备类型仍须读 MMIO device id，
不能把 32 个 slot 全当作已经存在的 RNG。

结论限定为这次机器配置：一致性有明确固件依据，不必为第一条真实路径默认
开启新的非一致性硬件支持。固件接口必须把这个事实传递出去。没有 dma-ranges
不单独证明任意平台都支持全 64 位地址，mask/地址转换仍须由所选平台与传输
规则共同确定。后续测试机配置变化时应重新取证，而非复用本次结论。

临时 DTB 只作本地调查证据，不加入仓库，也不等同于完整 make/QEMU 启动验收。

### 8.2 VirtIO 兼容入口的具体落点

选择增加实例 DMA 后端并保留旧默认后端的方案，不新建第二套队列算法。
候选类型关系可表达为 VirtQueue 的默认后端是 StaticHal<H>，显式构造时接受
实例后端；Dma/ring layout/RNG 同样传递该后端。实际 Rust 名称仍随库补丁评审。

旧 `VirtQueue<H, SIZE>::new`、`Dma<H>::new`、`VirtIORng<H,T>::new` 保留其调用
形状，通过默认后端转发原 Hal；新入口使用 new_with_dma 一类显式构造。
内部所有 ring 分配、descriptor share/unshare 必须经过所持后端，不能只改
RNG 构造入口。新接口以 Result 传播失败；旧适配保留原错误约定，不悄悄改变
其他设备的错误 ABI。

该结构要求检查：默认参数推导、Send/Sync 是否需要新后端约束、旧设备构造
是否仍编译、legacy/flexible ring 布局、direct/indirect descriptor、部分映射失败
回滚及两种后端的分配/释放配对。不是“给 struct 多加一个字段”的无风险变化。

兼容验收至少覆盖库内 queue 与 RNG、项目内 blk/net/input 等已有调用方；
不要求这些设备迁移到新模型。依赖补丁不通过这些回归，真实接入不得越过它。

### 8.3 共享 IRQ：退出等待属于被移除 action

源码已经确认：当前 IRQ 描述符 in_flight 在 0→1 时 reinit 同一个 completion，
归零时 complete_all；每次 dispatch 克隆 actions 快照。现有等待桥在循环中
可能重新登记。这既不是一次性终态，也不是特定 action 的生命周期。

候选修正为 action 私有的派发存续记录，由 kirq 拥有，而非由驱动或 model 统计。
记录具有“已移除/旧快照使用数/一次性完成”三个相关事实：

1. action 注册时建立记录及完成通知，尚未移除时即便使用数为零也不完成。
2. 在 IRQ_STATE 的快照选择临界区，为选中的每个 action 取得派发许可；快照
   离开全局锁前这些引用已被计数，随后执行 handler 不持 IRQ_STATE。
3. 每个快照最终归还许可，包含未执行到该 handler 的提前退出路径；快照不能
   通过普通 Clone 绕过计数。新增记录不要求 IRQ 路径分配内存。
4. 退出方先为该 action 的一次性完成准备等待者；失败时 action 仍注册。
5. 在控制锁/IRQ_STATE 下重新核对 token 与原记录一致，移除 action 并标记
   已移除；不再有新快照取得它，旧快照仍保有许可。
6. 已移除且旧许可为零时完成一次，解锁后唤醒。退出任务等这个终态，不等
   其他 action 的新 dispatch，也不因整条线重新活跃而重新登记。

记录采用短锁或等价已证明原子协议；若使用短锁，顺序固定为 IRQ_STATE →
action 记录，许可归还只拿记录锁，不反向获取 IRQ_STATE。通知在二者锁外。
动作对象及回调内存保留到许可退出，不能在移除表项时先 drop 掉。

整条 IRQ 的现有 synchronize_irq/disable_irq 语义不改为 action 语义；原
line in_flight 继续服务这些操作。新计数是被撤销 action 的快照存续，不是
另一份设备在线状态。若以后可合并优化计数，须独立证明，不作为本次前提。

测试必须构造：快照取得目标 action 后暂停、移除目标、另一 action 持续接受
新 IRQ、释放旧快照后目标释放完成。还要覆盖 prepare 失败、两次释放同 token、
移除最后 action 的平台 mask、未执行 handler 的快照释放和错误路径不重复归还。
这项是 kirq 范围内的前置变更，不把 IRQ 生命周期状态搬进 base。

### 8.4 设计成熟度

本轮将三项宽泛待办具体化为：有实际 DT 证据的配置输入、保留默认后端的库
兼容路线、独立于共享线重置事件的 action 退出算法。它们仍需实现验证，
不能把设计交错推演记录成已经跑过 UT。

下一步应进行整体设计一致性检查，重点确认这些依赖变更没有扩张 base 责任、
没有违反既有功能保留要求；完成后向用户提交一份集中评审结论，而不是继续
为每个实现细节增加文档或索取需求确认。

## 9. Linux 对照后的减法：同步等待不等于 poll 登记

### 已核实的差异

| 实现 | 已核实的行为 | 对本设计的含义 |
|---|---|---|
| Linux kernel/sched/completion.c，do_wait_for_common | 栈上 DECLARE_SWAITQUEUE，在 completion 锁下登记/检查，睡眠前释放锁，返回前完成摘链 | 不需要为所有设备预先准备堆分配的等待登记 |
| task/kpoll/src/source.rs，State::register | SmallVec 槽表扩容可失败，token 递增也可耗尽；wake 摘下整张槽表后锁外调用 waker | 适合现有 poll 契约，不能直接当成可靠同步退出等待 |
| task/ktask/src/future/mod.rs，PreparedTaskWait | prepare 创建 Arc<KWaker>，park 本身不再分配；不负责完成条件的登记 | 预准备一个 parker 不解决整个等待协议 |
| task/ktask/src/irq_wait.rs | 用 PollRegistrations 包装 completion，登记错误向上传递 | 当前 IRQ 同步桥仍继承 poll 登记的失败行为 |
| task/ktask/src/wait_queue.rs | Event listener + block_on，条件检查/登记后复查 | 是现有复用候选，但不能仅凭 WaitQueue 名称承诺无分配 |

进一步核对锁定依赖 event-listener 5.4.1：Event 内部可懒分配 Arc；其源码
根据 std/critical-section 特性选择 intrusive 或 slab 后端，默认关闭这些特性
时使用 Vec 支撑的 slab。仓库 workspace 声明 default-features=false；实际构建
仍须检查 feature 合并结果。ktask::block_on 本身也创建 Arc<KWaker>。
因此不把现有 WaitQueue 包一层就声称实现了 Linux 式无分配等待，也不擅自
修改依赖 feature 或全局 OOM 策略来消除表面上的 Result。

### 推荐承接位置

- driver-os-api 只表达一次性完成条件、任务上下文等待以及允许 IRQ 完成通知的
  契约，不向驱动暴露 PollRegistration、任务 ID 或下游退出计划。
- host adapter 将该契约接到宿主可靠同步等待。等待登记/唤醒的实现归宿主
  同步与任务层，不在 model/class 各写一套，也不让 kirq 反向依赖 driver-os-api。
- 普通 poll 的可失败登记语义不因本次驱动需求而整体改变。若要扩展 kpoll，
  应是明确的同步登记机制，不把所有 poll 用户改成不可恢复分配。
- 完成对象的创建可遵守正常初始化的失败策略；资源已经进入不可逆退出后，
  同步等待不能因临时登记失败而跳过排空或强制释放。

优先验证 Linux 式的调用方持有稳定等待节点：节点在等待期间固定地址，退出前
摘链并与唤醒访问同步，不依赖增长的槽表或全局登记 ID。Rust 需要明确 Pin 与
生命周期约束，不能让可任意迟到的 Waker 持有栈指针。具体实现应复用已存在的
任务阻塞/唤醒握手，不顺手重写调度器。当前只是候选方向，尚未证明完整接口。

### 保留哪些约束、暂停哪些机制

保留：单设备管理串行化、任务上下文、关闭后排空、硬件停止后释放、旧发布不
复活、总线自己负责下游退出顺序。这些不是为了迁就等待实现才出现的约束。

暂停：DriverOps::prepare_remove、公共 RemovePreparation、通用整树准备、
受限子计划执行接口。它们不是当前需求的直接要求，不能为了规避一个宿主
poll 登记错误就默认成为所有驱动必须学习的协议。

下一步先提交第 10 节的前置能力需求，经讨论后再设计底层实现；不自行选择
栈节点算法或修改宿主。如果既有能力足以满足需求，应优先适配而不是新增。
不恢复整树事务设计。没有内核构建、UT 或性能验证结果。

## 10. 前置需求提案：可靠的内核同步完成等待

状态：提交讨论的机制需求，不是已批准的实现票，不指定新增 crate、链表算法
或最终 Rust 接口。此项是驱动设计依赖的宿主能力，不改变既定驱动功能需求。

### 为什么应成为前置项

可复现的源码路径为：

```text
kirq::try_free_irq_inner
  → take_action / begin_teardown
  → wait_for_virq_in_flight
  → ktask::irq_wait::wait_for_completion
  → PollRegistrations / PollSet 登记
  → 登记失败返回 SyncWaitFailed
  → 已移除 action 的 free_irq 路径 panic
```

`arch/kirq/src/runtime/manager.rs` 对该 panic 已有明确 TODO；不能把它当成
不会到达的错误。等待前验证了上下文，也不能排除之后的 NoMemory/IdExhausted。
这说明底层已有实际使用者，不是只为未来 driver-class 假设一套能力。

Linux 的 completion 同步等待使用调用方栈上等待项，不把等待登记分配错误
交给每个驱动 remove 处理。借鉴的是这一能力边界；是否采用同一种内部结构
应由宿主同步层设计决定，不能从参考 Linux 直接跳到自行编写侵入式链表。

### 最小能力与限制

| 编号 | 必需能力 | 不承诺的内容 |
|---|---|---|
| W1 | 在合法可睡眠任务上下文等待完成；不可逆退出后不因临时登记分配或 token 耗尽而失败 | 不把错误上下文变成合法调用，不保证硬件必然完成 |
| W2 | 完成早于等待、登记与完成并发、虚假唤醒均不会丢失终态或提前返回 | wake 不是业务完成，不以“收到一次唤醒”代替条件检查 |
| W3 | IRQ 可发出完成通知；该通知路径不分配、不睡眠，遵守宿主锁与唤醒规则 | 不笼统扩展到 NMI，不保证唤醒数量无关的固定耗时 |
| W4 | 多等待者可安全等待同一完成条件；返回/撤销后通知不会访问失效等待者 | 不要求通用异步任务取消、任意强制终止等待线程 |
| W5 | 创建完成对象和等待登记的失败策略明确分开；完成发布与等待返回建立所需的内存可见性 | 不通过全局 OOM/panic 策略变更掩盖登记失败 |
| W6 | 提供确定性交错与失败注入验证入口，证明关闭后等待路径不依赖新的可失败登记 | 不要求性能独占窗口或虚构吞吐门槛 |

面向 class 的需求是一代一次完成、完成后保持终态；现有 kirq Completion 还有
可重置/令牌语义。前置设计必须明确如何适配现有调用者，不能直接把全部
Completion 改成一次性对象。是否进一步改 action/整线同步属于另一项 IRQ
语义设计，不借此次等待改造默认带入。

### 所属模块与接口责任

- 宿主同步/任务层：提供上述等待能力，复用已有阻塞/唤醒握手。究竟扩展 kpoll、
  ktask 的哪个 mod，由前置设计确认；当前不新增 sync/common 等 crate。
- kirq 的等待桥：接入能力，消除等待登记失败发生在 action 已移除之后的路径；
  不擅自改变共享 IRQ 的 dispatch、mask、synchronize 或 action 生命周期语义。
- driver-os-api：只定义可移植完成/等待契约；host adapter 实现它。
- model/class：消费契约，不拥有等待节点、任务登记表或宿主分配策略。

不在本项范围：重写调度器、替换所有 poll/epoll 用户、迁移整个 WaitQueue、
加入超时强制释放、重写总线/IRQ 框架、为驱动新增 prepare_remove/整树事务。

### 设计与验收的前后顺序

1. 确认该前置需求及责任范围；将现有实现可复用部分与真正缺口列清。
2. 单独设计宿主最小方案，对照 Linux 检查登记、唤醒、摘链和执行上下文；
   类型/生命周期与内存分配策略必须可解释，不以“可靠 wait”空接口代替实现。
3. 审阅通过后单独实现和验证：完成先到/后到/并发、多等待者、虚假唤醒、
   IRQ 通知、等待者退出以及旧 Completion 语义兼容；针对 IRQ 同步回归验证。
4. 接入 os-api，再收敛驱动退出设计。该能力只解除等待登记方面的障碍，不等于
   硬件停机、DMA 生命周期、父子退出次序已经被证明。

可在等待前置项期间继续审阅设备身份、注册/绑定及模块职责；依赖本能力的
退出接口不定稿、不通过驱动侧变通方案绕过。当前未修改任何宿主或驱动实现。

## 11. 当前已识别的前置能力总表

本表列能力与完成条件，不是开发顺序或已批准的实现票。“前置”表示相应功能
实际落地前必须满足，并非这些项必须全部完成后才能讨论 model。仍以一条真实
QEMU MMIO hwrng 路径为范围，未迁移驱动保持现有路径。

| 编号 | 能力与当前缺口 | 所属层 | 框架设计采用的假设 / 实施验收 |
|---|---|---|---|
| P1 | 可靠同步完成等待；当前 poll 登记可失败 | 宿主同步/任务层，os-api 提供契约 | 合法上下文等待不因临时登记失败中断；IRQ 通知、多等待者、无丢唤醒；见第 10 节 |
| P2 | IRQ 同步释放保证；现有 action 移除后等待失败会 panic | kirq 及 IRQ host adapter | 返回成功意味着旧 handler 不再访问所释放资源；不误关其他共享 action；借用 P1，不预设新增 action 计数算法 |
| P3 | 设备 DMA 事实从固件/平台传入描述；现有 FirmwareDevice 缺少 DMA 信息 | 固件/平台发现与转换层 | 约束有可追溯来源，不能表示的配置明确拒绝；不能从缺少 dma-ranges 猜全地址可用 |
| P4 | 新 os-api 的实际宿主实现及安装 | kdriver 等 host adapter | MMIO/IRQ/DMA/等待等实际用到的能力可获取；DMA 实例正确关联设备并配对释放，不直接把 host 类型暴露给驱动 |
| P5 | VirtIO 库支持每设备 DMA 实例 | kvirtiodrivers 库及实例后端适配 | ring、请求、间接表均经过同一实例；旧静态 Hal 入口兼容，多实例地址/释放不串用 |
| P6 | VirtIO 可靠停机及未完成请求回收 | transport/virtqueue/RNG 库接口 | 确认设备停止访问后才取消 outstanding token；不伪造完成，不双重 unshare，停机未确认不释放资源 |
| P7 | 新旧枚举与启动装配 | kdriver 枚举/启动集成 | 撤掉旧 RNG 注册仍可发现；旧 device_desc_add 前唯一分流；安装 provider 早于 probe，其他 VirtIO 类型不回退 |

P1 是通用机制前置；P2/P3 是现有基础能力补齐；P4/P7 是集成义务，不应被包装
成新通用机制；P5/P6 是外部库能力前置。P4 的契约设计可与 model 同时进行，
不能要求在驱动模型接口尚未确定时先完成全部 adapter 实现。

### 不是新增前置机制的事项

- kfinalize 已存在。若 class 收尾仍需要使用它，只需满足其 provider 安装与
  上下文要求，不新增执行器，不默认支持启动早期使用。
- hwrng 的停止条件、读者唤醒、发布撤销属于本轮具体驱动/子系统工作，不另设
  通用“取消管理器”；class 的许可与排空也是 class 本身的职责。
- sysfs/uevent 已有机制，迁移时盘点并保持实际行为，不先重写整套投影或命名。
- 共享 IRQ 的逐 action 计数是旧候选实现，不是 P2 的必选前置；先按 Linux
  同步语义和现有 kirq 设计核对，不能为本 PR 未证明的需求重写 dispatch。
- PCI/I2C、PM、物理热插拔和模块装卸不是本阶段需补齐的功能；参考其责任划分，
  不因此实现这些子系统。

### 使用这些假设时的纪律

框架设计可以直接使用 P1—P7 的成功契约，不再为其尚未实现发明驱动侧绕路。
失败仍按能力本身的规则处理：初始化失败回滚，硬件停机无法证明则不释放、不
宣布成功。可靠等待不保证事件最终发生，IRQ 同步也不证明 DMA 已停止。
若实施时某项前置无法满足，回到该项设计评审，不静默改动上层语义或恢复整树事务。
前置之间的实现依赖与提交顺序统一在后续 tickets 中安排。
