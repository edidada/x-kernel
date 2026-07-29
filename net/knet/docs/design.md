# knet — 设计文档

## 定位

`knet` 提供 x-kernel 的内核网络栈抽象。
它把驱动层网卡、smoltcp 协议栈、socket 对象、轮询唤醒、Unix domain socket、netlink 和可选 vsock 组合成统一的 `SocketOps` 接口。
上层 `posix/net` 负责 syscall 参数解析和文件描述符接入，`core/kruntime` 负责启动期设备注入，`knet` 负责协议状态、路由、收发推进和 socket 语义。

## 背景

x-kernel 运行在 `no_std` 内核环境中，无法直接使用 Linux 内核网络栈或标准库网络类型。
当前实现由 crate 内 IPv4、UDP 和 ICMPv4 数据路径处理 UDP 收发、分片与差错报告，smoltcp 继续处理 TCP、raw IP、IPv6 和 DNS，并提供兼容期 interface poll 模型。

## 范围

涉及的源文件：

```text
net/knet/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── device/
    │   ├── mod.rs
    │   ├── ethernet.rs
    │   ├── loopback.rs
    │   └── vsock.rs
    ├── link/
    │   ├── buf.rs
    │   ├── mod.rs
    │   ├── packet.rs
    │   └── wire.rs
    ├── netlink/
    │   ├── mod.rs
    │   ├── route.rs
    │   ├── socket.rs
    │   └── wire.rs
    ├── socket/
    │   ├── mod.rs
    │   ├── file.rs
    │   ├── general.rs
    │   ├── options.rs
    │   └── state.rs
    ├── stack/
    │   ├── fragment.rs
    │   ├── ipv4.rs
    │   ├── mod.rs
    │   ├── service.rs
    │   ├── router.rs
    │   ├── listen_table.rs
    │   └── wrapper.rs
    ├── transport/
    │   ├── mod.rs
    │   ├── tcp.rs
    │   ├── udp/
    │   │   ├── input.rs
    │   │   ├── mod.rs
    │   │   ├── output.rs
    │   │   ├── pcb.rs
    │   │   ├── registry.rs
    │   │   ├── state.rs
    │   │   ├── socket.rs
    │   │   └── wait.rs
    │   ├── raw.rs
    │   └── udp_err.rs
    ├── unix/
    │   ├── dgram.rs
    │   └── stream.rs
    └── vsock/
        ├── connection_manager.rs
        └── stream.rs
```

测试辅助代码位于 `socket/test_options.rs`、`socket/test_state.rs`、`netlink/tests.rs`，
IPv4、分片重组和 UDP 的回归测试与对应实现放在同一模块内。

## 架构

```text
core/kruntime
   │ init_network
   v
┌──────────────────────────┐
│ Service                  │
│  ├─ smoltcp Interface    │
│  ├─ Router               │
│  └─ timeout PollSet      │
└──────────┬───────────────┘
           │
           v
┌──────────────────────────┐
│ Router                   │
│  ├─ RouteTable           │
│  ├─ LoopbackDevice       │
│  └─ EthernetDevice       │
└──────┬──────────────┬────┘
       │ UDP / ICMPv4 │ TCP / raw / IPv6 / DNS
       v              v
┌──────────────────┐  ┌──────────────────────────┐
│ UDP PCB registry │  │ SocketSetWrapper         │
│ and socket queues│  │  └─ smoltcp SocketSet    │
└────────┬─────────┘  └──────────┬───────────────┘
         │                       │
         └───────────┬───────────┘
                     v
┌──────────────────────────┐
│ SocketOps implementations│
│ TCP / UDP / raw / Unix   │
│ netlink / vsock          │
└──────────────────────────┘
```

