# knet — 安全与可靠性分析

## 信任模型

```text
posix/net syscall 层
   │
   │ safe API: SocketOps, Socket, SocketAddrEx, options
   v
┌─────────────────────────────┐
│ knet                        │
│                             │
│ safe boundary               │
│  ├─ TCP / UDP / raw socket  │
│  ├─ Unix socket             │
│  ├─ netlink socket          │
│  └─ Service / Router        │
│                             │
│ unsafe boundary             │
│  ├─ Unix stream ring buffer │
│  └─ vsock RX ring advance   │
└──────────────┬──────────────┘
               │
               v
driver layer / smoltcp / ringbuf
```

- safe API 调用者信任 `knet` 维护 socket 状态、路由状态、buffer 边界和 errno 映射。
- `knet` 信任 `posix/net` 完成用户指针读取、sockaddr 长度校验和地址族选择，并在每次
  netlink send 路径传入与当前调用者一致的 `Cred` 快照；socket file write 路径在
  `SocketFileOps::write` 入口读取当前凭据。netlink mutation 的权限检查由 `knet`
  使用该快照逐次执行。
- pathname Unix socket 操作信任入口传入的 `Cred` 快照与当前系统调用主体一致；
  kvfs 负责逐级 search、父目录 mutation 和 inode DAC 检查。
- `knet` 信任 driver 层返回的 `NetBufHandle` 数据切片在 handle 生命周期内有效。
- `knet` 使用基于 `zerocopy` 和 `etherparse` 的 crate 内 checked parser 校验 Ethernet、ARP、IPv4、raw IPv4 和 UDP 数据，并继续依赖 smoltcp 校验 TCP 和 IPv6 数据；`link::wire` 只处理 Ethernet frame，`link::arp` 负责 ARP packet validity，`neighbor::NeighborResolver` 负责 cache、pending queue 和 retry state。
- 设备层只接收经 `Router` 控制面适配后的 crate 内地址、CIDR 和带明确状态的邻居项；不支持邻居表的设备拒绝对应 mutation。
- unsafe 边界由 crate 内部封装，外部调用者没有直接调用 unsafe API 的入口。

## 外部边界 / 攻击面

`knet` 是面向外部输入最丰富的 crate 之一，
攻击面不仅来自 Rust `unsafe`，
还来自网络包、控制面消息、设备缓冲区和上层 syscall glue。

经检查，本模块直接或间接接触以下边界：

- **网络输入**：Ethernet、ARP、IPv4、TCP、UDP、raw IP 包；Ethernet 和 ARP 的 parser 与 neighbor state machine 分属独立模块；
- **设备输入**：driver 提供的 `NetBufHandle`、IRQ 唤醒、RX/TX 缓冲区生命周期；
- **控制面输入**：每次发送携带的调用者凭据、批量 netlink message、header、attribute、
  route mutation 和 neighbor 更新；
- **Unix / vsock 输入**：peer socket 数据、连接建立与关闭事件，
  以及 pathname Unix socket 经 kvfs 解析的路径和 inode；
- **上层 syscall 语义输入**：由 `posix/net` 传入的 socket 地址族、
  socket option、阻塞语义和权限决策结果。

用户内存访问和长度校验由 `posix/net` 在进入本模块前完成，netlink 调用者凭据也由该层
按发送操作取得快照后传入。
本模块也不直接执行 DMA 编程，
但会消费由 driver 层提供的网络缓冲区，
因此仍需把 driver buffer 生命周期视为关键边界。

因此威胁分析重点应覆盖：

- 外部网络包是否能触发越界、panic、状态不一致或资源耗尽；
- 控制面 mutation 是否绕过 `Service` 的窄接口，导致 `Router`、`NetDevice`
  或 smoltcp 兼容地址视图失配；
- netlink mutation 是否复用旧凭据，或在混合批次中产生部分提交；
- driver buffer、socket handle、ring buffer
  是否可能因生命周期管理错误破坏内存安全；
- IRQ 路径与普通协议推进路径是否可能发生竞态或延迟放大。

## unsafe 代码清单

### 1. Unix stream 写入 vacant ring buffer

位置：`src/unix/stream.rs:388` 和 `src/unix/stream.rs:392`

```rust
let mut count = src.read(unsafe { left.assume_init_mut() })?;
count += src.read(unsafe { right.assume_init_mut() })?;
```

不变量：

- `left` 和 `right` 来自同一次 `HeapProd::vacant_slices_mut`。
- 两个切片表示当前 producer 的可写容量。
- `Read::read` 只初始化它返回的字节数。
- channel lock 持有期间，其他路径无法同时推进同一 producer。

安全依据：

- `send` 持有 `self.channel.lock()` 后取得 `chan.tx`。
- 写入后只发布 `Read::read` 返回的字节数。
- `ringbuf` producer 自身维护环形 buffer 边界。

