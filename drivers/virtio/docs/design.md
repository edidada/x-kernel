# VirtIO — 设计文档

## 定位

本模块是 x-kernel 的 VirtIO 驱动适配层，将 [`virtio-drivers`] crate 提供的各类
VirtIO 设备封装为 [`driver_base`] 系列 trait 的实现。块设备、网络、显示、输入、
vsock、9p 等子系统通过本模块访问 VirtIO 虚拟设备。

## 背景

x-kernel 运行在裸机 `no_std` 环境中，需要通过 VirtIO 协议与虚拟机监控器（VMM）
通信来使用虚拟设备。`virtio-drivers` crate 提供了 VirtIO 协议的基础实现（队列管理、
配置空间访问等），但其接口与 x-kernel 的驱动框架（`driver_base` 系列 trait）不一致，
因此需要本适配层进行桥接，使上层子系统无需关心 VirtIO 协议细节。

## 范围

涉及的源文件：

```
drivers/virtio/
├── src/
│   ├── lib.rs              # 设备探测、错误转换、公共 re-export
│   ├── blk.rs              # VirtIO Block 适配器（IRQ 驱动 + 轮询回退）
│   ├── gpu.rs              # VirtIO GPU 适配器
│   ├── input.rs            # VirtIO Input 适配器
│   ├── irq_completion.rs   # hardirq → softirq → bottom half 的共享完成桥接
│   ├── net.rs              # VirtIO Net 适配器（IRQ + NAPI 式中断抑制）
│   ├── rng.rs              # VirtIO RNG 适配器（IRQ 驱动 + 轮询回退）
│   ├── socket.rs           # VirtIO Vsock 适配器
│   ├── virtio_9p.rs        # VirtIO 9p 适配器
│   └── mock_virtio.rs      # 单元测试 Mock（Hal + Transport）
└── Cargo.toml
```

Feature flags 控制编译哪些设备适配器：

| Feature   | 启用的设备 | 额外依赖 |
|-----------|-----------|----------|
| `block`   | blk       | `block` crate |
| `gpu`     | gpu       | `alloc`, `display` crate |
| `input`   | input     | `alloc`, `input` crate |
| `net`     | net       | `alloc`, `net` crate |
| `rng`     | rng       | `alloc`, `char_driver` crate |
| `socket`  | socket    | `alloc`, `vsock` crate |
| `virtio-9p` | virtio_9p | `alloc` |

## 架构

```
┌──────────────────────────────────────────────────────────────────────┐
│                             上层子系统                                │
│      (块设备层 / 网络栈 / 显示框架 / 输入子系统 / vsock / 9p)             │
└───────┬──────────┬──────────┬──────────┬──────────┬──────────┬──────—┘
        │          │          │          │          │          │
      Block      Display    Input       Net       Vsock      9p API
      Device     Device     Device      Device    Device     Device
        │          │          │          │          │          │
┌───────┴──────────┴──────────┴──────────┴──────────┴──────────┴───────┐
│                          virtio (本模块)                              │
│                                                                      │
│  ┌───────┐  ┌───────┐  ┌───────┐  ┌───────┐  ┌───────┐  ┌───────┐    │
│  │  Blk  │  │  Gpu  │  │ Input │  │  Net  │  │ Sock  │  │  9p   │    │
│  │  Dev  │  │  Dev  │  │  Dev  │  │  Dev  │  │  Dev  │  │  Dev  │    │
│  └───┬───┘  └───┬───┘  └───┬───┘  └───┬───┘  └───┬───┘  └───┬───┘    │
│      │          │          │          │          │          │        │
│  ┌───┴──────────┴──────────┴──────────┴──────────┴──────────┴───────┐│
│  │                   lib.rs (probe / error / re-export)             ││
│  └───────────────────────────────┬──────────────────────────────────┘│
└──────────────────────────────────┼─────────────────────────────────——┘
                                   │
                          virtio-drivers crate
                                   │
                          VirtIO Transport (MMIO / PCI)
```