| 组件 | 职责 |
|------|------|
| `init_network` | 创建 loopback 与首个 Ethernet 设备，建立初始地址和默认路由，初始化 `Service`、`SocketSetWrapper` 与 `ListenTable` |
| `Service` | 持有 smoltcp `Interface`、`Router`、poll timeout 和 RX waker 注册入口 |
| `Router` | 管理设备列表、带 MTU 的路由表、`PacketBuf` RX/TX 队列、IPv4 输入校验、分片重组、输出分片、next-hop 选择和设备 dispatch |
| `NetDevice` | 抽象 loopback、Ethernet 和 feature gated vsock 设备后端，通过 `PacketBuf` 转移报文所有权，并使用 crate 内地址类型和 `MonotonicInstant` 表达设备边界 |
| `PacketBuf` | 保存报文数据、协议偏移、接口索引、包类型、校验状态和当前所有者 |
| `link::wire` | 使用 `zerocopy` 校验和构造 Ethernet 与 Ethernet/IPv4 ARP 头部 |
| `stack::ipv4` | 使用 `etherparse` 校验和构造 IPv4 与 ICMPv4 头部，并执行输出分片 |
| `stack::fragment` | 按源地址、目的地址、标识、协议和接口重组本地 IPv4 分片，并限制队列数量、内存和存活时间 |
| `transport::udp` | 维护 UDP PCB registry、接收队列、bind 与 connect 状态、校验和、socket option、异步错误和 IPv4 收发 |
| `SocketSetWrapper` | 串行化 smoltcp socket set 访问，并在新增 socket 时通知等待者 |
| `ListenTable` | 管理 TCP listen backlog、SYN 队列、accept 队列和 accept waker |
| `GeneralOptions` | 统一管理 nonblock、reuseaddr、超时和设备 mask |
| `SocketOps` | 上层 socket syscall 使用的统一操作接口 |
| `netlink` | 提供 AF_NETLINK socket、kobject uevent 和有限 rtnetlink 控制面 |
| `unix` | 提供 Unix domain stream 与 datagram transport；pathname 地址通过 kvfs 查找或创建 socket inode |
| `vsock` | 在 `vsock` feature 下提供 virtio-vsock stream 支持 |

## 调用约束 / 执行上下文

`knet` 运行在内核上下文中，
但并非所有路径都适用于任意执行环境。
调用者需要满足以下约束：

- **依赖初始化顺序**：在创建 socket、访问路由状态、
  或推进协议栈之前，必须先完成 `init_network`，
  以初始化 `SERVICE`、`SOCKET_SET`、`LISTEN_TABLE`
  和 netlink 初始状态。
- **普通协议推进不应在硬中断上下文执行**：
  `poll_interfaces`、socket send/recv/connect/accept、
  netlink mutation 等路径会获取 `Mutex` / `RwLock`
  并推进较重的数据路径，应在普通任务上下文中运行。
- **IRQ 路径只做通知**：
  设备中断回调应只负责唤醒等待者或登记事件，
  不应直接执行完整的协议推进或阻塞式 socket 语义。
- **允许阻塞的路径依赖 poll/waker 语义**：
  阻塞式 socket 操作依赖 `PollSet`、waker
  和 timeout 注册机制。`Pollable::register` 只能通过调用方提供的
  `PollContext` 注册源；调用方必须让对应 `PollRegistrations`
  跨 `Pending` 存活，并在注册后复查 readiness，以同时保证取消清理
  和关闭 check/register 竞态。
- **不要求固定当前进程线程才能访问全局状态**：
  `SERVICE` 和 `SOCKET_SET`
  的共享访问主要依赖全局锁和原子状态，
  但 syscall 语义相关路径仍由 `posix/net`
  负责提供进程文件描述符与凭据语境。
- **pathname Unix socket 需要文件系统与凭据语境**：
  用户态 `bind` / `connect` 必须在具有当前线程和 fs context 的任务中调用；
  不具有当前用户任务的内核调用者必须使用 `bind_with_cred`
  显式传入凭据，并保证调用环境具有可用的 fs context。
- **可重入性受全局锁约束**：
  允许多执行路径并发进入 crate，
  但同一时刻对 `Service`、`SocketSet`
  或 listener backlog 的关键访问会被串行化。

## 状态机

### TCP socket 状态

`TcpSocket` 使用 `StateLock` 记录用户可见状态，并通过 smoltcp socket 状态完成协议推进。

```text
Idle ──bind/connect/listen──> Busy
Busy ──connect pending──────> Connecting
Busy ──listen ok───────────> Listening
Connecting ──smoltcp established──> Connected
Listening ──accept child──────────> Listening
Connected ──shutdown/close────────> Closed
Busy ──operation error────────────> previous state
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| `Idle` | `Busy` | `StateLock::lock` 赢得状态转换 |
| `Busy` | `Connecting` | `connect` 成功提交到 smoltcp |
| `Busy` | `Listening` | `listen` 注册到 `ListenTable` |
| `Connecting` | `Connected` | `poll_connect` 观察到 smoltcp `Established` |
| `Connected` | `Closed` | shutdown 或底层状态进入关闭路径 |
| `Busy` | 原状态 | `StateGuard::transit` 内部操作返回错误 |

### rtnetlink 配置更新

```text
rtnetlink request
        │ parse and validate
        v