调用者：

- `StreamTransport::send`，由 Unix stream socket `SocketOps::send` 调用。

### 2. Unix stream 推进 write index

位置：`src/unix/stream.rs:407`

```rust
unsafe { chan.tx.advance_write_index(count) };
```

不变量：

- `count` 等于本次写入 `left` 与 `right` 的总字节数。
- `count` 小于等于本次 `vacant_slices_mut` 暴露的总容量。
- 推进 write index 前，相关字节已经初始化。

安全依据：

- `count` 只由两个 `Read::read` 返回值累加得到。
- 每次 `Read::read` 的目标切片长度受 ring buffer vacant 区域限制。
- channel lock 保护 producer。

调用者：

- `StreamTransport::send`。

### 3. Unix stream 推进 read index

位置：`src/unix/stream.rs:440`

```rust
unsafe { chan.rx.advance_read_index(count) };
```

不变量：

- `count` 等于本次从 `chan.rx.as_slices` 返回的 occupied 区域复制出的字节数。
- `count` 小于等于当前 readable 区域长度。
- PEEK 语义没有进入此路径。

安全依据：

- `recv` 持有 `self.channel.lock()` 后访问 `chan.rx`。
- `dst.write` 的输入切片来自 `as_slices` occupied 区域。
- `count` 由两个 `dst.write` 返回值累加得到。

调用者：

- `StreamTransport::recv`。

### 4. vsock 推进 RX read index

位置：`src/vsock/connection_manager.rs:151`

```rust
unsafe {
    self.rx_consumer.advance_read_index(count);
}
```

不变量：

- 调用者持有 connection lock。
- `count` 来自同一 connection 的 `rx_slices` 复制结果。
- `count` 小于等于当前 RX occupied 长度。

安全依据：

- `VsockStreamTransport::recv` 在 `conn.lock()` 保护下读取 `rx_slices` 并调用 `advance_rx_read`。
- PEEK 请求跳过 advance。
- ringbuf consumer 只在 connection lock 内推进。

调用者：

- feature `vsock` 下的 `VsockStreamTransport::recv`。

### 5. UDP waiter 测试专用 `RawWaker`

位置：`src/transport/udp/wait.rs:97-124`

该实现只在 `cfg(unittest)` 下编译，用于统计 `PollSet` 的唤醒次数。
`RawWaker` 数据指针始终来自 `Box::leak` 生成的静态 `AtomicUsize`，
vtable 回调保持原指针与静态生命周期，不释放该测试计数器。
生产构建不包含这组 unsafe 代码。

## 内存安全不变量