| 组件 | 职责 |
|------|------|
| `lib.rs` | 设备探测（`probe_mmio_device` / `probe_pci_device`）、VirtIO 错误到 `DriverError` 的转换、公共类型 re-export |
| `blk.rs` | 将 `VirtIOBlk` 封装为实现 `BlockDevice` 的 `VirtIoBlkDev`；IRQ 可用时经 Block softirq push 式逐请求完成（hardirq ack → softirq 按 token 派发 → 请求者弹出并级联派发），多请求可在途，否则回退同步轮询 |
| `gpu.rs` | 将 `VirtIOGpu` 封装为实现 `DisplayDevice` 的 `VirtIoGpuDev` |
| `input.rs` | 将 `VirtIOInput` 封装为实现 `InputDevice` 的 `VirtIoInputDev` |
| `irq_completion.rs` | blk/rng 共享的 IRQ 完成桥接：hardirq 仅 ack 并标记 pending + raise softirq；softirq action 逐设备执行 bottom half——blk 按 used ring 头 token 派发完成，rng 广播单请求唤醒 |
| `net.rs` | 将 `VirtIONetRaw` 封装为实现 `NetDevice` 的 `VirtIoNetDev`，管理收发缓冲区、IRQ ack、NAPI 式中断抑制和网络 RX scheduler 调度 |
| `rng.rs` | 将 `VirtIORng` 封装为实现 `CharDevice` 的 `VirtIoRngDev`；IRQ 可用时经 Hrtimer softirq 向量广播唤醒有界睡眠等待（该向量无其他生产占用者；`Tasklet` 被 bottom-half workerqueue 占用），否则回退关中断有界自旋 |
| `socket.rs` | 将 `VsockConnectionManager` 封装为实现 `VsockDevice` 的 `VirtIoSocketDev` |
| `virtio_9p.rs` | 将 `VirtIO9p` 封装为实现 `Virtio9pDevice` 的 `VirtIo9pDev`，提供 `mount_tag()` 和 `request()` |
| `mock_virtio.rs` | 提供 `MockHal` 和 `MockTransport`，用于单元测试 |

## 调用约束 / 执行上下文

`virtio` 位于平台传输层与上层子系统之间，
调用者需要满足以下执行约束：

- **依赖平台已完成设备枚举与映射**：
  `probe_mmio_device` 要求 MMIO 寄存器区已经映射且地址有效；
  `probe_pci_device` 要求 PCI 配置空间、BAR 和 IRQ 路由可访问。
- **初始化路径依赖早期启动环境**：
  设备探测、feature 协商、virtqueue 分配和 IRQ 注册
  通常发生在启动期或驱动注册期，
  需要底层内存分配、DMA 支持和中断设施已经可用。
- **普通设备操作不应在错误的中断语境中执行**：
  块设备同步 I/O、9p 请求、GPU flush、
  网络缓冲区回收等路径可能持有锁或等待设备完成，
  不应假设适用于任意不可睡眠上下文。
- **blk/rng 的 IRQ 驱动等待路径要求任务上下文**：
  请求任务在提交后在本请求的完成槽上睡眠等待 bottom half 唤醒
  （blk 按 token push；rng 广播+重查）；
  无可睡眠上下文（早期启动或 IRQ 上下文调用）时自动退化为
  开中断轮询，hardirq handler 持续 ack 设备 ISR，
  共享电平触发中断线不会 livelock。
- **网络设备的 IRQ 与正常路径并发访问
  依赖 `SpinNoIrq`**：
  调用者不应绕过现有封装直接并发访问 `InnerDev`。
- **网络 IRQ handler 只做设备级快速处理**：
  virtio-net IRQ handler 按 resolved IRQ 过滤对应设备 handle，
  ack 设备中断后仅进行 NAPI 式中断抑制并调用上层通过
  `driver_net::NetRxScheduler` attach 的 RX 调度能力，
  不直接运行协议栈或 socket 唤醒逻辑。
- **virtio-net 中断抑制与 Linux 一致、不触碰 IRQ 线**：
  queue 级 NO_INTERRUPT 标志覆盖未协商 `VIRTIO_F_RING_EVENT_IDX` 的设备；
  协商了 EVENT_IDX 的设备由弹出推进 `used_event` 的规范机制隐式抑制。
- **设备对象的共享安全依赖 trait 约束和内部封装**：
  非 net 设备主要通过 `&mut self` 独占访问；
  net 设备依赖内部锁和 token-buffer 对应关系维持正确性。
- **上层必须尊重 handle 生命周期**：
  例如 net 设备返回的 TX/RX handle
  必须按约定回收，不能重复消费或跨设备混用。

## 状态机

### 设备生命周期