Service narrow mutation
        │
        ├─> Router: addresses and routes
        ├─> NetDevice: link and neighbor state
        └─> smoltcp Interface: compatible IP address view
                    │
                    └─narrow snapshots──> link, address and route dump
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| rtnetlink request | `Service` mutation | `apply_newlink`、`apply_newaddr`、`apply_newroute`、`apply_newneigh` 完成校验 |
| `Service` mutation | owner state | `Router` 或 `NetDevice` 的窄接口更新对应配置 |
| owner state | netlink dump | `Service::link_snapshots`、`Service::ipv4_addr_snapshots` 或 `Service::route_snapshot` 在对应 dump 持有的一次 `SERVICE` 锁定期间读取 owner |

## 算法流程

### 网络初始化

1. `init_network` 创建 `Router`。
2. 添加 `LoopbackDevice`，注册 `127.0.0.1/8` 路由。
3. 从 `DeviceContainer<NetDevice>` 取首个 NIC，包装成 `EthernetDevice`，注册默认 IPv4 路由。
4. 创建 `Service`，把 loopback 和 Ethernet 地址写入 smoltcp `Interface`。
5. 初始化全局 `SOCKET_SET`、`LISTEN_TABLE` 和 UDP PCB registry。

### RX 推进

1. `poll_interfaces` 获取 `SERVICE` 和 `SOCKET_SET` 锁。
2. `Service::poll` 调用 `Router::poll`，设备以 `PacketBuf` 形式逐包转移 RX 所有权。
3. `Router::poll` 根据网络层版本分流，IPv4 包先校验头部、长度、校验和和本地目的地址。
4. IPv4 分片进入 `Ipv4Reassembler`。完整数据报进入 crate 内 UDP 分流，未命中 socket 的单播 UDP 触发 ICMPv4 Port Unreachable。
5. UDP PCB 按 FIFO 保留已入队的数据报；`connect` 更新后续入包的 peer 匹配，不重排或丢弃连接前已入队的数据报。
6. TCP、raw IP、ICMP 和 IPv6 数据继续进入 smoltcp RX adapter，`Interface::poll` 驱动兼容期协议 socket。
7. `ListenTable::wake_touched_acceptors` 唤醒 accept 等待者。
8. `Router::dispatch` 把 crate 内 UDP 和 smoltcp 生成的 TX IP 包按路由发到设备。
9. loopback 或设备 TX 触发后继续返回 `true`，外层循环继续推进。

### TX 路由

1. crate 内 UDP 根据目的地址选择源地址和匹配路由的 MTU，在单个预分配 buffer 中写入 payload、UDP 头和 IPv4 头后提交到 `Router` 的 TX 队列。smoltcp 协议也通过同一队列提交 IP 包。
2. `IP_MTU_DISCOVER` 决定 UDP IPv4 头部的 DF 标志。超出路由 MTU 且允许分片的包由 `fragment_output_packet` 拆分，DF 包返回 `EMSGSIZE`。
3. `Router::dispatch` 接管所有权，并按 IP 版本解析目的地址；IPv4 输出先按实际长度更新 `total_len` 和头部校验和。
4. IPv4 广播包复制到参与转发且处于 up 状态的设备，并以这些设备的最小 MTU 作为发送上限；普通 IPv4 multicast socket 发送返回 `EOPNOTSUPP`。
5. 单播包通过 `RouteTable::lookup` 选择最长前缀路由，处于 down 状态的设备不参与源地址和出接口选择。
6. 带固定 `prefsrc` 的路由使用该地址；动态选源先按地址 scope 过滤，再优先选择其前缀覆盖 gateway 或直连 next-hop 的地址。
7. crate 内 UDP 和 smoltcp raw IP 在提交报文前按对应出口 MTU 与 IP 长度字段上限检查长度，超限返回 `EMSGSIZE`。
8. Ethernet 设备先查 ARP neighbor cache，命中后通过 `link::wire` 封装 Ethernet frame。
9. `NUD_INCOMPLETE` 和 cache miss 把 IP 包及其实际源地址放入 `pending_tx`，ARP request 使用该源地址作为 SPA；设备维护 deadline 主动推进重试。
10. 可重试的 TX 失败保留 pending 包和重试状态，硬错误记录后丢弃对应包；源地址从全局本机地址集合消失时，跨设备移除使用该源地址的 pending 包。
11. ARP 报文的 target protocol address 命中全局本机地址集合时可被接收，包含其他接口的本地地址。
12. `NUD_PERMANENT` 直接提供固定 MAC 映射，动态 ARP 学习不会覆盖该配置。
13. link 进入 down 状态后，loopback 和 Ethernet 停止普通 IP 收发，Ethernet raw frame 发送返回 `ENETDOWN`。