1. `SocketHandle` 生命周期：任何 smoltcp socket handle 在访问前必须仍存在于 `SOCKET_SET`。
2. `SocketSet` 互斥访问：所有 `SocketSet` 读写必须经过 `SOCKET_SET.inner` 的 mutex。
3. `Service` 互斥访问：smoltcp `Interface`、`Router` 和设备 dispatch 必须在 `SERVICE.lock()` 内推进；一次 rtnetlink mutation 的状态检查与 owner 更新必须位于同一临界区。
4. ring buffer publish 规则：Unix stream 和 vsock 只能发布已经写入的字节，只能消费当前 occupied 区域内的字节。Unix stream 的 write index 发布、方向关闭和空队列 EOF 判定由同一个方向锁排序。
5. driver buffer 生命周期：`NetBufHandle::data` 只在 handle 被 recycle 前使用，RX handle 必须在处理后归还。
6. netlink message 边界：所有 payload 读取必须先经过单条 header 长度、批次对齐边界、attribute 长度和 family 校验，非零接口编号在进入 mutation 逻辑前转换成 `InterfaceIndex`。
7. netlink credential 边界：每次 `send` 或 `write` 必须传入当前调用者的独立 `Cred` 快照，netlink socket 不得缓存调用者权限。
8. netlink batch 执行边界：混合查询和 mutation 的批次必须在 owner 更新前拒绝；同类 mutation 执行前必须为完整批次预留 response queue 空间。response 生成在 `rx_queue` 锁外完成，只在 push 时持有该锁；`SERVICE` 临界区内不得再锁任何 netlink `rx_queue`（锁顺序：`SERVICE` 先于 netlink `rx_queue`）。
9. route index 边界：rtnetlink route 的非零 `oif` 由 `InterfaceIndex` 表达，转换成设备索引后必须检查 `dev < devices.len()`。
10. 网络配置 owner：地址和路由只由 `Router` 持有，链路和邻居只由对应 `NetDevice` 持有；mutation 必须经 `Service` 窄接口更新 owner，接口名称、MTU 和配置路由 invariant 必须由 owner API 校验。
11. 地址生命周期：地址新增和删除必须同步重建自动 local、connected 路由和 smoltcp 地址视图；缺少 loopback 时只跳过依赖其 egress 的自动 local `/32`，仍须安装 connected prefix 路由；源地址从全局本机地址集合消失时，必须清理固定 `prefsrc` 已失效的路由与跨设备使用该源地址的 ARP pending 包。
12. dump 快照一致性：每类 link、address 和 route dump 必须从对应请求持有的一次 `SERVICE` 临界区完整创建窄快照，地址与设备 label 必须在临界区内完成配对。
13. static init 顺序：创建 socket 前必须完成 `init_network` 初始化 `SERVICE`、`SOCKET_SET` 和 `LISTEN_TABLE`。
14. pathname credential 一致性：一次 Unix pathname bind 的查找、创建和属主初始化必须使用同一份 `Cred` 快照。
15. kernel caller 边界：没有当前用户任务的内核调用者不得进入隐式 `current_cred()` 路径；pathname 操作和 socket file 构造都必须显式选择凭据。
16. `PacketBuf` 所有权：设备、Router、loopback 和 smoltcp adapter 之间按值转移报文；协议偏移只能落在当前有效数据范围内。
17. IPv4 输入边界：本地交付前必须校验版本、头长、总长和头部校验和，并按 `total_len` 截断尾部数据。
18. 网络类型边界：`RouteTable` 和 `NetDevice` 不暴露 smoltcp 地址或时间类型；控制面与协议兼容转换由 `Router`、`Service` 和初始化入口完成。
19. IPv4 重组边界：分片按源地址、目的地址、标识、协议和接口隔离；被已有区间完全覆盖的分片按重复包丢弃，部分重叠或范围矛盾会删除整条队列；重组状态受 64 条队列、4 MiB 高水位、3 MiB 低水位和 30 秒超时限制。
20. UDP 接收边界：UDP 长度、校验和和 payload range 通过 checked parser 验证，单个 PCB 的接收队列上限为 1024 个数据报。
21. IPv4 输出边界：输出长度必须满足 IPv4 `total_len` 字段上限；设备 TX 在最终发送点复核当前 MTU，DF 包超过 MTU 时返回 `EMSGSIZE`，允许分片的 UDP 与 raw IPv4 按 8 字节对齐切分 payload。
22. 本地地址投影边界：Router 的全局本地地址集合用于固定源地址校验和 weak-host ARP 接收，设备所属地址集合单独用于广播和出接口判断。

## 线程安全

