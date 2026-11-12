# tipc — Security and Reliability Analysis

## Trust Model

```text
core/ksyscall / tipc::syscall / kernel TIPC users
        │
        │ validated Rust values, Arc<dyn Handle>
        ▼
┌────────────────────────────────────────────┐
│ tee/tipc                                   │
│                                            │
│  global service registry                   │
│  port/channel/message state machines       │
│  memref metadata handle                    │
└────────────────────────────────────────────┘
        │
        ▼
tipc-handle / ktask / kspin / alloc
```

- `tipc::syscall` handles user pointer access, ABI record copying and syscall number dispatch for the Trusty syscall ABI. Errno mapping is provided by `tipc::error::kerror_to_tipc_errno`, called from core/ksyscall.
- Direct kernel callers are responsible for providing validated Rust values before entering the core API.
- This crate is responsible for maintaining the invariants of TIPC object lifetimes, connection state, message queue bounds, handle ownership and event notification.
- The service path, UUID, queue sizes, message contents, attached handles and memref metadata must all be treated as coming from untrusted or semi-trusted callers.
- `IpcUuid::default()` is currently used to identify a non-secure client, and all other UUIDs are treated as TA clients. Real platform identity and access policy still need to be strengthened by later TEE/NS client integration.

## External Boundaries / Attack Surface

| Boundary | Source | Form entering `tipc` | Constraints |
|------|------|---------------------|------|
| service path | syscall adapter or kernel caller | `String` / `&str` | Non-empty, must not contain NUL, length less than `IPC_PORT_PATH_MAX` |
| port queue sizing | service creator | `num_recv_bufs`, `recv_buf_size` | Non-zero, and not exceeding `IPC_CHAN_MAX_BUFS` and `IPC_CHAN_MAX_BUF_SIZE` respectively |
| connect flags | client | `IpcConnectFlags` | May contain only `WAIT_FOR_PORT` and `ASYNC` |
| port access flags | service creator | `IpcPortFlags` | May contain only `ALLOW_TA_CONNECT` and `ALLOW_NS_CONNECT` |
| message bytes | channel sender | `&[u8]` / user iovecs | The core send path rejects slices larger than the queue slot; the syscall iovec entry rejects the message with `OutOfRange` when the total iovec length exceeds the peer receive slot. |
| attached handles | channel sender | `Arc<dyn Handle>` slice | The count must not exceed `IPC_MAX_MSG_HANDLES`, and each handle must be `is_sendable()` |
| memref metadata | syscall adapter or kernel caller | address, size, protection bits | size is non-zero, address/size are page-aligned, the address range must not overflow, protection bits must be within the mask and include at least one permission bit, exec and write-only are rejected; on syscall creation, the current process VMA is required to cover the range and hold the requested READ/WRITE user permissions |

Only the `syscall` module handles user pointers; the core TIPC path does not directly handle MMIO, PIO, DMA, device tree, ACPI, FFI or inline assembly.

## Unsafe Code Inventory

`tee/tipc/src` currently contains no hand-written `unsafe` blocks or `unsafe impl`.
The `syscall` module uses the `UserRead` / `UserWrite` derive macros from `posix_types` to generate marker impls for the Trusty syscall ABI carriers.

Audit requirements that must continue to be upheld:

- Do not use raw pointers to express handle or channel ownership;
- Do not use unchecked downcasts;
- Do not store bare user addresses in the message queue;
- Do not directly establish unvalidated kernel mappings in a memref.

## Memory Safety Invariants

1. **Object lifetimes are managed by `Arc`/`Weak`**: ports, channels, handle sets and memrefs are held through `Arc<dyn Handle>` or a concrete `Arc<T>`; the registry and peer links use `Weak` so that the global table or bidirectional channel cycles cannot prevent deallocation.
2. **The registry does not own ports**: the global `PortRegistry` stores only weak ports, and a lookup must successfully upgrade before the port is used.
3. **Channel peer access requires an upgrade**: every peer access first upgrades the weak peer, and returns `NotConnected` on failure.
4. **Message slot state is protected by the queue lock**: `IpcMsgQueue` is accessed only while holding the `IpcChan::msg_queue` lock, and slot state must transition in the order `Free -> Filled -> Read -> Free`.
5. **Only a read slot can be read**: `ipc_read_msg` and `ipc_read_msg_handles` accept only slot ids in the `Read` state.
6. **Attached handles use strong references**: the message queue stores `Arc<dyn Handle>`, which is released only when the receiver `put`s the slot.
7. **Closing in the handle table first detaches from handle sets**: before closing the target handle, `tipc-handle::HandleTable::uctx_handle_remove` removes stale registrations from handle sets within the same table.
8. **A memref does not dereference addresses**: `sys_tipc_memref_create` first verifies that the caller's address range is accessible within the current process address space; `MemRef` stores only the validated address range and protection bits, and does not access that address directly.