```
Uninitialized ──probe──> Probed ──try_new──> Ready ──ops──> Active
                              │                  │
                              │    try_new 失败   │  设备错误
                              └─────Error────────> Failed
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| Uninitialized | Probed | `probe_mmio_device` / `probe_pci_device` 识别到 VirtIO 设备 |
| Probed | Ready | `try_new()` 成功初始化设备（协商 feature、分配队列） |
| Probed | Failed | `try_new()` 失败（设备不支持、DMA 分配失败等） |
| Ready | Active | 首次调用驱动操作（如 `read_block`、`send` 等） |
| Active | Failed | 设备返回不可恢复错误 |

### 块设备请求状态

```
Idle ──read_block/write_block──> Pending ──设备响应──> Complete ──返回──> Idle
  │                                                              │
  └────────────────────── flush ──设备响应──> Complete ───────────┘
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| Idle | Pending | 调用 `read_block()`、`write_block()` 或 `flush()`，请求提交到 VirtIO 队列 |
| Pending | Complete | IRQ 模式：bottom half 标记 used ring 头 token 的完成槽并唤醒对应请求者，请求者弹出该项并级联派发下一个 token；轮询模式：`virtio-drivers` 内部轮询获取结果 |
| Complete | Idle | 返回 `DriverResult`，设备回到空闲状态 |

> 块设备为同步接口：每次请求等待完成后才返回。IRQ 模式下等待为睡眠等待
> （bottom half 按 token 唤醒本请求），多个请求可同时在途、按设备完成顺序
> 逐个派发；轮询模式下等待为关中断自旋。

### GPU 设备 scanout resource 状态

```
Uninit ──probe/query resolution──> Ready
Ready ──create_scanout_resource──> ResourceAttached
ResourceAttached ──present_scanout_resource──> ScanningOut
ScanningOut ──present_scanout_resource(new resource)──> ScanningOut
ResourceAttached / ScanningOut ──destroy_scanout_resource──> Ready
Ready / ResourceAttached / ScanningOut ──设备错误──> Failed
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| Uninit | Ready | `try_new()` 完成 feature 协商、virtqueue 初始化并查询分辨率 |
| Ready | ResourceAttached | DRM dumb buffer backing 被创建并 attach 到 virtio-gpu 2D resource |
| ResourceAttached | ScanningOut | DRM modeset/page-flip 触发 transfer-to-host、set-scanout 和 flush |
| ScanningOut | ScanningOut | page flip 切换到另一个已 attach 的 resource |
| ResourceAttached / ScanningOut | Ready | dumb buffer 销毁，resource detach/unref |
| Ready / ResourceAttached / ScanningOut | Failed | 设备返回不可恢复错误 |

> GPU 驱动不再在 `try_new()` 中创建固定 framebuffer。像素内存由 DRM dumb buffer
> 分配并 mmap 给用户态，virtio-gpu resource 直接 attach 这段 backing。page flip
> 只提交 transfer-to-host、set-scanout 和 resource-flush 命令，不再做 CPU framebuffer copy。

### Input 设备事件队列状态

```
Empty ──设备产生事件──> HasEvent ──read_event──> Empty
                          │                        │
                          │  read_event (无事件)    │  返回 WouldBlock
                          └──> WouldBlock ──────────┘
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| Empty | HasEvent | 设备产生输入事件，VirtIO 队列中存在待读取事件 |
| HasEvent | Empty | `read_event()` 成功弹出事件 |
| Empty | WouldBlock | `read_event()` 调用时队列无事件，返回 `DriverError::WouldBlock` |
| WouldBlock | HasEvent | 设备产生新事件 |

> 输入设备为轮询模型：`read_event()` 非阻塞地从 VirtIO 队列弹出事件，
> 无事件时返回 `WouldBlock`，由上层决定轮询策略。

### 网络设备缓冲区状态

```
Free ──alloc_tx_buf──> Allocated ──send──> InFlight ──recycle_tx──> Free
  │                                                               │
  └───────────────────── recv + recycle_rx ────────────────────────┘
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| Free | Allocated | `alloc_tx_buf()` 从 `free_tx_bufs` 取出缓冲区 |
| Allocated | InFlight | `send()` 将缓冲区提交到 VirtIO 队列 |
| InFlight | Free | `recycle_tx()` 回收已发送完成的缓冲区 |
| Free (rx) | InFlight (rx) | `try_new()` 中 `receive_begin()` 将 rx 缓冲区提交到队列 |
| InFlight (rx) | Free (rx) | `recv()` + `recycle_rx()` 取回并重新提交 rx 缓冲区 |

### Vsock 连接状态

```
Closed ──listen──> Listening ──connect──> Connecting ──Connected事件──> Established
  │                    │                                          │
  │                    │    ConnectionRequest                      │  recv / send
  │                    └──> Accept ──accept──> Established          │
  │                                                               │
  │    disconnect / abort                                         │  Disconnected事件
  └───────────────────────────────────────────────────────────────> Closed
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| Closed | Listening | `listen(src_port)` 开始监听端口 |
| Listening | Established | 收到 `ConnectionRequest` 事件，上层调用 `connect()` 接受 |
| Closed | Connecting | `connect(cid)` 主动发起连接 |
| Connecting | Established | 收到 `Connected` 事件 |
| Established | Established | `send()` / `recv()` 数据传输；`recv()` 后自动 `update_credit()` |
| Established | Closed | 收到 `Disconnected` 事件，或调用 `disconnect()` / `abort()` |
| Any | Closed | `abort()` 强制关闭（`force_close`），不等待对端确认 |