| 类型 | Send 条件 | Sync 条件 |
|------|-----------|-----------|
| `TcpSocket` | 字段满足 Send | 内部锁、atomic 和 global socket set 串行化共享状态 |
| `UdpSocket` | 字段满足 Send | `Arc<UdpPcb>` 内的锁、atomic 和分桶 PCB registry 保护共享状态 |
| `RawSocket` | 字段满足 Send | `RwLock`、atomic 和 immutable handle 保护共享状态 |
| `StreamTransport` | 字段满足 Send | `Mutex<Option<Channel>>` 串行化本端操作，per-direction `SpinNoPreempt` 排序数据发布、半关闭与 EOF 判定，三组 `PollSet` 隔离读、写和连接状态 waiter |
| `NetlinkSocket` | 字段满足 Send | `Arc<NetlinkSocketInner>` 内部使用 `RwLock`、`Mutex` 和 `PollSet` |
| `Service` | 在 `Mutex<Service>` 内使用 | 通过全局 `Mutex` 提供共享访问 |
| `Router` | 在 `Service` 内使用 | 通过 `Service` mutex 间接共享 |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 初始化顺序错误导致 `LazyInit` 访问未初始化对象 | 高 | socket 在 `init_network` 前创建或 poll | 启动路径由 `core/kruntime` 先调用 `init_network`；新增入口需保留该顺序 |
| T-02 | smoltcp socket handle 已删除后继续访问 | 高 | listener 关闭、accepted child 清理与并发 socket 操作交错 | `SocketSet` 访问由 mutex 串行化；`ListenTable::unlisten` drain 后删除 handle；调用点需避免缓存 handle 后跨释放点使用 |
| T-03 | ring buffer index 推进超过实际写入或读取长度 | 高 | `advance_write_index` 或 `advance_read_index` 的 count 与切片来源不一致 | count 由持有 channel mutex 时取得的同一批 slices 计算；unsafe 注释固定不变量 |
| T-04 | 恶意 netlink 消息越界读取或构造非法状态 | 高 | 用户传入短 header、错误批次对齐、畸形 attr、非法 family、非法 ifindex 或未实现的语义属性 | `NlMsgHeader::read`、批次拆分、`RtnlRequest::decode`、对象请求类型的 `decode` 方法、`parse_attrs`、`parse_ip_by_family` 和 `InterfaceIndex` 转换拒绝非法输入；未实现语义返回 `EOPNOTSUPP` |
| T-05 | ARP spoofing 污染 neighbor cache | 中 | 外部主机发送伪造 ARP reply 或 request | 校验 unicast MAC、广播和本机目标 IP；request 与非 host-directed 报文只进入 `STALE`；动态 MAC 更新受 1 秒锁定时间约束，且不得覆盖 `NUD_PERMANENT` 项；其他邻居仍依赖网络隔离 |
| T-06 | listen backlog 被 SYN 洪泛占满 | 中 | 大量连接请求命中同一 listener | backlog 被 clamp 到 `LISTEN_QUEUE_SIZE`；超限丢弃并记录 warn |
| T-07 | raw socket 被无权限调用者创建 | 中 | syscall 层没有实施权限门禁 | `posix/net::sys_socket` 在创建 AF_INET raw 和 AF_PACKET socket 前检查当前凭据，当前 pre-capabilities 模型要求有效 UID 为 0 |
| T-08 | netlink RX queue 被 uevent 或 response 填满 | 中 | 订阅者不消费，publisher 持续写入 | `NETLINK_RX_QUEUE_LIMIT` 限制单 socket queue 字节数，超限丢弃 |
| T-09 | 网络配置的 owner 状态不一致 | 中 | 控制入口绕过 owner invariant，地址更新遗漏自动路由、smoltcp 兼容视图或 pending 清理，或不支持的设备静默接受邻居更新 | 地址和路由由 `Router` 持有，链路和邻居由 `NetDevice` 持有；`Service` 通过 `LinkConfigUpdate` 和路由 mutation API 提交更新，`Router` 在写入前统一校验，dump 使用三类窄快照 |
| T-10 | 外部网络包触发 parser panic | 中 | malformed Ethernet、ARP、IP、UDP 或 TCP packet 进入 RX | Ethernet 和 ARP 使用 `zerocopy` checked view，IPv4、raw IPv4 与 UDP 使用 crate 内 checked parser，TCP 和 IPv6 使用 smoltcp checked parser；错误包直接丢弃 |
| T-11 | 中断上下文误用导致锁竞争或延迟放大 | 中 | IRQ waker 回调中直接推进 `SERVICE`、`SOCKET_SET` 或执行阻塞 socket 操作 | 中断路径只注册和唤醒 waker，协议推进保留在普通 poll 路径 |
| T-12 | driver buffer 或 DMA 输入破坏 packet 边界 | 高 | 驱动返回长度异常、数据在 recycle 后继续被访问、TX/RX buffer 生命周期使用错误 | RX 数据只在 `NetBufHandle` recycle 前解析和复制；外部帧使用 checked parser；TX buffer 由 driver handle 管理 |
| T-13 | vsock-TIPC bridge 误把普通 AF_VSOCK 连接路由到 TIPC | 中 | 事件分流没有区分桥接端口或已桥接连接 | bridge 只接管静态 port map 和自己的 connection id，未命中事件继续交给 `VSOCK_CONN_MANAGER` |
| T-14 | host 通过 bridge 注入超大或非法 TIPC message | 中 | `Received` record 超过 TIPC slot 或 port 0 service name 非法 | bridge 限制 record 长度为 `IPC_CHAN_MAX_BUF_SIZE`，动态 service name 需通过 UTF-8、NUL 和长度校验；非法 name 回 `[1]` 并断开 |
| T-15 | TIPC handle/memref capability 经 vsock 泄露到 host | 高 | TA 向 bridge 发送带 attached handles 的 message | bridge v1 只转发 bytes，发现 attached handles 时关闭连接 |
| T-16 | host 误判 port 0 handshake 结果 | 中 | 未读状态字节就发 payload；忽略 `[1]`；无 recv 超时导致永久阻塞 | 协议要求 host 先读单字节状态（`0`=成功，`1`=拒绝）；`libtrusty` 使用 `SO_RCVTIMEO`；CA 测试拒绝非 `[0]` 状态 |
| T-17 | IPv4 分片耗尽内核内存 | 中 | 外部持续发送无法完成重组的不同分片流 | 重组器限制队列数量与总内存，超过高水位后淘汰最早队列到低水位，队列存活时间固定为 30 秒 |
| T-18 | 重叠 IPv4 分片混淆上层解析 | 中 | 同一重组 key 提交相互覆盖的 payload range | 被已有区间完全覆盖的分片按重复包丢弃，任何部分重叠或总长度矛盾会删除整条队列 |
| T-19 | UDP 接收洪泛占满 socket 队列 | 中 | 应用读取速度低于入包速度 | 每个 PCB 最多保留 1024 个数据报，满队列丢弃新数据报并保持内存上限 |
| T-20 | pathname Unix socket 绕过 inode/目录 DAC 或复用已有 inode | 高 | bind/connect/sendto 直接访问 binding 表，或 bind 接受已有路径 | bind 通过 `parent_at` 和 `Path::mknod` 排他创建；connect/sendto 在 lookup 后检查最终 inode `MAY_WRITE`；abstract 地址才直接访问内存 binding 表 |
| T-21 | 内核任务隐式读取用户凭据 | 高 | 启动期 pathname bind 调用普通 `SocketOps::bind`，当前线程不存在或主体错误 | 内核调用者使用 `bind_with_cred` 显式传入 `initial_cred()` 等已选择凭据；普通入口只服务当前用户任务 |
| T-22 | Unix stream 在 EOF 后发布数据 | 中 | send、shutdown 与 peer recv 并发交错，关闭状态和 write index 缺少共同排序 | 每个发送方向使用共享 `tx_order`；send 在锁内复检后发布，recv 在锁内复查 empty 和 closed，Channel 释放前先发布关闭状态 |
| T-23 | netlink socket 复用旧凭据导致越权 mutation | 高 | socket 跨进程传递或调用者凭据变化后继续使用创建时权限 | POSIX send 与 socket file write 路径每次取得当前 `Cred`，`NetlinkSocket::send_with_cred` 逐条检查 mutation；无权限请求生成 `NLMSG_ERROR` 和 `EPERM` |
| T-24 | 混合 netlink 批次发生部分 mutation | 高 | 同一发送同时包含 query 和 mutation，或 response queue 在批次中途耗尽 | 混合批次在执行前返回 syscall `EOPNOTSUPP`；同类 mutation 批次先预留完整 response 空间 |
| T-25 | ARP 使用错误 SPA 或保留失效源地址的 pending 包 | 中 | 多地址设备发送、跨设备本地源、地址删除与解析重试交错 | request 使用实际 packet source 作为 SPA，全局本地地址均可作为 reply target，源地址从全局本机集合消失时跨设备清理 pending，设备 deadline 主动重试 |
| T-26 | MTU 或 link 状态选择错误导致错误出接口和超长报文 | 中 | 多设备 MTU 不同、并发降低 MTU、部分 link down 或 IPv4 长度超过字段上限 | 发送前按路由 MTU 检查，设备 TX 按当前 MTU 最终复核，down 设备不参与单播源和出接口选择，IPv4 长度限制为 `u16::MAX` |

