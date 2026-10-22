# 驱动设备模型基础设施

## 定位

`drivers/base` 承载与设备家族、具体总线实现和宿主内核实现无关的
**驱动设备模型基础设施**，不是驱动子系统所有公共代码的集合。

它拥有设备身份与绑定生命周期，以及通用能力发布和退出协调机制。
它通过驱动能力契约获取宿主服务，通过设备子系统表达具体设备能力，
不拥有这些服务的宿主实现或设备家族语义。

它负责回答：一个设备如何进入模型、与驱动绑定、发布能力，以及如何安全退出。
具体设备怎样收发数据、访问硬件，以及宿主内核怎样提供运行能力，由各自的层负责。

本文定义后续演进的职责边界，不表示当前代码已经完成分层。
当前目录是根 workspace 下两个 crate 的容器：

| 目录 / package | Rust crate 名 | 职责 |
|---|---|---|
| `model/` / `driver-model` | `driver_model` | 设备模型、绑定与退出编排 |
| `class/` / `driver-class` | `driver_class` | 通用发布、使用许可和撤销协调 |

依赖方向为 `model → class`；hwrng 与 entropy 直接依赖 class，不依赖模型。
它们与 `drivers/contracts/driver_base` 中的旧 `driver_base` crate 分别演进。
内核 `entry` 仅在 `unittest` feature 下链接 model，保证模型 UT 在迁移期间仍被执行。
资源契约与 platform 总线暂留 model，后续按本文边界迁移。

## base 拥有的职责

| 职责 | 拥有的状态与不变量 |
|---|---|
| 设备与驱动绑定 | 设备身份、拓扑、驱动匹配、probe/defer/retry/remove，以及绑定状态的修改权限 |
| 通用能力发布 | 发布身份、注册与撤销、消费者访问许可、在途操作和延迟 teardown |
| 设备生命周期协调 | 退出后哪些操作必须拒绝，驱动私有数据、发布对象和持有资源如何协调收尾 |

这些机制不应需要知道设备属于 hwrng、block 还是 net，也不应需要知道宿主
使用哪种调度器、IRQ 核心或内存管理器。所需宿主能力通过契约注入。

设备绑定状态由设备模型维护，发布状态由通用 class 机制维护。两者通过明确的
身份与生命周期接口协作；拆分时不得复制设备的在线状态，或让多个模块成为
同一个状态的修改权威。

## 不属于 base 的内容

| 内容 | 归属 |
|---|---|
| hwrng 的读取和随机源失败语义、block 请求语义、net 收发语义 | 对应的设备子系统，如 `drivers/subsystems/hwrng` |
| virtio-rng 队列操作、具体寄存器布局和硬件协议 | 具体设备驱动 |
| platform compatible 匹配、PCI 配置空间访问及枚举 | 相应总线实现 |
| IRQ 注册、DMA 分配、MMIO 映射等宿主实现 | host adapter / integration / provider 实现 |
| 与设备模型无关的通用数据结构和工具 | 仓库已有的工具层 |

“很多驱动都会用”不足以成为放入 base 的理由。通用 `Bus` 契约属于设备模型，
具体 `PlatformBus` 实现则有自己的身份、匹配和枚举语义。

通用 class 机制与具体设备 class 也必须区分：`Class`、`Publication`、使用许可
和撤销协调属于基础设施；hwrng 的设备接口和注册入口属于 hwrng 子系统。
其他设备家族迁移时遵循相同划分。

## 资源契约与 devres 的边界

当前 `devres` 涉及两种不同职责，不能把整个目录直接等同于一个未来的 crate：

- **模型侧协调**：资源请求携带哪个设备身份，获取失败怎样影响 probe，持有资源
  的驱动数据何时释放。这些属于 base。
- **资源能力契约**：资源如何描述、获取、操作和释放，包括 MMIO、IRQ、DMA、
  时钟句柄及 provider 契约。这些属于驱动能力契约层，宿主实现位于适配层。

例如，`Device::iomap()` 可以属于 base，但返回的映射对象和底层映射契约不必
由 base 定义。RAII 释放也不能替代 remove 阶段显式停止硬件和协调在途操作。

仓库已有 `device-res` 能力契约。调整此处资源抽象前，必须明确与它的复用、演进
或替代关系，避免维护两套平行契约。当前对 `kirq` 的直接依赖也不能因为已有代码
或移动目录而视为永久例外，应按驱动子系统边界规则明确契约与宿主适配的归属。

## crate 与 mod 的划分

遵循仓库的[模块与 crate 规则](../../docs/ai/skills/code-guidelines/modules-and-attributes.md)：

- crate 围绕内聚的所有权职责组织：数据、状态、不变量、生命周期和执行上下文
  约束需要共同维护，且形成可供多个同级 crate 使用的完整能力。
- mod 保留单个 crate 的实现细节，不因文件大、代码多或存在 trait 就单独建 crate。
- 不为拆分而公开内部状态、注册表细节或桥接辅助函数；跨 crate 暴露的是自然的
  领域对象与稳定能力。
- 不建立仅用于收纳零散类型、错误或工具的 `common`、`types` 等兜底 crate。

按这个标准，设备模型与通用 class 是 base 内部的独立职责候选；
`domain` 的 `registry`、`probe`、`dependency`、`lifecycle` 共同维护
`DomainInner`、锁及 generation 协议，应继续作为模型内部的 mod。
资源契约需要先完成职责梳理，具体总线实现应与模型核心分开。

`base/` 可以演进为包含多个职责完整 crate 的目录容器，不必保留一个重新导出
所有内容的总入口 crate。子 crate 仍加入根 workspace，各自维护 `ut-tests/`
和设计、安全文档，不新建嵌套 workspace。具体数量、名字和目录布局由职责与
依赖设计决定，本文不预先固定。

class 定义 `PublicationOwner` 契约，model 的私有 `DeviceCore` 实现它；
`Device::publish` 向 class 传递同一个身份的 `Arc`。模型仍持有唯一在线状态，
class 通过模型实现的 `mark_offline` 回调完成原有共享退出协议。
没有复制状态、公开 `DeviceCore` 或增加反向依赖。
遗留设备的合成身份由 class 内部过渡适配器拥有，不依赖 model。

## 演进与归属判断

hwrng 是第一条验证新模型的迁移链路，并非 base 唯一的目标消费者。
划分应面向其他设备家族继续迁移后的稳定职责，再用当前 hwrng 的完整链路验证
接口是否成立。无需等多个家族迁移完才确立边界，也不为假设中的扩展逐个拆出
trait crate。

新增或迁移代码时，先回答：

1. 它拥有哪个状态、生命周期或不变量？谁有权修改？
2. 它是否必须知道具体设备家族、总线实现或宿主内核？若必须，哪一层应拥有它？
3. 它是模型自身的决定，还是模型通过契约向外请求的能力？
4. 独立 crate 后是否形成完整能力，还是迫使调用者了解更多内部状态和调用顺序？
5. 是否已有契约承担这项职责？迁移后如何避免状态和接口重复？

先明确所有权和依赖方向，再确定 crate 数量及名称。结构重构与行为修改分开提交。

## 相关文档

- [驱动子系统边界规则](../../docs/ai/driver-subsystem-boundaries.md)
- [驱动目录分层](../README.md)
- [设备模型设计](model/docs/design.md)
- [通用 class 设计](class/docs/design.md)
- [设备模型安全分析](model/docs/security.md)
- [通用 class 安全分析](class/docs/security.md)