> Vsock 连接由 `VsockConnectionManager` 内部管理，本模块通过 `poll_event()` 获取
> 事件并翻译为上层 `VsockDriverEventType`。`recv()` 后自动调用 `update_credit()`
> 通知对端可用缓冲区大小。

### 9p 设备请求状态

```
Idle ──request──> Waiting ──设备响应──> ResponseReady ──返回──> Idle
                       │
                       │  设备错误
                       └──> Failed
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| Idle | Waiting | 调用 `request()`，将 9p 请求提交到 VirtIO 队列 |
| Waiting | ResponseReady | 设备返回响应数据 |
| ResponseReady | Idle | `request()` 将响应拷贝到 `resp` 缓冲区并返回写入字节数 |
| Waiting | Failed | 设备返回错误或响应缓冲区不足 |

> 9p 设备为同步请求/响应模型：`request()` 阻塞等待设备响应，无超时机制。
> `mount_tag()` 可随时调用，不受请求状态影响。

## 算法流程

### PCI 设备探测

1. 读取 PCI 配置空间，获取设备类型信息
2. 通过 `virtio_device_type()` 判断是否为 VirtIO 设备，再通过 `as_device_kind()` 映射为 `DeviceKind`
3. x86_64 架构：对 virtio-net 与 virtio-blk 尝试 MSI-X 设置（两者的中断驱动
   完成路径已接入；其余设备暂用 legacy），失败回退 legacy IRQ
   - 查找 MSI-X capability，分配 CPU 中断向量，配置 MSI-X 表项
   - 若 MSI-X 不可用或向量耗尽，读取 Interrupt Line 寄存器获取 legacy IRQ
   - 通过 `kirq::try_map()` 将硬件 IRQ 映射为虚拟 IRQ
4. 其他架构：通过设备树或固定映射获取 IRQ
   - aarch64：`pci::legacy_interrupt_route()` 获取设备树路由，按触发类型创建 GIC 描述符
   - riscv64：映射为 PLIC 中断
   - loongarch64：直接使用硬件 IRQ 号
5. 创建 `PciTransport`，返回 `(DeviceKind, PciTransport, irq)`

### MMIO 设备探测

1. 将内存基地址转换为 `VirtIOHeader` 指针（`NonNull::new` 确保非空）
2. 创建 `MmioTransport`，验证魔数（`0x74726976`）和版本（仅支持版本 2）
3. 映射设备类型为 `DeviceKind`（通过 `as_device_kind()`）
4. 返回 `(DeviceKind, MmioTransport)`

### 块设备读写

**初始化流程**：

1. `try_new(transport, irq)` → `InnerDev::new(transport)`：协商 feature、分配 VirtIO 队列
2. 记录 `SECTOR_SIZE`（512 字节）作为 `block_size`
3. 若提供 `irq` 且注册成功：设备经 `BLK_COMPLETION_HUB` 注册到 Block softirq 向量；
   注册失败时回退轮询模式

**读取流程（IRQ 模式，push 式完成）**：

1. 调用 `read_block(block_id, buf)`，经 `read_blocks_nb()` 非阻塞提交读请求，
   按 token 注册完成槽；提交与注册之间到达的完成由注册后的派发补收
2. 在本请求完成槽上睡眠；hardirq ack ISR 后标记 pending 并 raise
   `SoftirqVec::Block`，softirq action 标记 used ring 头 token 的完成槽并唤醒
   该请求的等待者（一次中断覆盖多个完成时，其余完成由被唤醒请求者弹出后
   的级联派发逐个标记）
3. 被唤醒的请求者在设备锁内 `complete_read_blocks()` 弹出本请求（同时注销
   完成槽，防止 token 在注销前被复用）、释放锁后再派发下一个头 token；
   无可睡眠上下文时退化为开中断轮询（handler 持续 ack，无 livelock）
4. flush（设备支持 `VIRTIO_BLK_F_FLUSH` 时）经 `flush_nb()` 非阻塞提交，
   与读写完全相同的 token 完成路径，无独占屏障；不支持该特性的设备
   `flush_nb` 返回 `None`，直接返回成功

**读取流程（轮询回退）**：

1. 调用 `read_block(block_id, buf)`，内部调用 `read_blocks(sector, out_buf)`
2. `virtio-drivers` 在 `SpinNoIrq` 下自旋等待设备响应

**写入流程**：与读取对称（`write_blocks_nb` / `complete_write_blocks`）。

**刷新流程**：

1. 调用 `flush()`，将 flush 请求提交到 VirtIO 队列，同步等待设备确认
2. `virtio-drivers` 未提供非阻塞 flush，刷新频率低，保持同步轮询路径

### GPU 设备 scanout resource

**初始化流程**：

1. `try_new(transport)`：协商 feature、分配 control/cursor virtqueue
2. `GET_DISPLAY_INFO`：查询显示分辨率 `(width, height)`
3. 将 `width`、`height` 封装为 `DisplayInfo { width, height }`。
   virtio-gpu 是 scanout-only 设备，不暴露直接 framebuffer 映射——`DisplayDevice`
   trait 不再包含 `fb()` / 直接映射字段，所有像素输出经 scanout resource 路径
   （由 `fbdevice` 的 fbdev emulation 和 `drmdevice` 共享使用）。

**resource-backed 显示流程**：

1. DRM `CREATE_DUMB` 分配连续 guest pages，并调用 `create_scanout_resource()`
2. virtio-gpu 发送 `RESOURCE_CREATE_2D` 和 `RESOURCE_ATTACH_BACKING`
3. 用户态 mmap dumb buffer 并写入像素
4. DRM modeset/page-flip 调用 `present_scanout_resource()`
5. virtio-gpu 发送 `TRANSFER_TO_HOST_2D`、`SET_SCANOUT` 和 `RESOURCE_FLUSH`
6. DRM `DESTROY_DUMB` 调用 `destroy_scanout_resource()`，发送 detach/unref

> resource ID 由 DRM 层分配并随 dumb buffer 生命周期释放；virtio-gpu 驱动负责把这些
> resource 命令串行提交到 control virtqueue。

### RNG 熵请求

**初始化流程**：

1. `try_new(transport, irq)` → `RngInner::try_new()`：协商 feature、分配 VirtIO 队列
2. 若提供 `irq` 且注册成功：设备经 `RNG_COMPLETION_HUB` 注册到 Hrtimer softirq 向量
   （`Tasklet`/`High` 已被启动期安装的 bottom-half workerqueue 占用，`Hrtimer` 在本内核
   无生产占用者——hrtimer 走 ktask oneshot 定时器——故 rng 独占该槽位）；
   注册失败时回退轮询模式

**熵请求流程（IRQ 模式）**：

1. 先回收上一次超时遗留的请求（若已完成）；否则返回 `WouldBlock`（单请求槽）
2. `submit_entropy()` 提交 DMA 缓冲并记录为唯一在途请求，释放设备锁
3. 有界睡眠（广播唤醒 + 谓词重查，`timeout_at` 包裹 50ms 上限）；hardirq ack 后
   Hrtimer 向量的 softirq 广播唤醒等待任务；无可睡眠上下文时退化为开中断有界自旋
4. 每次唤醒后重查 used ring：完成则拷贝数据返回；超时则在途请求留在 `pending`，
   返回 `WouldBlock`，下次读取时回收。唤醒不携带 token，晚到的完成不会让
   下一次读取提前返回

**熵请求流程（轮询回退）**：

1. 与 IRQ 模式相同的单请求槽状态机，但在 `SpinNoIrq` 下有界自旋（10ms 上限）
   等待完成，超时后同样返回 `WouldBlock`

### 输入设备事件读取

**初始化流程**：

1. `try_new(transport)` → `InnerDev::new(transport)`：协商 feature、分配 VirtIO 队列
2. `name()`：读取设备名称字符串（失败则使用 `"<unknown>"`）
3. `ids()`：读取设备 ID（bustype / vendor / product / version），封装为 `InputDeviceId`

**事件读取流程**：

1. 调用 `read_event()` → `inner.pop_pending_event()`
2. 若队列有待读取事件：返回 `Event { event_type, code, value }`
3. 若队列为空：返回 `DriverError::WouldBlock`，由上层决定轮询策略

**能力查询流程**：

1. 调用 `get_event_bits(ty, out)` → `inner.query_config_select(EvBits, ty, out)`
2. 设备将事件位图写入 `out` 缓冲区
3. 返回 `true`（有数据）或 `false`（无数据）

### 网络设备收发

**初始化流程**：

1. `try_new(transport, irq)` → `InnerDev::new(transport)`：协商 feature、分配 VirtIO 队列
2. 创建 `NetBufPool`（容量 `2 * QS`，缓冲区长度 1526 字节）
3. 填充所有 rx 缓冲区：对每个 `rx_buffers[i]`，调用 `receive_begin()` 提交到接收队列，验证 `token == i`
4. 预分配所有 tx 缓冲区：对每个缓冲区，调用 `fill_buffer_header()` 填充帧头，存入 `free_tx_bufs`
5. 若提供 `irq`，调用 `register_virtio_net_irq()` 注册中断回调

**发送流程**：

1. 调用 `alloc_tx_buf(size)` 从 `free_tx_bufs` 栈中弹出缓冲区，设置 `payload_len`
2. 上层填充数据后调用 `send()` → `transmit_begin(frame)` 提交到 VirtIO 发送队列
3. 将 `NetBuf` 存入 `tx_buffers[token]`，确保缓冲区在设备使用期间存活
4. 中断到来时调用 `recycle_tx()`：循环调用 `poll_transmit()` 获取已完成令牌，`transmit_complete()` 释放缓冲区，回收至 `free_tx_bufs`

**接收流程**：

1. IRQ 模式由 handler 调用 `ack_interrupt()` 确认设备事件并调度 RX，任务上下文调用 `recv()` 后直接通过 `poll_receive()` 获取令牌；纯轮询模式没有 IRQ handler，由 `recv()` 调用 `ack_interrupt()` 后查询接收队列
2. 从 `rx_buffers[token]` 取出缓冲区，调用 `receive_complete()` 读取帧头和包长度
3. 设置 `hdr_len` 和 `payload_len`，返回 `NetBufHandle`
4. 上层处理完毕后调用 `recycle_rx()`：`receive_begin()` 重新提交缓冲区，存回 `rx_buffers[new_token]`

**IRQ 处理流程（NAPI 式）**：

1. `register_virtio_net_irq(irq, inner)`：将 `VirtIoNetIrqHandle` 存入全局 `NET_IRQ_HANDLES`，
   同时创建与设备共享的 `NetNapiState`（记录 `IrqResource` 与线屏蔽状态）
2. 若该 IRQ 首次注册（`REGISTERED_NET_IRQS` 去重），调用 `Irq::request(resource, Arc::new(handle_virtio_net_irq))` 注册中断，guard 存入 `NET_IRQ_GUARDS`
3. 中断到来时 `handle_virtio_net_irq()` 遍历所有句柄，调用 `ack_interrupt()` 确认中断：
   - 确认后执行 queue 级中断抑制（`disable_interrupts()`，即 avail 环
     NO_INTERRUPT 标志；对协商了 EVENT_IDX 的设备该写被设备忽略，
     抑制由 EVENT_IDX 机制隐式承载，见下条）
   - 调用 `schedule_rx()` 经 NetRx softirq 唤醒上层 drain 任务
   - 返回包含来源位图的 `IrqEvent`；没有句柄确认中断时返回 `IrqEvent::NOT_HANDLED`
4. 协商了 EVENT_IDX 的设备在积压期间天然抑制通知：每次 `receive_complete`
   弹出都会把 `used_event` 推进到驱动已消费位置，符合规范的设备只在追平
   该索引时才通知——drain 滞后于设备时后续完成不再产生中断，drain 排空后
   下一个报文自然触发通知。这与 Linux 的 NAPI 一致：完全不触碰 IRQ 线
5. drain 任务将接收队列排空后，`recv()` 在空队列分支恢复 queue 级通知
   （`enable_interrupts()`，EVENT_IDX 设备由最后一次弹出自然 re-arm）
   并重查一次接收队列：
   与恢复竞争到达的报文要么由重查返回，要么由重新开启的中断再次调度 drain，
   不丢失唤醒；排空期间不再产生逐包硬中断
5. 已注册 IRQ 时，`ack_interrupt()` 仅由 IRQ handler 调用，避免 RX 任务清除运行期间
   新到达但尚未调度的设备事件；未注册 IRQ 时，由 `recv()` 在纯轮询路径中确认中断

### Vsock 连接管理

**初始化流程**：

1. `try_new(transport)` → `VirtIOSocket::new(transport)`：协商 feature、分配 VirtIO 队列
2. `InnerDev::new_with_capacity(socket, 32KB)`：创建连接管理器，指定内部缓冲区大小

**连接建立流程**：

1. 服务端：`listen(src_port)` 开始监听 → `poll_event()` 收到 `ConnectionRequest` → `connect()` 接受连接
2. 客户端：`connect(cid)` 主动发起 → `poll_event()` 收到 `Connected` 事件 → 连接建立

**数据传输流程**：

1. 发送：`send(cid, buf)` → 翻译 `VsockConnId` 为 `VsockAddr` → `inner.send(peer_addr, host_port, buf)`
2. 接收：`recv(cid, buf)` → `inner.recv(peer_addr, host_port, buf)` → 自动调用 `update_credit()` 通知对端可用缓冲区

**事件轮询流程**：

1. 调用 `poll_event()` → `inner.poll()` 获取 `VsockEvent`
2. `translate_event()` 将 `VsockEvent` 翻译为上层 `VsockDriverEventType`：
   - `ConnectionRequest` → `VsockDriverEventType::ConnectionRequest`
   - `Connected` → `VsockDriverEventType::Connected`
   - `Received { length }` → `VsockDriverEventType::Received`
   - `Disconnected` → `VsockDriverEventType::Disconnected`
   - `CreditUpdate` → `VsockDriverEventType::CreditUpdate`

**连接关闭流程**：

1. 优雅关闭：`disconnect(cid)` → `inner.shutdown()` 等待对端确认
2. 强制关闭：`abort(cid)` → `inner.force_close()` 不等待对端确认

### 9p 设备请求

**初始化流程**：

1. `try_new(transport)` → `InnerDev::new(transport)`：协商 feature、分配 VirtIO 队列
2. 设备就绪后即可通过 `mount_tag()` 获取挂载标签

**请求/响应流程**：

1. 调用 `request(req, resp)` → `inner.request(req, resp)`
2. 将 9p 请求字节写入 VirtIO 队列
3. 阻塞等待设备响应
4. 响应数据写入 `resp` 缓冲区，返回写入字节数

> 9p 设备为同步请求/响应模型，`request()` 阻塞等待设备响应，无超时机制。
> `mount_tag()` 可随时调用，不受请求状态影响。

## 并发模型

- **blk（IRQ 模式）/ rng**：`InnerDev` 被 `Arc<SpinNoIrq<...>>` 包裹，因为 hardirq
  handler 在中断上下文访问设备（`ack_interrupt`）。请求任务在提交与完成两个短
 临界区之间睡眠，不跨睡眠持锁。blk/rng 共享 `irq_completion.rs` 中的 hub：
  `devices`/`registered_irqs`/`guards` 均由 `SpinNoIrq` 保护，锁序为 hub → 设备锁，
  不会反向嵌套。
- **blk（轮询回退）/ gpu / input / socket / virtio_9p**：所有操作通过 `&mut self`
  独占访问或经 `SpinNoIrq` 串行化，调用者负责上层串行化。
- **net**：`InnerDev` 被 `Arc<SpinNoIrq<...>>` 包裹，因为 IRQ 回调在中断上下文中
  访问设备（`ack_interrupt`），与正常收发路径并发。全局静态变量
  `NET_IRQ_HANDLES`、`NET_IRQ_GUARDS` 和 `REGISTERED_NET_IRQS` 也使用 `SpinNoIrq` 保护。
  NAPI 抑制不触碰 IRQ 线（与 Linux 一致）：未协商 EVENT_IDX 的设备用
  avail 环 NO_INTERRUPT 标志，协商了 EVENT_IDX 的设备由弹出推进
  `used_event` 的机制隐式抑制，不存在跨 CPU mask/unmask 顺序问题，
  也不影响共享同一中断线的其他设备。
- **IRQ 注册**：`register_virtio_net_irq()` 在 `NET_IRQ_HANDLES` 中注册回调句柄，
  同一 IRQ 仅注册一次（通过 `REGISTERED_NET_IRQS` 去重）。`Irq::request()` 返回的
  guard 存入 `NET_IRQ_GUARDS`，设备 Drop 时自动注销。blk/rng 的 hub 采用同样的
  按线去重与 guard 保留策略。

## 设计决策

### 为什么用适配器模式而非直接实现

`virtio-drivers` crate 的设备类型（如 `VirtIOBlk<H, T>`）是泛型结构体，
直接为其实现 `driver_base` 的 trait 会违反孤儿规则（orphan rule）。
因此采用 newtype 适配器模式，在每个子模块中定义自己的包装类型。

### 为什么手动实现 Send/Sync

`virtio-drivers` 的设备类型内部包含 `PhantomData` 等标记，编译器无法自动推导
`Send`/`Sync`。但 VirtIO 设备在 x-kernel 中确实可以跨线程传递和共享引用
（通过锁保护），因此手动 `unsafe impl Send/Sync`。

### 为什么 net 的 IRQ 处理使用全局静态变量

中断处理函数 `handle_virtio_net_irq` 作为返回 `IrqEvent` 的回调注册，但仍需访问
所有网络设备实例。使用全局 `NET_IRQ_HANDLES` 存储所有网络设备的 IRQ 回调句柄，
中断到来时遍历所有句柄执行 `ack_interrupt()`。
`NET_IRQ_GUARDS` 持有 `Irq` guard，确保设备 Drop 时自动注销中断。

### 为什么用 softirq 桥接而不是 hardirq 直接唤醒

blk/rng 的完成唤醒经 `kirq::softirq`（Block / Hrtimer 向量；`Hrtimer` 槽位是为 rng 保留的空闲向量）而非在 hardirq 里
直接操作等待者：

1. hardirq 只保留最短路径（ack ISR、标记 pending、raise softirq），
   中断尾/`ksoftirqd` 的下半部负责所有与等待者的交互；
2. 唤醒原语选用 `kpoll::PollSet`（`wake` 侧显式支持中断上下文，且在
   IRQ 尾 softirq 上下文无 `event-listener` 类自旋锁的同 CPU 重入风险）；
   blk 的完成派发与 Linux 一致为 push 语义——bottom half 标记 used ring
   头 token 对应的完成槽并唤醒该请求（等价 `virtqueue_get_buf` 循环 +
   per-request endio），唤醒粒度是请求而非设备，与中断次数天然解耦；
   被唤醒请求者弹出后级联派发下一个 token，因此一次中断覆盖多个完成时
   无需额外的唤醒计数；
3. 睡眠侧复用 `kirq::IrqSyncWaitIf` 桥接（由 ktask 提供），无可睡眠上下文时
   返回错误，驱动据此退化为开中断轮询，避免引入驱动层对调度器的直接依赖。

### 为什么 net 的中断抑制不用 IRQ 线屏蔽

Linux 的 NAPI 从不屏蔽 IRQ 线，本驱动保持一致：

1. 未协商 `VIRTIO_F_RING_EVENT_IDX` 的设备：queue 级 `disable_interrupts()`
   设置 avail 环 NO_INTERRUPT 标志，drain 结束 `enable_interrupts()` 恢复；
2. 协商了 EVENT_IDX 的设备：`set_dev_notify` 虽是空操作（fork 限制），但
   每次 `receive_complete` 弹出会把 `used_event` 推进到驱动已消费位置，
   符合 virtio 规范（vring_need_event 窗口语义）的设备只在追平该索引时
   通知——积压期间隐式抑制、排空后自然 re-arm，效果等价于 Linux 的
   `virtqueue_disable_cb`/`enable_cb` 事件机制；
3. 线屏蔽会引入跨 CPU mask/unmask 顺序倒置风险（迟到的 mask 把线留在
   屏蔽态且标志显示未屏蔽），并延迟共享同一 INTx 线的其他设备，故不采用。

## Drop / 资源释放

`VirtIoNetDev` 实现了自定义 `Drop`：在设备销毁时调用 `unregister_virtio_net_irq()`
注销 IRQ 回调，释放 `NET_IRQ_GUARDS` 中对应的 guard。`VirtIoBlkDev` 与
`VirtIoRngDev` 同样在 Drop 时从各自的 hub 注销设备，同线最后一个设备移除后
释放 hardirq handler。其他设备适配器未实现自定义
`Drop`，设备资源（DMA 缓冲区、VirtIO 队列）的释放依赖 `virtio-drivers` 内部类型的
`Drop` 实现。当设备结构体离开作用域时，`InnerDev` 的 `Drop` 会自动通知设备重置
并释放 DMA 内存。