影响等级定义：

- 高：导致 UB、内存破坏、权限提升。
- 中：导致 panic、服务不可用、数据不一致。
- 低：导致性能退化、日志丢失、功能降级。

## 故障模式与影响分析

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 网络设备缺失 | `DeviceContainer<NetDevice>` 为空 | 只有 loopback 可用 | 对外 TCP、UDP、raw IP 返回无路由或连接失败 | 3 | `init_network` 记录 warn，`Service::get_source_address` 返回 `ENETUNREACH` |
| F-02 | 目的地址无路由 | `RouteTable::lookup` 未命中 | 当前发送失败 | 应用连接失败 | 3 | connect 或 send 路径返回 `ENETUNREACH`，dispatch 路径 warn 后丢弃 |
| F-03 | Ethernet pending queue 满或 ARP 解析失败 | ARP 未解析且 `pending_tx` 达到上限，或 6 次 request 未得到应答 | 新进入的 IP 包丢弃，已排队包在失败时清理 | 单目的或多目的通信丢包并由上层超时 | 3 | 记录 warn；维护逻辑按 next-hop 推进，单个 unresolved next-hop 不阻塞其他已解析目标；失败项进入 `FAILED` 并设置回收 deadline |
| F-04 | driver TX 失败 | NIC driver 在 buffer 回收、分配或发送阶段返回 `WouldBlock`、`ResourceBusy` 或硬错误 | 当前 frame 未发送 | 网络吞吐下降或连接超时 | 3 | 可重试错误保留 pending 包并等待维护 deadline；硬错误记录后移除对应包；ARP probe 仍受 6 次上限约束 |
| F-05 | UDP ICMP error queue 丢失 | PCB registry 未注册、ICMP 引用头无效或 socket 已关闭 | `SO_ERROR` 或 error queue 缺失 | 应用无法获得异步网络错误 | 4 | socket 创建时初始化 PCB registry，bind 时登记 PCB，Drop 时注销；ICMP 引用报文使用允许截断 payload 的 IPv4 头解析路径 |
| F-06 | TCP accepted child abort | 握手期间 peer reset 或 smoltcp child 关闭 | `accept` 返回 `ConnectionAborted` | 应用重试 accept | 4 | `ListenTable::accept` 清理 closed child 并继续扫描队列 |
| F-07 | poll waker 或协议 timer 丢失 | device mask 错误、timeout 或 Ethernet IRQ source 未注册、注册所有者未跨 `Pending` 存活、缺少 register 后复查或 poller 未按 deadline 调度 | socket 阻塞等待延迟，TCP 重传或 keep-alive 延后 | 应用 IO latency 上升或连接超时 | 3 | bind/connect 后更新 device mask；`Service::register_rx_waker` 通过同一个 `PollContext` 直接注册 timeout poll 和相关 Ethernet IRQ source；调用方持有 `PollRegistrations` 并在注册后复查 readiness；每次 Interface poll 后按 `poll_at` deadline 调度 poller |
| F-08 | malformed netlink request | header、批次边界或 attr 长度非法 | framing 错误使发送返回 syscall `EINVAL` 且不生成 response；完成拆分后的 payload 或 attr 错误生成 `NLMSG_ERROR` | 调用者请求失败 | 4 | checked reader、批次拆分和 error response 处理 |
| F-09 | SERVICE 尚未初始化 | netlink route 请求早于 `init_network` | mutation 或 dump 无法访问网络配置 | netlink 功能不可用 | 2 | `ensure_service_inited` 返回 `ENODEV`；启动路径需先完成 `init_network` |
| F-10 | Unix stream peer 提前关闭 | channel 被 shutdown 或 drop | send 在无发送进度时返回 `BrokenPipe`，已有进度时返回部分字节数；recv 排空本端缓冲后返回 EOF；peer 关闭时丢弃未读输入则返回一次 `ConnectionReset`；poll 报告 `RDHUP`、`HUP` 或 `ERR` | 应用感知连接关闭 | 4 | endpoint atomic 记录双方半关闭状态与待处理 reset；per-direction `tx_order` 统一关闭与数据顺序；三组 `PollSet` 按受影响事件定向唤醒 |
| F-11 | 中断上下文执行重型网络推进 | IRQ 路径误调用 socket send、recv 或 `poll_interfaces` | 锁竞争、调度延迟或死锁 | 网络 IO 延迟上升，严重时系统卡顿 | 2 | IRQ 路径只做 waker notification，实际协议推进由 task 上下文执行 |
| F-12 | RX buffer recycle 顺序错误 | frame payload 在 `recycle_rx` 后仍被引用 | 读取悬垂数据或数据损坏 | packet 解析异常，严重时破坏内存安全 | 1 | `EthernetDevice::poll_rx` 在 recycle 前完成解析和复制；新增设备适配需保持同样生命周期 |
| F-13 | port 0 handshake 永久等待 | host 早于 TA publish 连接且未设 recv 超时；或 service 永不 publish | host `read` 阻塞；负例测试挂起 | CA/测试进程无响应 | 3 | dynamic connect 保留 `WAIT_FOR_PORT`；host 设 `TRUSTY_VSOCK_TIMEOUT_SEC`；明确拒绝场景回 `[1]` |
| F-14 | 快速重连 `tipc_connect` 超时 `-11` | `route_event` 在 `has_connection()` 前丢弃同批 `Received`，service-name record 丢失 | host status-byte `EAGAIN`；约半数快速重连失败 | storage client/proxy harness 间歇失败 | 2 | mapped bridge port 仅按 `local_port` 认领；事件入 FIFO，不依赖 `has_connection()` |
| F-15 | IPv4 分片重组超时 | 首片到达后 30 秒内缺少后续分片 | 当前数据报丢失 | UDP 接收超时 | 3 | 删除过期队列；首片存在且允许回复时发送 ICMPv4 Fragment Reassembly Timeout |
| F-16 | UDP DF 数据报超过路由 MTU | `IP_MTU_DISCOVER` 要求 DF 且 packet 长度超过路由 MTU | 当前发送失败 | 应用收到 `EMSGSIZE` | 4 | 发送前读取路由 MTU，Router 拒绝对 DF 包执行输出分片 |
| F-17 | 启动期 Unix pathname bind panic | 内核任务调用隐式 `current_cred()`，但尚无当前用户线程 | `/dev/log` 等内核 socket 无法绑定 | 启动中断 | 2 | 启动期调用 `bind_with_cred` 并显式传入 `initial_cred()`；保留可用的初始 fs context |
| F-18 | smoltcp 过期 poll 期限变成超长等待 | 有符号微秒差值为负后通过 `as u64` 转换 | soft timer 被设置到远未来 | TCP 数据路径停顿，可能伴随 timer IRQ 异常 | 2 | 在同一 epoch 下直接把 `SmoltcpInstant` 映射为 `MonotonicInstant`，不计算无符号 delay；单测覆盖过期和未来期限 |
| F-19 | 无权限 netlink mutation | 当前发送凭据不具备配置权限 | mutation 不执行，RX queue 收到 `NLMSG_ERROR` 和 `EPERM` | 调用者配置失败 | 4 | 每次发送重新检查凭据；error 入队后发送入口返回已消费请求长度 |
| F-20 | 混合 query 和 mutation 批次 | 单次发送包含两类请求 | 整批未执行并返回 syscall `EOPNOTSUPP` | 调用者需拆分批次 | 4 | 批次分类在 owner mutation 和 response 生成前完成 |
| F-21 | 本机源地址彻底删除时存在 ARP pending | pending 包仍使用已经从全局本机地址集合消失的源地址 | 旧包无法继续合法发送 | 邻居解析重试产生错误 SPA | 3 | 地址投影更新时跨设备清理使用该源地址的 pending 包 |
| F-22 | IP 报文超过出口 MTU 或 IPv4 长度上限 | 并发降低 MTU、路由缓存过期或 payload 超过 `total_len` 表示范围 | DF 报文或无法分片的报文返回 `EMSGSIZE`，允许分片的报文拆分后发送 | 当前数据报发送失败或按分片发送 | 4 | 发送前计算输出分片数量，Router 按当前 MTU 执行分片，设备 TX 最终复核 MTU 和 IPv4 长度上限 |