当前实现保留 smoltcp TCP、raw socket 和 IPv6 推进。UDP、IPv4 分片重组、输出分片和 ICMPv4 UDP 差错报告使用 crate 内实现。
`PacketBuf`、`link::wire` 和 `stack::ipv4` 提供设备与 crate 内 IPv4/UDP 数据路径共用的报文表示和 parser/emitter。
路由表和设备接口使用 `crate::ip` 地址类型，设备时间使用 `ktime::MonotonicInstant`。
`Router`、`Service` 和初始化入口在 smoltcp 兼容边界完成地址与时间转换。
smoltcp 的单个 `Interface` 仍共享所有设备的最小 MTU，因此 TCP 和其他经该
`Interface` 发送的流量受全局最小 MTU 的保守约束。

### TCP listen 和 accept

1. `TcpSocket::listen` 把本地 endpoint 注册到 `ListenTable`。
2. `Router::poll` snoop 首个 SYN 包，并调用 `ListenTable::incoming_tcp_packet`。
3. `ListenTable` 为新连接创建一个 smoltcp TCP socket，放入 SYN 队列。
4. 后续 poll 观察到连接可接受后移动到 accept 队列。
5. `TcpSocket::accept` 取出 `AcceptedTcp`，构造新的 connected `TcpSocket`。
6. POSIX 层通过 `AcceptOptions` 传入监听文件的 nonblocking 状态；accept 队列为空时，
   nonblocking 调用返回 `WouldBlock`，由用户态 poll/epoll 负责等待下一次可读事件。

### Unix pathname bind 和 connect

1. 用户态 `SocketOps::bind` 在操作入口获取一次当前线程的 `Arc<Cred>` 快照，并计算
   `0777 & !umask`，随后调用 `bind_with_cred`。
2. `bind_with_cred` 使用同一份 `&Cred` 和明确 mode，通过 `parent_at` 与 `Path::mknod`
   排他创建 socket inode；任意已有路径都映射为 `EADDRINUSE`，不会复用已有 socket inode。
3. kvfs 负责检查每一级目录的 search 权限、父目录的 write/search 权限，并以
   `fsuid` / `fsgid` 初始化新 inode 的属主。
4. 不具有当前用户任务的内核调用者显式传入凭据和 mode；例如 devfs 创建 `/dev/log`
   时传入 `initial_cred()` 与 `0755`，不尝试读取不存在的当前线程凭据。
5. pathname `connect` 和 datagram `sendto` 在操作入口获取凭据；kvfs 使用该快照完成
   全路径查找，并要求最终 socket inode 具有 `MAY_WRITE` 后才查询内存 binding。
6. abstract 地址不进入 VFS，因此不执行 pathname DAC 检查。

### rtnetlink 请求

1. POSIX send 路径和 socket file write 路径分别在每次操作入口取得当前调用者的 `Cred` 快照，通过 `Socket::send_with_cred` 传给 `NetlinkSocket::send_with_cred`；socket 不缓存调用者权限。
2. `NlMsgHeader::read`、批次拆分和各类 `parse_*` helper 校验每条 netlink message 的 header、对齐边界与 attribute；批次 framing 错误在处理任何 message 前返回 syscall `EINVAL`。
3. 仅含查询的批次逐条生成 response，仅含 mutation 的批次在预留 RX response 空间后逐条执行；混合查询和 mutation 的批次在修改 owner 前返回 syscall `EOPNOTSUPP`。
4. 每条 mutation 使用本次发送携带的凭据检查权限。无权限请求在 RX queue 中生成带 `EPERM` 的 `NLMSG_ERROR`，response 入队后发送入口返回已消费的请求长度。
5. link、address 和 route dump 分别通过 `Service::link_snapshots`、`Service::ipv4_addr_snapshots` 和 `Service::route_snapshot` 读取当前 owner，并生成 multi-part response。
6. mutation 请求获取一次 `SERVICE` mutex，在同一临界区内检查当前 owner 状态并通过 `Service` 的窄接口更新地址、路由、链路或邻居，失败时返回 netlink error。
7. 地址更新同时维护自动生成的 local 和 connected 路由、smoltcp `Interface` 的兼容地址视图以及 Ethernet pending 状态。
8. source-specific、metric、multipath 和其他未实现的语义属性返回 `EOPNOTSUPP`；不支持邻居表的设备同样返回 `EOPNOTSUPP`，带 `NLM_F_ACK` 的成功请求返回 ack。

