# StarryOS 网络子系统：AF_UNIX SCM_RIGHTS 与命名空间集成

## 1. 对比背景：x-kernel 的 SCM_RIGHTS "补丁"

x-kernel 的 `de040ed59` 修复的是一个具体症状：AF_UNIX **stream** 传输静默丢弃
`SendOptions.ancillary`（只有 datagram 路径携带控制数据），导致 ttrpc（containerd↔shim）
经 SOCK_STREAM 传递的 socket/日志 pipe 全部丢失，表现为 `ttrpc: closed` / EOF。

tgoskits 的 AF_UNIX 实现不是"补一个分支"，而是把 Linux 的 unix_stream 语义完整建了模型：
`net/ax-net/src/unix/` 下 `dgram.rs`/`stream.rs`/`namespace.rs`/`mod.rs` 四文件，
stream 与 dgram 共享底层控制面，且带 Linux 对齐的字节标记 cmsg 引擎。

## 2. stream socket 的字节标记 cmsg（成熟度核心）

`net/ax-net/src/unix/stream.rs`

### 2.1 数据模型

```text
listening socket  →  连接入队时带附加 cmsg 批次
每批 cmsg 绑定到字节区间：end_byte = 携带该 cmsg 的 send 的第 1 字节（1-based）
接收侧 start_byte 用于消费对齐：读到 ≥ start_byte 时释放对应 cmsg
```

设计要点（文件头注释与实现共同表达）：

- cmsg 附着在**字节区间**而非单个字节上；`recv` 到达 cmsg 首个字节时交付一次，
  且 `recv` 可能停在 cmsg 边界，下一次 `recvmsg` 从下一条消息的 ancillary 开始。
- 一个 send 调用携带多个 cmsg 时按序合并；读上限会停在下一个 cmsg 边界，
  保证每调用至多消费一个批次，避免跨调用粘滞。

### 2.2 `MSG_PEEK` 的 Linux 语义

```rust
// Linux unix_stream_read_generic 在 peek 时调用 scm_fp_dup：
// peek 触达 cmsg 首字节时返回复制出的 SCM_RIGHTS fd（共享 open file description），
// 字节队列原样保留，之后的消费 recv 会再交付一次。
if let Some(dst) = options.cmsg.as_deref_mut() {
    let ready_upto = chan.rx_bytes_total.saturating_add(recv_count);
    for entry in q.iter() { if entry.start_byte > ready_upto { break; } dst.extend(...); }
}
```

即：peek 产生**重复的 fd（共享 OFD）**、不消耗数据；跟随的 recv 再次交付。

### 2.3 无 control buffer 时的丢弃

```rust
// Linux man recvmsg(2)：ancillary 只在读到首字节的那次调用交付。
// recv 消费了首字节却没给 msg_control，必须丢弃 pending cmsg，
// 否则后来的 recvmsg 会静默继承陈旧数据。
let mut dst_cmsg = options.cmsg.as_deref_mut();
if dst_cmsg.is_none() { /* 丢弃已就绪批次 */ }
```

### 2.4 SCM_RIGHTS 与对端 fd 表

- `sendmsg` 侧收集对端文件名/OFD 引用；接收侧把**复用 OFD** 的 fd 插入当前进程 fd 表——
  dgram.rs:619 明确“SCM_RIGHTS fds are cloned via Arc, sharing …”，dup 的是 Arc 引用
  而不是重新 open。

## 3. dgram 与共享控制面

`unix/dgram.rs` 与 `stream.rs` 共用 `namespace.rs` 的 BindSlot 与 mod.rs 的传输底层。
x-kernel 提到"只有 datagram 携带控制数据"——tgoskits 中 dgram 的 cmsg 能力与 stream
从同一个控制面接口出发，不存在"某一条路径漏传"的架构性缺口。

## 4. unix socket 命名空间（filesystem 钩子）

`net/ax-net/src/unix/namespace.rs`

```rust
pub trait UnixNamespace: Send + Sync {
    fn resolve(&self, path: &str) -> NetResult<Arc<BindSlot>>;
    fn bind(&self, path: &str) -> NetResult<Arc<BindSlot>>;
    fn unbind(&self, path: &str) -> NetResult<()>;
}
static UNIX_NS: OnceLock<Box<dyn UnixNamespace>> = OnceLock::new();
```

- **abstract namespace socket** 的名字在 ax-net 内部管理；**path-based** 名字委托给
  可选的 filesystem 提供者（`register_unix_namespace` 注册一次）。
- 边界划分：ax-net 只负责槽位的 resolve/create/remove，不拥有 dentry、权限、
  mount 命名空间或生命周期；无 filesystem 的内核仍可使用 unnamed/abstract socket。
- 语义对应 Linux 的 `unix_mkname`/`unix_find` 与路径回收约定；bind 槽随 transport drop 解绑。

收益：命名空间隔离天然落在内核的文件系统视图上——不同 mount/网络命名空间的
同名 path socket 互不可见（由 provider 决定视图），这是 x-kernel 没有单独建模的部分。

## 5. per-process netns 与 socket 的关联

`kernel/src/namespace/net.rs`（细节见 namespace 文档）：

- `NetNamespace { ns_id }`，初始 ns id=1，之后单调分配；
- `unshare(CLONE_NEWNET)`/`clone(CLONE_NEWNET)` 产生仅含 down loopback 的新 ns；
- 接口/路由/防火墙规则/socket 集合按 ns_id 隔离（kernel netlink 等按 id 过滤），
- 进程 `net_ns` 为 plain atomic，fork 继承。

与 x-kernel `38507b5bb`（把全局单例迁入初始命名空间、新 ns 只有 loopback）同思路，
但 tgoskits 的 `NetNamespace` 是所有 socket/route/listen 表的真实所有者，
并且 `/proc/<pid>/ns/net`（NsFd）→ `setns(CLONE_NEWNET)` 已在体系中（x-kernel 是后续提交补齐）。

## 6. Docker 链路验证对应点

| 依赖 | tgoskits 位置 | x-kernel 对应提交 |
| --- | --- | --- |
| ttrpc 经 AF_UNIX stream 传 fd | `unix/stream.rs` SCM_RIGHTS | `de040ed59` |
| containerd↔shim socket 生命周期 | `unix/mod.rs` BindSlot + `unix/namespace.rs` UnixNamespace provider（path 槽位 resolve/bind/unbind）+ Arc 共享 OFD | （无对应建模） |
| 沙箱 ns bind/setns | `/proc/pid/ns/net` + NsFd + setns | `c029425de` |
| 每容器独立网络视图 | per-process NetNamespace | `38507b5bb` |

## 7. 边界与非目标

- veth/bridge/netfilter 不在 ax-net（smoltcp 栈）范围内——Docker bridge 网络与端口发布
  仍是缺口（host 网络模式可用）。
- vsock 是独立实现（`net/ax-net/src/vsock/`），不作为业务数据通道。
- 未验证：多命名空间下 path socket 的 provider 视图切换在 `setns(CLONE_NEWNS)` 后的
  一致性（依赖 ax-fs-ng mount namespace 同步，属集成测试范畴）。