严重度定义：

- 1：致命，系统崩溃、数据丢失。
- 2：严重，功能不可用，需重启恢复。
- 3：一般，功能降级，可自动恢复。
- 4：轻微，影响有限，用户可容忍。

## 故障管理

- 普通输入错误使用 `KError` 和 `LinuxError` 返回，例如 `EINVAL`、`EAFNOSUPPORT`、`ENETUNREACH`、`EADDRINUSE`、`EWOULDBLOCK`。
- netlink framing 错误在处理前返回 syscall `EINVAL` 且不生成 response；完成批次拆分后的 payload 或 attribute 错误返回 `NLMSG_ERROR`。
- 无权限 netlink mutation 在 RX queue 中返回带 `EPERM` 的 `NLMSG_ERROR`；混合查询和 mutation 批次在修改 owner 前返回 syscall `EOPNOTSUPP`。
- source-specific、metric、multipath 和其他未实现的路由语义属性返回 `EOPNOTSUPP`。
- malformed Ethernet、ARP、IP、UDP、TCP 包在 RX 路径丢弃，并通过 warn 或 trace 记录。
- UDP PCB 接收队列和 Router TX 队列满时映射为丢包或 `WouldBlock`，poller 负责等待 IO readiness。
- smoltcp TCP buffer、raw/UDP 接收队列和 Unix stream ring buffer 满时映射为 `WouldBlock` 或丢弃新包，poller 负责等待 IO readiness；非阻塞 Unix stream send 已有进度时返回部分字节数。
- loopback 和 Ethernet 队列满时丢包并记录 warn。
- link down 后设备停止普通 IP 收发，Ethernet raw frame 发送返回 `ENETDOWN`。
- UDP 的 DF 报文超过选定出口 MTU 时返回 `EMSGSIZE`，允许分片的 UDP 与 raw IP 按出口 MTU 分片；出口 MTU 小于 IPv4 头部或总长超过字段上限时返回 `EMSGSIZE`。
- panic 路径主要来自初始化顺序、内部 invariant 破坏和 `expect` 断言；新增公开入口应先返回 `KError`，再进入内部断言区。

