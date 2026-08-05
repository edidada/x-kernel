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
    ├── poller.rs
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
    │   ├── ingress.rs
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
│  ├─ IngressProcessor     │
│  └─ reusable batches     │
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
| `init_network` | 创建 loopback 与首个 Ethernet 设备，建立默认路由，初始化网络全局状态，并启动后台 poller 与 Ethernet RX 任务 |
| `poller` | 使用单个 `AtomicU8` 四态状态机串行化执行者，按后台预算或 socket assist 预算推进 RX、smoltcp timer 和 TX |
| `Service` | 持有加锁的 smoltcp `Interface`、`Router`、`IngressProcessor`、可复用 batch、poll timeout 和 RX waker 注册入口 |
| `Router` | 管理设备列表、RX 设备游标、带 MTU 的路由表、smoltcp ingress queue、control/data TX queue、IPv4 输出分片、next-hop 选择和 budget dispatch |
| `IngressProcessor` | 在 Router 锁外校验和重组 IPv4 输入，执行 crate 内 UDP 分流、UDP error 与 TCP listen snoop，并生成 ICMPv4 control packet |
| `NetDevice` | 抽象 loopback、Ethernet 和 feature gated vsock 设备后端，通过 `PacketBuf` 转移报文所有权，并使用 crate 内地址类型和 `MonotonicInstant` 表达设备边界 |
| `PacketBuf` | 保存报文数据、协议偏移、接口索引、包类型、校验状态和当前所有者 |
| `link::wire` | 使用 `zerocopy` 校验和构造 Ethernet 与 Ethernet/IPv4 ARP 头部 |
| `stack::ipv4` | 使用 `etherparse` 校验和构造 IPv4 与 ICMPv4 头部，并执行输出分片 |
| `stack::fragment` | 按源地址、目的地址、标识、协议和接口重组本地 IPv4 分片，并限制队列数量、内存和存活时间 |
| `transport::udp` | 维护 UDP PCB registry、接收队列、bind 与 connect 状态、校验和、socket option、异步错误和 IPv4 收发 |
| `SocketSetWrapper` | 串行化 smoltcp socket set 与 TCP deferred-close 元数据访问，并在新增 socket 时通知等待者 |
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
  和 netlink 初始状态。`knet-poller` 在这些对象完成初始化后启动，
  Ethernet RX 任务随后启动。
- **普通协议推进不应在硬中断上下文执行**：
  `poll_interfaces`、socket send/recv/connect/accept、
  netlink mutation 等路径会获取 `Mutex` / `RwLock`
  并推进较重的数据路径，应在普通任务上下文中运行。
- **IRQ 路径只做通知**：
  设备中断回调应只负责唤醒等待者或登记事件，
  不应直接执行完整的协议推进或阻塞式 socket 语义；
  Ethernet IRQ 任务只调用 `notify(PollReason::Rx)`，数据面在普通任务上下文推进。
- **允许阻塞的路径依赖 poll/waker 语义**：
  阻塞式 socket 操作依赖 `PollSet`、waker
  和 timeout 注册机制。`Pollable::register` 只能通过调用方提供的
  `PollContext` 注册源；调用方必须让对应 `PollRegistrations`
  跨 `Pending` 存活，并在注册后复查 readiness，以同时保证取消清理
  和关闭 check/register 竞态。
- **不要求固定当前进程线程才能访问全局状态**：
  `SERVICE`、`SOCKET_SET` 和 `ROUTE_STATE`
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

### rtnetlink 控制面状态