## 并发模型

- 全局 `SERVICE` 是 `LazyInit<Mutex<Service>>`，串行化 smoltcp `Interface` 和 `Router` 访问。
- 全局 `SOCKET_SET` 内部使用 `Mutex<SocketSet>`，所有 smoltcp socket handle 访问都通过 `with_socket_mut` 串行化。
- `UDP_PCB_REGISTRY` 按端口分为 256 个 bucket，每个 bucket 独立加锁；每个 UDP PCB 的接收队列、endpoint、错误队列和 option 状态分别受锁或 atomic 保护。
- `Ipv4Reassembler` 位于 `Router` 内，并由 `SERVICE` mutex 串行访问。
- `LISTEN_TABLE` 用 `Mutex<HashMap<...>>` 管理 listener；每个 entry 的 `accept_poll`
  放在 entry 级 `Mutex` 之外，backlog 队列仍由该 `Mutex` 保护。`register_accept_waker`
  先无锁注册到 `accept_poll`，再短持锁做 readiness recheck，避免在队列锁内做
  `Waker::clone` / `PollSet` 工作，同时保留 register-recheck。
- TCP 状态转换使用 `StateLock` 的 atomic CAS，失败时返回当前状态。
- socket option 和 shutdown 标志使用 atomics，跨线程读写只表达配置或关闭状态。
- Unix stream 的 `channel: Mutex<Option<Channel>>` 串行化同一 endpoint 发起的 send、recv、shutdown 和 channel 释放。
- Unix stream 每个 endpoint 的 `tx_order: SpinNoPreempt<()>` 是对应发送方向的共享排序点。本端 write shutdown、对端 read shutdown、write index 发布和对端空队列 EOF 判定都经过该锁。固定锁序为 `channel` mutex 后取得单个 `tx_order`，两个方向的顺序锁不会同时持有。
- Unix stream 的用户数据复制和 `PollSet` 唤醒在 `tx_order` 外执行。send 先写入未发布的 vacant 区域，再在锁内复检关闭状态并推进 write index；recv 读取为空后在锁内复查 occupied 长度和关闭状态；shutdown 完成状态发布并复制 peer endpoint 引用后释放 `channel` mutex，再执行 waiter 唤醒。
- Unix stream 每个 endpoint 分别持有 readable、writable 和 connection-state 三组 `PollSet`。写入只唤醒 peer readable waiter，读取跨过发送缓冲低水位时只唤醒 peer writable waiter，半关闭只唤醒受影响方向，完整关闭通过 connection-state waiter 通知双方。
- rtnetlink mutation 的状态检查与 owner 更新由同一次 `SERVICE` mutex 获取串行化，netlink rx queue 和 subscriber 列表使用 `Mutex`。
- rtnetlink 每次发送携带独立的凭据快照；批量 mutation 在执行前预留完整 response 空间，避免 queue 满后只提交批次前缀。
- RX waker 由 `GeneralOptions::device_mask` 指向相关设备。`Service::register_rx_waker` 通过调用方传入的 `PollContext` 注册聚合 `timeout_poll`，调度 smoltcp timeout 和 ARP 主动重试 deadline，并向选定设备注册当前等待；Ethernet 直接把当前等待注册到 IRQ source，loopback 保存 `timeout_poll` 的聚合 waker（`Mutex<Option<Waker>>`），多任务广播由 `timeout_poll` 完成；设备层不得保存调用方的裸 waker或绕过 `Service` 注册互不等价的 task waker。

## 设计决策

### UDP 与 IPv4 数据路径迁出 smoltcp

UDP PCB、bind 冲突检查、收发队列、IPv4 校验、分片重组、输出分片和 ICMPv4 差错均由 crate 内类型管理。
TCP、raw IP、IPv6 和 DNS 继续通过 smoltcp socket set 推进，协议推进仍依赖显式 `poll_interfaces` 调用。
该边界让 UDP 不再依赖 smoltcp socket handle，同时保留现有 TCP 和 raw socket 行为。