## 隐私分析

`knet` 会处理用户进程通过 socket 发送的 payload、从网络收到的 packet payload、Unix socket credentials、netlink 消息和 vsock payload。
这些数据在内核内按 socket buffer、ring buffer、driver buffer 或 netlink queue 保存。
模块自身不做持久化，也不把 payload 写入日志；trace 日志当前会输出 Ethernet frame 字节，生产环境需按日志级别控制敏感网络数据泄露。

## 已知限制

- `RTM_GETNEIGH` dump 尚未实现；控制面通过 `RTM_NEWNEIGH` 写入 Ethernet 邻居表，ARP RX 继续维护动态邻居。控制面当前支持 `NUD_INCOMPLETE` 和 `NUD_PERMANENT`，设备内部还维护 `REACHABLE`、`STALE`、`DELAY`、`PROBE` 和 `FAILED`，前者在首次 egress 时发起 ARP，后者不接受动态 ARP 覆盖；其余控制面 NUD state、非零 neighbor flags 和非 Ethernet 设备返回 `EOPNOTSUPP`。
- IPv4 输出使用匹配路由的接口 MTU，ICMP Fragmentation Needed 中的 next-hop MTU 只进入 UDP error queue，尚未形成动态 PMTU cache。
- IPv4 输出分片只支持无 options 的栈内生成报文，尚未实现 options copy 语义。
- `link_snapshots`、`ipv4_addr_snapshots` 和 `route_snapshot` 只覆盖各自已实现的 dump 字段，未包含邻居表、驱动瞬时统计和 smoltcp 内部计数。
- `Router` 最多配置 8 个 IPv4 地址；replace 已存在地址保留原 scope。
- source-specific route、metric、multipath、非 main table、非 unicast type 和其他未建模的选路语义返回 `EOPNOTSUPP`。
- `NETLINK_ROUTE` multicast notification 尚未实现，bind 的非零 group mask 以及发送目标中的非零 pid 或 group 返回 `EOPNOTSUPP`。
- 自动 local `/32` 在当前内部模型中仍表示为 main table、unicast type 和 loopback egress，尚未拆分独立 local table 与 local route type。
- crate 内 UDP 按路由 MTU 执行 DF 检查或输出分片，raw IP 按实际出口设备 MTU 输出分片；普通 IPv4 multicast socket 发送尚未实现并返回 `EOPNOTSUPP`。TCP 和其他 smoltcp `Interface` 流量仍受所有设备最小 MTU 的保守上限约束。
- raw socket 创建权限由 syscall 层承担，`knet` 构造器自身没有进程凭据参数。
- Ethernet 设备只处理 IPv4 ARP，IPv6 NDP、非 Ethernet 链路和多队列 NIC 抽象仍待扩展。
- crate 内 UDP 数据路径当前只支持 IPv4；IPv6 UDP 继续由 smoltcp DNS 路径使用，普通 UDP socket 不提供 IPv6 收发。
- vsock-TIPC bridge v1 不转发 TIPC handles 或 memrefs，也不为 vsock send credit 建立持久重试队列。

