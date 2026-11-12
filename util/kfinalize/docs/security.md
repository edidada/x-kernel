# kfinalize — Security and Reliability Analysis

## Payload Allocation Failure Boundary

Payload storage is reserved with NOWAIT after provider preparation and before
moving the input. Reservation failure returns NoMemory with the original T,
releases the unused carrier, and publishes no identity or Finalization. A private
fault-injection test verifies this rollback and that provider rejection prevents
reservation. These tests do not force global allocator OOM.

Only KBox::into_raw pointers are paired with KBox::from_raw. Header initialization
precedes public access, and the worker never reads the retired identity header.
The carrier and identity allocations still follow kernel fatal-OOM policy;
task-owner preparation still has no recoverable allocation-error channel. NOWAIT
does not make provider code, metadata release or T::drop NMI-safe, nor relax the
existing sleepable/integration construction requirements.

## Trust Model and External Boundaries

The input is a kernel Rust object T: Send + Sync + 'static; it does not directly accept user pointers, MMIO, DMA, firmware or network bytes. If T contains raw hardware/RCU accesses, the owning module must first end those accesses that are not counted as references; a reference count reaching zero cannot substitute for a GP or for stopping the device. The provider is trusted kernel integration code, not an executor that users can inject.

## unsafe Inventory and Memory-Safety Invariants

- `lib.rs::Identity` Send/Sync：标准 Arc 保证共享访问阶段；T 的 Send/Sync 允许跨 CPU 共享及最终移交。
- `DeferredArc::deref`: public strong ownership protects initialized stable KBox storage; no mutable reference is exposed.
- `DeferredArc::from_raw/borrow_raw`：原始分配 provenance、未消费的强引用和初始化后不可变的 payload 头部共同保证恢复；borrow_raw 视图不得消耗计数，真实 owner 必须覆盖全部借用。
- `Identity::drop`：仅一次提交，所有公开 payload 借用已结束。指针及 destructor 类型配对由构造器维护。
- `destroy_value`: the unique Finalization recovers the original KBox owner, dropping and freeing it exactly once.
- `service::Finalization::finish`：调用者保证普通可睡眠 worker 上下文，且不在发布调用内执行。token 不可克隆；丢弃它只泄漏，不错误内联析构。
- `service::Prepared` / `install_normal`：unsafe provider 负责一次交接、永久服务、发布/执行保活、无取消、无提交分配或睡眠。安装仅用于串行启动。
- `service::new_integration` / `install_integration`：主 CPU 串行安装永久 provider，各 CPU 独立满足本地分配/锁前提后可并发准备，包括已审计 NoPreempt 路径。provider 返回新鲜 carrier，未绑定时保留责任；不创建任务、不等待执行。未安装返回原输入。此 unsafe 入口不允许 hardirq/softirq/NMI 构造，普通安全构造检查不变。

## Thread Safety

The public identity is not kept alive by the background, and a Weak cannot resurrect a retired object. The end of T's shared access synchronizes with the last Drop of the standard Arc; the queue is responsible for synchronizing the write of the token to the worker. The carrier is kept alive separately, and one cannot infer from the payload that the carrier is still alive, or vice versa. The core does not expose a Finalization constructor or mutable access to the payload.

## Threat Analysis and Fault Management

| Risk | Impact | Protection and Residual Constraints |
| --- | --- | --- |
| 错误上下文构造 | 原输入提前析构 | 返回 (CreateError, T)，调用者负责处理原值 |
| Weak 在队列等待中复活 | UAF/双重释放 | 公开 Arc 与私有 carrier 分开 |
| worker 先于提交返回 | 载体 UAF | provider 必须保留独立发布者 owner |
| 错误 provider 内联执行 | 原子上下文睡眠 | unsafe 安装及 Prepared 契约；普通用户不可选择 provider |
| Clone/Weak 改变执行策略 | 错误上下文析构 | 原 carrier 固定队列，Weak 升级仍只返回 DeferredArc |
| Payload reservation OOM | Construction rejected | Return NoMemory and unchanged input; drop unused carrier without submitting |
| Carrier/identity OOM or impossible admission failure | Fatal stop/leak | Existing kernel policy; never free a token whose publication may have succeeded |
| 析构 panic | 泄漏或内核停止 | 不重试析构，不提供恢复承诺 |
| 业务等待形成共享池或同一 finalizer 的等待环 | 无进展 | 每类共享 Work 串行；析构不得等待同类其他对象回收，使用者审核间接依赖，本层不做全局死锁检测 |

## Privacy and Limitations

NMI support covers both the carrier handoff of the last strong reference and the control-block release of the last Weak.
The two have independent preparation nodes and independent lifetimes; NMI must not enter ordinary locks or the allocator.
The original allocation token is stored independently in the allocator copy, and it is forbidden to enlarge
permissions from the Arc region pointer to access/free the prefix. The provider must install the permanent memory policy before preparing objects.
NMI handoff requires this CPU's irq_work to be initialized; on failure it retains the original token and then reports a fatal error,
without destructing inline or degrading to the ordinary commit path. Accessing T in NMI still requires that the operation itself be legal.

Payload contents are not logged; business objects may be retained until the worker has a chance to run. It provides no upper bound on reclamation delay, no NMI reclamation on unsupported platforms, no making all frees asynchronous, no forced-shutdown draining, and no reference-cycle collection.

## Audit Checklist

The handoff of raw ownership adds a payload-header audit point: it is fully initialized before publication and read-only
afterward; it stores a non-owning identity pointer and an allocator copy that performs no release. The raw
owner itself retains an Arc strong reference, which guarantees that the identity is still alive at recovery. Only offset round trips
that preserve full provenance from the original into_raw pointer are allowed; constructing an owning pointer from &T or reading the header is forbidden. The number of recoveries strictly matches the number of handoffs one to one, and repeated
consumption is not allowed. The final worker does not read identity fields that may be dangling. Miri tests cover
intrusive field offset round trips, interleaved shared T accesses, high alignment, ZST, and release after NMI recovery.

- Do the last strong reference and Weak upgrade still all go through the standard Arc?
- Does the token come only from a public identity that has already terminated, and do the destructor and the allocation type match?
- Does the provider prepare only once and commit once, and preserve the execution context and storage during publication?
- Does metadata destruction still have no business sleeping behavior?
- Does a construction failure return the original input rather than implicitly dropping it?