### 数据路径持有网络配置

地址和路由由 `Router` 持有，链路属性和邻居表由对应 `NetDevice` 持有。
rtnetlink mutation 通过 `Service` 的窄接口直接修改 owner，link、address 和 route dump 分别使用
`link_snapshots`、`ipv4_addr_snapshots` 和 `route_snapshot` 读取同一份状态。
每类快照均在对应 dump 所持有的一次 `SERVICE` 锁定期间完整创建，地址快照把地址配置与当时的设备 label 配对，
避免 dump 离开 owner 临界区后重新查询易变的设备属性。
该结构移除控制面镜像及其全量同步路径，smoltcp `Interface` 只保留协议兼容所需的地址视图。
三类快照只表达各自已实现的 dump 字段，不收集尚未提供 `RTM_GETNEIGH` 的邻居表，
也不包含驱动瞬时统计和 smoltcp 内部计数。

`Router` 在地址新增和删除时重建该设备的自动路由，每个地址产生 local `/32` 路由，
每个前缀产生 connected 路由；固定 `prefsrc` 已失效的配置路由同时删除。
`Router` 最多保存 8 个 IPv4 地址，该全局容量与 smoltcp `Interface` 地址槽一致。
replace 已存在地址只确认其存在，不修改原 scope。动态选源按 route scope 过滤候选地址，
并优先选择前缀覆盖 gateway 或直连 next-hop 的地址。

当前内部路由模型没有独立 local table 和 local route type。自动 local `/32` 仍以 main table、
unicast type 和 loopback egress 表达，rtnetlink dump 对外呈现这组内部属性。
Ethernet 邻居项显式区分待探测、动态可达和永久配置状态，避免把“尚未发出请求”和“请求已在途”压缩成同一表示。

### 出口 MTU 的双层约束

UDP 和 raw IP 能在提交完整报文前取得目标地址，因此单播和定向广播按最终路由设备 MTU
检查，全局广播按全部参与设备的最小 MTU 检查，普通 IPv4 multicast socket 发送返回
`EOPNOTSUPP`。
处于 down 状态的设备不计入参与集合。Ethernet 和 loopback 在最终 TX 边界按当前设备 MTU
再次复核报文长度，覆盖报文排队后 MTU 下调的并发窗口。
smoltcp 仍以单个 `Interface` 推进 TCP 等协议，该 `Interface` 的 capability MTU 取所有设备
的最小值，形成更保守的全局上限。该限制保留跨设备共享 `Interface` 时的安全报文长度。

### rtnetlink 语义子集

路由配置只接受当前 owner 模型能够完整表达的 IPv4、main table、unicast 和单出接口语义。
source-specific route、metric、multipath 及其他会改变查找或选路结果但尚未建模的属性
返回 `EOPNOTSUPP`，避免接收后丢弃属性并产生与请求不一致的路由。
当前未发布 route multicast notification，`NETLINK_ROUTE` bind 的非零 group mask
和发送目标中的非零 pid 或 group 均返回 `EOPNOTSUPP`。

### pathname Unix socket 显式传递凭据

Linux 可从任务上下文隐式读取 `current_cred()`，但 knet 也服务于启动期内核调用者。
因此用户态入口只在操作开始时获取一次凭据快照，pathname VFS 路径继续显式接收 `&Cred`；
内核调用者通过 `bind_with_cred` 选择明确的凭据。
凭据不保存在 Unix socket、pathname lookup 状态或 dentry 中，避免凭据生命周期与路径状态耦合。
`sock_alloc_file` 也显式接收 `Arc<Cred>`，只把它交给 `VfsFile::f_cred`；socket 对象不保存
第二份 credential，也不在 knet 内部读取当前 task。

### 设备 mask 驱动 RX 唤醒

每个 socket 根据 bind 或 connect 结果记录设备 mask。
等待 RX 时只向相关设备注册 waker，同时注册 smoltcp poll timeout。
smoltcp `poll_at` 使用传入 timestamp 的同一 epoch 返回期限；兼容边界在
`SmoltcpInstant` 与 `MonotonicInstant` 之间直接映射时间点。过期的 smoltcp deadline
因此仍是过期的单调 deadline，不再计算有符号 delay，也不会经过 `as u64` 窄化。
这个设计减少无关设备中断唤醒，但依赖路由和地址同步保持 mask 准确。

### TCP listen 表独立于 smoltcp listener socket