```text
build_initial_state
        │
        v
ROUTE_STATE
        │ RTM_NEWLINK / RTM_NEWADDR / RTM_NEWROUTE / RTM_NEWNEIGH
        v
updated ROUTE_STATE ──sync_netlink──> Service / Router / Interface / devices
        │
        └─RTM_GETLINK / RTM_GETADDR / RTM_GETROUTE──> netlink dump
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| 初始 state | `ROUTE_STATE` | `init_network` 调用 `build_initial_state` 与 `init_route_state` |
| `ROUTE_STATE` | 更新后的 `ROUTE_STATE` | `apply_newlink`、`apply_newaddr`、`apply_newroute`、`apply_newneigh` |
| 更新后的 `ROUTE_STATE` | data-plane 同步 | `update_route_state` 调用 `SERVICE.sync_netlink` |

### NAPI 风格推进状态

```text
IDLE ──notify──────────────> SCHEDULED
  ^                              │
  │                              │ acquire
  │                              v
  └──finish without work────── RUNNING ──notify──> RUNNING_PENDING
                                 │                       │
                                 └──finish with work─────┴──finish──> SCHEDULED
```

| 当前状态 | 操作 | 下一状态 | 唤醒行为 |
|----------|------|----------|----------|
| `IDLE` | `notify` | `SCHEDULED` | 唤醒后台任务 |
| `SCHEDULED` | `notify` | `SCHEDULED` | 已有唤醒保持有效 |
| `SCHEDULED` | 后台或 assist 获取执行权 | `RUNNING` | 当前执行者运行一轮或一批有界轮次 |
| `RUNNING` | `notify` | `RUNNING_PENDING` | 当前执行者完成时负责后续唤醒 |
| `RUNNING_PENDING` | `notify` | `RUNNING_PENDING` | pending 状态保持 |
| `RUNNING` | 完成且没有立即工作 | `IDLE` | 无唤醒 |
| `RUNNING` | 完成且仍有立即工作 | `SCHEDULED` | 唤醒后台任务进入下一轮 |
| `RUNNING_PENDING` | 完成 | `SCHEDULED` | 唤醒后台任务进入下一轮 |

`SCHEDULED` 表示已发布待推进工作，`RUNNING` 表示唯一执行者持有执行权，
`RUNNING_PENDING` 记录执行期间到达的新通知。完成路径通过一次 CAS 同时归还执行权
并发布 `IDLE` 或 `SCHEDULED`，通知与完成并发时由同一原子修改序列保留事件。
`Interface::poll_at` 返回的下一次协议 deadline 由 `Service` 注册为 timer，
到期后调度 poller 并唤醒正在等待 socket readiness 的任务。

## 算法流程

### 网络初始化

1. `init_network` 创建 `Router`。
2. 添加 `LoopbackDevice`，注册 `127.0.0.1/8` 路由。
3. 从 `DeviceContainer<NetDevice>` 取首个 NIC，包装成 `EthernetDevice`，注册默认 IPv4 路由。
4. 创建 `Service`，初始化内部 `Interface`、`Router`、`IngressProcessor` 和可复用 batch。
5. 初始化 rtnetlink 初始状态，并同步到 `Service`、`Router`、`Interface`、`IngressProcessor` 和设备。
6. 初始化全局 `SOCKET_SET`、`LISTEN_TABLE` 和 UDP PCB 及异步错误 registry。
7. 启动常驻 `knet-poller` 后台任务。
8. 启动 Ethernet RX 任务，后续 IRQ 路径只发布 RX 工作。

### RX 推进

1. Ethernet IRQ 任务调用 `NetworkPoller::notify(PollReason::Rx)`；socket recv 和 readiness 路径通过 `poll_interfaces` 尝试执行一次已有工作；TCP、UDP 和 raw TX 生产路径通过 `notify(PollReason::Tx)` 发布工作。
2. `IDLE` 上的首次通知进入 `SCHEDULED` 并唤醒后台任务；执行期间的通知进入 `RUNNING_PENDING`；后台任务和 assist 只有一个能够通过 `SCHEDULED` 到 `RUNNING` 的 CAS。
3. `Router::drain_rx_budgeted_into` 按 RX budget 从设备轮转拉取 `PacketBuf`，`next_rx_device` 在设备间保持公平并随设备删除修正。
4. `IngressProcessor` 在 Router 锁外校验 IPv4 头、长度、校验和与本地目的地址，并完成 IPv4 分片重组。
5. 完整 UDP 数据报进入 crate 内 UDP PCB 分流，未命中 socket 的单播 UDP 触发 ICMPv4 Port Unreachable。UDP PCB 按 FIFO 保留已入队数据报，`connect` 只影响后续入包的 peer 匹配。
6. TCP、raw IP、ICMP 和 IPv6 packet 进入有界 smoltcp ingress queue，`Interface::poll` 驱动兼容期协议 socket。
7. `ListenTable::wake_touched_acceptors` 唤醒 accept 等待者。
8. `Router::dispatch_budgeted` 先处理 control TX queue，再处理 data TX queue，单轮发送数量受 TX budget 限制。
9. `Interface::poll_at` 与 orphan `FIN_WAIT_2` deadline 共同决定下一次网络 deadline，`Service` 将其写入原子 deadline；每次系统 tick 检查该值，到期后通过 `notify(PollReason::Timer)` 发布工作。尚未到期的 deadline 不进入 `PollProgress::has_more`。
10. `PollProgress::has_more` 只汇报当前可立即处理的 RX、ingress、TX 或已到期 timer 工作。后台 Future poll 在一次获得执行权后连续调用有限轮次的 `Service::poll_budgeted`；达到后台轮数上限或没有立即工作时归还执行权，仍有 backlog 时通过 `SCHEDULED` 状态唤醒下一批。批次执行期间保持 `RUNNING` 或 `RUNNING_PENDING`，assist 无法获取执行权。

### TX 路由

1. TCP connect、send、receive window update 和 close，raw send 以及 UDP data queue 提交完成后发布真实 TX 通知；调用方可随后通过 `assist_once` 协助执行已调度的一轮。TCP 文件关闭不会立即删除仍处于协议关闭过程中的 handle。
2. crate 内 UDP 在 Router mutex 内检查队列容量、选择源地址和路由 MTU，写入 UDP 与 IPv4 头后提交到 data TX queue。smoltcp 生成的 IP 包进入同一 data TX queue，`IngressProcessor` 生成的 ICMPv4 错误进入 control TX queue。
3. `IP_MTU_DISCOVER` 决定 UDP IPv4 头部的 DF 标志。超出路由 MTU 且允许分片的包由 `fragment_output_packet` 拆分，DF 包返回 `EMSGSIZE`。
4. `Router::dispatch_budgeted` 优先接管 control packet，再按 IP 版本解析目的地址；IPv4 输出先按实际长度更新 `total_len` 和头部校验和。
5. 广播或组播包复制到所有设备。
6. 单播包通过 `RouteTable::lookup` 选择最长前缀路由。
7. Ethernet 设备先查 ARP neighbor cache，命中后通过 `link::wire` 封装 Ethernet frame。
8. 未命中时发送 ARP request，并把 IP 包放入 `pending_tx` 等待 neighbor 解析。

当前实现保留 smoltcp TCP、raw socket 和 IPv6 推进。UDP、IPv4 分片重组、输出分片和 ICMPv4 UDP 差错报告使用 crate 内实现。
`PacketBuf`、`link::wire` 和 `stack::ipv4` 提供设备与 crate 内 IPv4/UDP 数据路径共用的报文表示和 parser/emitter。
路由表和设备接口使用 `crate::ip` 地址类型，设备时间使用 `ktime::MonotonicInstant`。
`Router`、`Service` 和初始化入口在 smoltcp 兼容边界完成地址与时间转换。

### TCP listen 和 accept

1. `TcpSocket::listen` 把本地 endpoint 注册到 `ListenTable`。
2. `IngressProcessor` snoop 首个 SYN 包，并调用 `ListenTable::incoming_tcp_packet`。
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

1. `NetlinkSocket::send` 把用户请求交给 `handle_route_request`。
2. `NlMsgHeader::read` 和各类 `parse_*` helper 校验 netlink header 与 attribute。
3. dump 请求从 `ROUTE_STATE` 生成 multi-part response。
4. mutation 请求构造新的 `RtnetlinkState`，失败时返回 netlink error。
5. 成功更新后同步到 `Service`，带 `NLM_F_ACK` 时返回 ack。

## 并发模型

- 全局 `SERVICE` 是 `LazyInit<Service>`。`Service` 内部分别使用 mutex 保护 smoltcp `Interface`、`Router`、`IngressProcessor` 和可复用 batch。网络 timer deadline 使用 `AtomicU64` 保存，系统 tick 回调只执行原子检查、waiter 唤醒和 poller 通知。
- `NetworkPoller` 使用一个 `AtomicU8` 保存 `IDLE`、`SCHEDULED`、`RUNNING` 和 `RUNNING_PENDING`。后台预算为 512 个 RX packet、256 个 TX packet 和 32 个 timer event，socket assist 预算为 16、16 和 8。每次后台 Future poll 最多连续执行四轮，每次 assist 最多执行一轮；达到批次上限后唤醒下一批并归还执行权。
- RX 设备拉取和 TX dispatch 位于 Router 锁内，IPv4 校验、过滤和 snoop 位于 Router 锁外。smoltcp ingress queue 有固定容量，队列满时先推进 Interface 和 TX，再继续拉取设备 packet。
- 全局 `SOCKET_SET` 内部使用 `Mutex<SocketSetState>`，其中的 smoltcp socket set 与 TCP deferred-close 元数据共享同一所有权锁。关闭、登记、协议推进和回收不会跨两个 mutex 交接 handle。
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
- netlink `ROUTE_STATE` 使用 `RwLock`，rx queue 和 subscriber 列表使用 `Mutex`。
- RX waker 由 `GeneralOptions::device_mask` 指向相关设备。`Service::register_rx_waker` 通过同一个 `PollContext` 把当前等待注册到 `timeout_poll` 和 Ethernet IRQ source，调用方持有的 `PollRegistrations` 统一管理两项注册的取消与释放。loopback 只保存 `timeout_poll` 的聚合 waker，多任务广播由 `timeout_poll` 完成。每次 `Interface::poll` 也会刷新独立的协议 timer，使 TCP 重传等 deadline 不依赖阻塞中的 socket waiter。

## 设计决策

### UDP 与 IPv4 数据路径迁出 smoltcp

UDP PCB、bind 冲突检查、收发队列、IPv4 校验、分片重组、输出分片和 ICMPv4 差错均由 crate 内类型管理。
TCP、raw IP、IPv6 和 DNS 继续通过 smoltcp socket set 推进，协议推进依赖后台 poller 调度和显式 `poll_interfaces` assist。
该边界让 UDP 不再依赖 smoltcp socket handle，同时保留现有 TCP 和 raw socket 行为。

### NAPI 风格预算推进

IRQ、socket TX 生产路径、协议 timer 和后台任务共享同一个 `NetworkPoller`。`notify` 使用状态低位发布 pending 工作，`SCHEDULED` 到 `RUNNING` 的 CAS 提供单执行者约束，完成 CAS 同时归还执行权并发布下一状态。执行期间到达的通知把状态变为 `RUNNING_PENDING`，完成路径据此发布 `SCHEDULED` 并唤醒后台任务。assist 在 `IDLE`、`RUNNING` 和 `RUNNING_PENDING` 上直接返回，不创建事件，也不等待执行权。

RX、TX 与 timer 使用独立预算。control TX 优先于 data TX，避免 ICMP 错误和协议控制流量长期滞后。TCP send 可写快路径向 smoltcp socket buffer 写入数据并发布 TX 通知，连续发送由后台 owner 在有界批次内完成协议推进和 TX dispatch。TCP 和 raw socket 直接向 smoltcp 注册聚合 send waker；Router data TX queue 的可用 packet slot 实际增加时，`PollProgress::tx_capacity_changed` 触发 poller TX waiter 唤醒。协议与 deferred-close deadline 写入原子值，系统 tick 到期后唤醒 socket waiter，并以 `PollReason::Timer` 通知 poller。

### 控制面状态与 data-plane 同步

rtnetlink mutation 先更新 `RtnetlinkState`，再同步到 `Service`、`Router`、smoltcp `Interface` 和 `NetDevice`。
这种结构让 netlink dump 有稳定的 ABI presentation，也让路由表和设备地址能通过同一状态来源重建。
代价是 dump 当前读取控制面状态，运行时统计、驱动瞬时状态和 smoltcp 内部计数没有完整反查。

### pathname Unix socket 显式传递凭据

Linux 可从任务上下文隐式读取 `current_cred()`，但 knet 也服务于启动期内核调用者。
因此用户态入口只在操作开始时获取一次凭据快照，pathname VFS 路径继续显式接收 `&Cred`；
内核调用者通过 `bind_with_cred` 选择明确的凭据。
凭据不保存在 Unix socket、pathname lookup 状态或 dentry 中，避免凭据生命周期与路径状态耦合。
`sock_alloc_file` 也显式接收 `Arc<Cred>`，只把它交给 `VfsFile::f_cred`；socket 对象不保存
第二份 credential，也不在 knet 内部读取当前 task。

### 设备 mask 驱动 RX 唤醒

每个 socket 根据 bind 或 connect 结果记录设备 mask。
等待 RX 时只向相关 Ethernet IRQ source 注册当前任务，同时注册 smoltcp poll timeout。
两项注册使用调用方传入的同一个 `PollContext`，生命周期由跨越 `Pending` 的
`PollRegistrations` 管理。loopback 保留单个 `timeout_poll` 源 waker，
包到达后由该 poll set 广播给当前等待任务。
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

Ethernet 设备为未解析 next-hop 保留 `pending_tx`。
同一个 next-hop 的 ARP 回复到达后，设备按队头顺序发送等待中的 IP 包。
当前队列存在 head-of-line blocking，长时间 unresolved 的 next-hop 会阻塞后续 pending 包。

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
- TCP 文件对象 Drop 时先对 smoltcp socket 发起协议关闭。已经进入 `Closed` 且没有待发送复位报文的 handle 立即删除；接收队列存在未读数据时改用 abort 并保留 handle，直到 poller 发出 RST。其余 handle 转移到 deferred-close registry，继续参与 poller 推进、payload 发送、FIN、ACK 和重传。poller 在协议进入 `Closed` 后回收 handle。脱离文件对象引用的连接进入 `FIN_WAIT_2` 后启动 60 秒回收期限，并把该 deadline 合并到协议 poll timer；其他关闭状态不附加统一期限。
- TCP listener 关闭时，`ListenTable::unlisten` 标记 entry closed，drain child handles，并从 `SOCKET_SET` 删除。
- Unix stream listener 在 `Drop` 中清空 bind slot，释放 pending connection request。
- Unix stream channel 被 `Option<Channel>` 持有。shutdown 在对应方向锁内更新 endpoint 的读写关闭状态并唤醒受影响方向。`Channel::drop` 在释放 ring producer 和 consumer 前发布双向关闭状态并唤醒双方 connection-state waiter；peer 的 `recv` 先消费已发布数据，缓冲区耗尽后返回 EOF，`poll` 报告 `RDHUP`。关闭端丢弃自身接收队列中的未读数据时，对端记录一次 `ConnectionReset`，由 `recv` 或 `SO_ERROR` 消费。
- UDP socket Drop 时从 PCB registry 注销，PCB 销毁时释放接收队列与异步错误队列。
- Ethernet RX buffer 在 `poll_rx` 完成 frame 处理后调用 driver `recycle_rx` 归还。