## 审计清单

修改本模块时需验证：

- 每个 `unsafe` 块均有 `SAFETY:` 注释。
- 新增 smoltcp socket handle 的生命周期受 `SOCKET_SET` 保护。
- 新增 route 或 device mutation 通过 `Service` 窄接口更新对应 owner，owner API 在写入前完成接口名称、MTU、设备索引、路由能力、`prefsrc` 和 gateway 校验，并让每类 dump 从对应请求持有的一次 `SERVICE` 临界区完整创建窄快照。
- 地址 mutation 同步维护 local、connected 路由、固定 `prefsrc` 有效性、smoltcp 地址视图和 ARP pending，`Router` 最多保留 8 个地址；replace 已有地址保持原 scope。
- 动态选源按 scope 过滤，并优先选择前缀覆盖 gateway 或直连 next-hop 的地址；link down 的设备不参与单播源地址和出接口选择。
- 每次 netlink `send` 或 `write` 传入当前调用者的 `Cred` 快照，socket 不缓存权限；无权限 mutation 生成带 `EPERM` 的 `NLMSG_ERROR`。
- 新增 netlink parser 先校验单条 header 长度和批次对齐边界，再通过 `RtnlRequest::decode` 与对象请求类型的 `decode` 方法校验 attribute、family 和 index；未实现的语义属性显式返回 `EOPNOTSUPP`。
- 仅含查询或仅含 mutation 的 netlink 批次支持逐条处理，混合查询和 mutation 批次在 owner 更新前返回 `EOPNOTSUPP`，mutation 批次执行前预留完整 response 空间。
- ARP request 使用实际 packet source 作为 SPA，Router 全局本机地址均可回复 request；动态邻居过期后保留旧 MAC，实际发包进入 `DELAY`，随后进行最多 6 次 `PROBE`；失败时清理对应 pending，设备 deadline 负责重试和动态项回收。可重试 TX 失败保留 pending，源地址从全局本机集合消失时跨设备清理 pending。
- UDP 和 raw IP 使用实际出口 MTU，定向广播使用路由设备 MTU，全局广播使用参与设备最小 MTU，普通 IPv4 multicast socket 发送返回 `EOPNOTSUPP`；设备最终 TX 复核当前 MTU；smoltcp 单 `Interface` 的全局最小 MTU 限制需保持明确。
- 新增 ring buffer 操作的 advance count 来自同一锁内同一批 slices。
- Unix stream 的 write index 发布、方向关闭和空队列 EOF 判定保持同一个 per-direction 排序点，方向锁内不执行用户复制或 `PollSet` 唤醒，shutdown 在释放 `channel` mutex 后执行 waiter 唤醒。
- 新增外部网络输入使用 checked parser。
- IPv4 分片重组改动保持队列数量、内存和超时上限，并拒绝重叠 range。
- UDP registry 改动保持 bind、connect、普通接收和 ICMP error lookup 使用同一 PCB 所有权来源。
- 新增 socket option 明确 errno、阻塞语义和 poll readiness。
- 新增 pathname Unix socket 入口使用单次凭据快照，并让全部 VFS 操作显式接收该快照。
- pathname bind 保持排他创建、`0777 & !umask` mode 和 fs credential owner；connect/sendto
  在读取 binding 前检查最终 inode `MAY_WRITE`。
- 新增内核调用路径不得依赖 `current_cred()`；调用者必须显式选择凭据。
- 新增公开 API 先确认是否需要跨 crate 暴露。