监听 socket 和 accepted child socket 分开管理。
`ListenTable` 根据收到的 SYN 创建 child smoltcp socket，把待完成连接放入 backlog 队列。
这个设计让 POSIX accept 语义集中在 knet 内部，代价是 listen table 必须 snoop TCP 首包并清理 aborted child。

### Unix stream 按 readiness 类别隔离 waiter

Unix stream 的双向 channel 为两端保存独立的 `StreamEndpoint`，endpoint 通过 atomic 记录本地读写关闭状态、监听状态和待处理连接错误，并通过 `StreamPollSets` 分别维护 readable、writable 和 connection-state waiter。listener 的连接请求复用 endpoint readable 集合。读取事件只注册到 readable，写入事件只注册到 writable，仅请求连接状态的 waiter 注册到 connection-state，避免同一方向重复占用多个固定容量集合。数据进入 ring buffer 时只通知对端读取者，接收端读取数据使发送缓冲占用量从容量四分之一以上降到四分之一以下时通知对端写入者。连接的接收和发送方向均关闭或 endpoint 被释放时同时通知三个集合，使所有受连接状态影响的等待者重新检查 readiness。

每个 `StreamEndpoint` 还保存本端发送方向的 `tx_order`。send 只在该锁内提交 write index，shutdown 只在对应方向锁内发布关闭状态，peer recv 只在同一锁内完成空队列和 EOF 的最终判定。该顺序保证已经发布的数据先于 EOF 被观察，关闭状态先取得方向锁时后续 send 返回 `BrokenPipe`。阻塞 send 已发布部分数据后遇到关闭时返回已发送字节数。非阻塞 send 在已有发送进度时返回部分字节数，零进展且 ring buffer 已满时返回 `WouldBlock`；连接存在且双方对应方向保持打开时，零长度发送返回 0。绑定只保留 Unix 地址，`listen` 单独开启连接接收；监听端关闭读取方向后拒绝新连接，并保留已入队连接供 `accept` 取出。

### Ethernet ARP pending queue

Ethernet 设备为未解析 next-hop 保留 `pending_tx`，每个 pending 包同时保留实际 IP 源地址。
ARP request 使用对应包的源地址作为 SPA，ARP request 的 target 命中 Router 全局本机地址集合时均可回复。
设备维护逻辑按 next-hop 推进 pending 包和主动重试 deadline，一个 unresolved next-hop
不会阻塞其他已解析 next-hop。`WouldBlock`、`ResourceBusy` 等可重试 TX 失败保留原包，
硬错误只移除对应包。源地址从全局本机地址集合消失时，地址配置更新会跨设备清理对应
pending 包，避免后续使用失效 SPA。

### vsock-TIPC bridge

`vsock_tipc_bridge` feature 打开时，bridge 在 vsock device 注册后初始化端口资源：
保存 vsock device、监听桥接端口并发布反向 TIPC port。
bridge worker 不在 device 注册回调里启动；
它们由 `kruntime` 在 SMP secondary run queue 注册完成后 late-start，
以避免 early boot 阶段把 task 调度到尚未注册的远端 CPU run queue。
它不创建普通 AF_VSOCK stream endpoint，而是在 `device/vsock.rs` 的 driver event router 处优先识别桥接端口和已桥接连接。
未命中的事件继续交给原有 `VSOCK_CONN_MANAGER`，保持普通 AF_VSOCK stream 行为。

host-to-TA 方向使用固定端口映射：

- port 0：动态 service-name handshake（见下节）。
- port 1：`com.android.trusty.keymint`
- port 2：`com.android.trusty.gatekeeper`
- port 3：`com.android.trusty.vsock.forwarder`
- port 4：`com.android.trusty.widevine.transact`

#### Port 0 动态 handshake 协议

Trusty-compatible dynamic bridge 在 host 连上 vsock port 0 后按 record 语义工作：

1. Host 发送**第一个 vsock record**：UTF-8 TIPC service path（无 NUL 终止），长度不超过 `IPC_PORT_PATH_MAX`。
2. Guest bridge 对该 path 调用 `ipc_port_connect_async(..., WAIT_FOR_PORT | ASYNC)`：
   - 若 service **尚未 publish**，channel 保持 `Connecting`，**不**向 host 回包；等 TA publish 且 TIPC READY 后再继续。
   - 若 service **已存在**或随后 publish 成功，bridge 在 TIPC READY 后向 host 发送单字节状态码 `[0]`，此后双方按 record 转发 payload。