## Thread Safety

| State | Concurrency protection | Risk control |
|------|----------|----------|
| global port registry | `SpinNoIrq<PortRegistryState>` | Serializes path publishing, lookup and waiting-list updates |
| port state and pending queue | `SpinNoIrq<IpcPortInner>` | Serializes publish/attach/accept/close accesses to the lifecycle state and the pending queue |
| channel state | `AtomicU8` | Acquire/Release publication of lifecycle changes |
| channel aux events | `AtomicU32` / `AtomicBool` | Events such as READY, HUP and SEND_UNBLOCKED are visible across threads |
| channel peer and queue | `SpinNoIrq` | Serializes access to the peer weak link and the receive queue |
| handle set entries | `tipc-handle` internal `SpinNoIrq<BTreeMap<...>>` | Serializes ctrl, poll and close |
| handle cookie | `tipc-handle` internal `AtomicUsize` | Lock-free reads and writes of the caller cookie |
| handle table | external lock held by the caller | `tipc-handle::HandleTable` is not itself a concurrent container, and is usually wrapped by the `RwLock` of the process state |

The current lock design is not suitable for long blocking or for calls from interrupt context.
If a new path uses TIPC in IRQ or early boot context, the lock and allocator assumptions must be re-audited.

## Threat Analysis

| ID | Threat description | Impact level | Trigger condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | 恶意 service path 造成 registry 混淆 | 中 | 空 path、含 NUL、超长 path | `validate_port_path` 拒绝非法 path |
| T-02 | 重复发布同名 live port | 中 | 两个服务使用同一路径 | publish 时检查 live weak port，返回 `AlreadyExists` |
| T-03 | stale weak entry 造成连接到已释放 port | 中 | port drop 后 registry 尚未清理 | lookup/publish 时必须 upgrade，stale entry 会被忽略或清理 |
| T-04 | 未授权 client 连接服务 | 高 | port flags 与 client identity 不匹配 | `ipc_port_check_access` 基于 TA/NS flags 拒绝；真实身份集成仍是已知限制 |
| T-05 | 消息过大耗尽队列或覆盖 slot | 高 | 发送方传入超过 slot 大小的数据 | `push` 检查 `data.len() <= item_sz`；syscall 入口最多复制 peer receive slot 大小 |
| T-06 | 队列满导致发送方忙等 | 中 | 接收方不 `put` slot | 返回 `WouldBlock`，释放 slot 后通知 `SEND_UNBLOCKED` |
| T-07 | handle set 持有已关闭 handle id | 中 | handle 关闭但仍注册在 handle set | `HandleTable::uctx_handle_remove` 调用 `detach_from_handle_sets` |
| T-08 | handle set 循环导致 poll 递归或死循环 | 高 | handle set 嵌套注册 | 当前直接拒绝注册 `HandleSet` |
| T-09 | memref 元数据表示越界、非页对齐或未映射地址范围 | 中 | `addr + size` 溢出、范围不满足 Trusty memref 页粒度、或 syscall 调用方试图引用未覆盖/权限不足的 VMA | `MemRef::create` 使用 `checked_add` 并要求 address/size 页对齐；`sys_tipc_memref_create` 使用当前进程地址空间校验 VMA 覆盖和 READ/WRITE 权限 |
| T-10 | 不可发送 handle 被跨 channel 传递 | 中 | 发送方附带 handle set 等对象 | `IpcMsgQueue::push` 检查 `is_sendable()` |
| T-11 | close/drop 重入导致状态不一致 | 中 | 多路径重复关闭 port/channel | `close` 设计为幂等，状态切换后通知等待者 |
| T-12 | vsock-TIPC bridge 把 object capability 透传给 host | 高 | TIPC message 附带 handle 或 memref | bridge v1 只转发 bytes，发现 attached handles 时关闭连接 |
| T-13 | host 通过动态 port 0 连接非法 TIPC service path | 中 | 首个 vsock record 为空、含 NUL、超长或非 UTF-8 | bridge 在连接 TIPC 前校验 service name，并复用 TIPC path validation |

Impact level definitions:

- High: may lead to permission bypass, incorrect cross-process exposure of object capabilities, or memory safety risks.
- Medium: may lead to IPC semantic errors, denial of service, resource leaks, or wait/poll errors.
- Low: transient query failures, event delays, or inconsistent statistics.

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `ipc_port_connect_async` returns `NotFound` | The port is not published and `WAIT_FOR_PORT` is not set | Client connection fails | TA service discovery fails | 3 | The caller chooses whether to wait for the port according to Trusty semantics |
| F-02 | A waiting client cannot attach | The client closed before the port was published, or access was denied | A single pending connection fails | The connection of that client fails | 3 | On publish, skip stale weak references; if attach fails, close the client |
| F-03 | `ipc_port_accept` returns `WouldBlock` | The pending queue is empty | Service accept fails | The server must wait for the READY event | 3 | Port poll returns READY when pending is non-empty |
| F-04 | Channel send returns `WouldBlock` | The peer receive queue is full | The current message is not sent | The sender must wait for SEND_UNBLOCKED | 3 | `put` notifies the peer when the queue changes from full to writable |
| F-05 | Message read returns `BadState` | `get` was not called first, or the slot has already been `put` | Read fails | Caller protocol error | 3 | Slot state check |
| F-06 | Handle id reuse fails | The `i32` id space is exhausted | Installing a new handle fails | The process runs out of TIPC capacity | 2 | Return `TooManyOpenFiles` after a full circular scan |
| F-07 | Handle set poll returns `NotFound` | The handle set is empty | Wait fails | The caller must re-register the target handles | 4 | Clearly distinguish an empty set from not yet ready |
| F-08 | A memref cannot be mapped into the receiver address space | Only metadata is currently stored | A memref can only carry metadata | Full shared-memory semantics are unavailable | 3 | Integrate VMM/mmap semantics later |

Severity definitions:

- 1: Fatal; may lead to memory corruption or privilege escalation.
- 2: Serious; makes the TIPC subsystem or the process capability table unavailable.
- 3: Moderate; a single connection, message, or handle operation fails.
- 4: Minor; recoverable by the caller retrying or re-registering.

## Fault Management

- Parameter errors use `KResult` to return typed errors such as `InvalidInput`, `OutOfRange`, and `NameTooLong`.
- Temporary queue unavailability uses `WouldBlock`, which the caller handles together with poll/wait.
- A poll source registration failure terminates the current wait, so it never enters `Pending` without a wakeup registration. `map_register_error` maps `NoMemory` and `IdExhausted` to `NoMemory`, and `InvalidState` to `InvalidInput`.
- Peer close or a failed peer weak upgrade returns `NotConnected`.
- A stale registry entry does not panic; the publish/connect paths ignore or clean it up.
- The close/drop paths wake waiters so that poll/wait can observe ERROR/HUP.
- `debug_assert!` is used only for internal state self-checks and is not a defense against external input.

## Privacy Analysis

`tipc` stores service paths, caller UUIDs, message bytes, strong references to attached handles, handle cookies, and memref address metadata.
This data can be observed by the peer of the same connection through the syscall layer or kernel callers.

This crate is not responsible for privacy policy beyond access control:

- Service path naming conventions are defined by upper-layer services;
- UUID authenticity is provided by the TEE/NS client integration;
- Whether a message payload contains sensitive information is constrained by the protocol between the communicating parties;
- A memref address value is currently only metadata, but it may still leak the sender's virtual address layout, so syscall/TEE policy should decide whether it may be exposed.

## Known Limitations

- `IpcUuid::default()` is temporarily treated as an NS client; real identity paths such as Trusty `is_ns_client` are not yet integrated.
- The access policy supports only the two flag classes TA/NS; the full `tipc_config` policy is not implemented.
- `MemRef` stores only the address, size, and permission bits that passed syscall VMA coverage/permission validation; it is not yet bound to a VMM object and does not support receiver mmap; `validate_mmap` only fixes the offset/size/permission validation semantics in advance.
- `HandleSet` currently forbids nesting; Trusty supports acyclic nesting.
- `HandleTable` is a process-local data structure of `tipc-handle`, but the lock is provided by the caller.
- Kernel-internal callers use the `ipc_port_*`, `IpcChan`, and `HandleSet` object APIs directly; a separate `ktipc` facade is not yet provided.
- The vsock-TIPC bridge v1 forwards only message bytes, not handles or memrefs.

## Audit Checklist

When modifying this module, verify the following:

- The rustdoc of a newly added public API describes error returns, blocking semantics, and object lifetime.
- A newly added user-input field has its length, range, and flag validation completed at the syscall adapter or at this crate's boundary.
- A newly added `unsafe` must carry a `SAFETY:` comment, and the unsafe inventory in this file must be updated.
- A newly added registry operation does not perform potentially blocking or complex cross-object operations inside the global lock.
- A newly added channel/message operation preserves the slot state machine and queue lock invariants.
- A newly added handle type clearly defines the `is_sendable`, `poll`, `close`, and `cookie` semantics.
- When adding handle set capabilities, re-audit cycles, stale handle ids, and close detach behavior.
- When adding memref/VMM binding, audit address-space permissions, lifetime, mapping revocation, and cross-process information leaks.
- When adding IRQ/early boot call sites, re-audit the allocator, scheduler, waker, and spin lock assumptions.