3. 下列情况 guest 发送 `[1]` 并关闭 vsock（**明确拒绝**，不同于“等待 publish”）：
   - service name 非法（UTF-8 / 长度 / 内容校验失败）；
   - 读取 service name record 失败；
   - TIPC channel 在 `TipcConnecting` 阶段收到 `HUP` / `ERROR`（例如 port 存在但拒绝连接）。
4. Host 侧（`libtrusty`）在发出 service name 后阻塞读取状态字节；应设置 recv 超时（`TRUSTY_VSOCK_TIMEOUT_SEC`，默认 60s）。超时或 EOF 视为连接失败。收到 `[1]` 映射为 `-EIO`。
5. 若 service **永远不存在**，guest 会一直等待 publish，host 在 recv 超时后失败——负例测试依赖该超时，而非 `[1]`。

固定端口（1–4）在 vsock 连接建立时即 `WAIT_FOR_PORT | ASYNC` 连到预置 TIPC service，不使用上述单字节 handshake。

TA-to-host 方向发布 `com.android.trusty.vsock.forwarder` TIPC port，并把 accepted channel 连接到 host CID 2 port 0。
bridge worker 直接使用 `VsockDevice` 的 `listen/connect/recv/send/disconnect/abort`，并直接复用 TIPC core 的 `IpcChan`、`IpcPort` 和 `HandleSet`。

vsock `Received(conn_id, len)` 事件由 bridge 以 record 语义处理：
bridge 分配 `len` 大小的临时 buffer 并调用一次 `recv(conn_id, &mut buf[..len])`，然后把该 record 作为一个 TIPC message。
如果 TIPC send 返回 `WouldBlock`，bridge 只保留一条 pending record 并等待 `SEND_UNBLOCKED` 后重试。
TIPC 到 vsock 方向按 `get_msg -> read_msg -> put_msg -> send` 转发；
v1 不排队等待 vsock credit，send 失败会关闭连接。
bridge v1 只转发 bytes，不转发 TIPC handles 或 memrefs。

**事件路由约束**：`route_event` 对 `ConnectionRequest`（mapped port）立即记入 `pending_inbound`，使同批 `Received`（port 0 service name）在 `BridgeConnection` 创建前也能入 bridge FIFO。**不得**仅凭 `bridge_mapping(local_port)` 认领所有 `Connected`/`Received`，否则 guest 普通 AF_VSOCK 若显式 `bind(1..4)` 再 `connect()`，事件会被 bridge 误消费。

`vsock-poll` 可能在同一批 `poll_event` 中连续弹出 `ConnectionRequest` 与 `Received`；若丢弃 `Received`，动态 handshake 的 service-name record 永久丢失，host `tipc_connect` 会在 status byte 上超时（`EAGAIN` / `-11`）。

`bind()` 在 `vsock_tipc_bridge` 开启时拒绝显式绑定 `BRIDGE_PORT_MAP` 端口（ephemeral 分配本就跳过这些端口）。

反向（TIPC→vsock）连接使用 `allocate_reverse_port()`，不在 `BRIDGE_PORT_MAP` 中；`accept_reverse_tipc()` 在 `dev.connect()` 之前插入 `BridgeConnection`，`has_connection()` 为真时生命周期事件仍走 bridge。

## Drop / 资源释放

- `SocketSetWrapper::remove` 按 smoltcp handle 移除 socket。
- TCP listener 关闭时，`ListenTable::unlisten` 标记 entry closed，drain child handles，并从 `SOCKET_SET` 删除。
- Unix stream listener 在 `Drop` 中清空 bind slot，释放 pending connection request。
- Unix stream channel 被 `Option<Channel>` 持有。shutdown 在对应方向锁内更新 endpoint 的读写关闭状态并唤醒受影响方向。`Channel::drop` 在释放 ring producer 和 consumer 前发布双向关闭状态并唤醒双方 connection-state waiter；peer 的 `recv` 先消费已发布数据，缓冲区耗尽后返回 EOF，`poll` 报告 `RDHUP`。关闭端丢弃自身接收队列中的未读数据时，对端记录一次 `ConnectionReset`，由 `recv` 或 `SO_ERROR` 消费。
- UDP socket Drop 时从 PCB registry 注销，PCB 销毁时释放接收队列与异步错误队列。
- Ethernet RX buffer 在 `poll_rx` 完成 frame 处理后调用 driver `recycle_rx` 归还。
