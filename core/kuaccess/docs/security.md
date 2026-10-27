# kuaccess — 安全与可靠性分析

## 信任模型

- 用户态地址、长度、字符串内容均不可信。
- `kuaccess` 负责把这些输入约束在当前进程用户地址空间内，并把失败显式返回给上层。

## 外部边界 / 攻击面

- 所有 `vm_load_string*()` 调用点。
- 所有经 `osvm::VirtMemIo` 落到 `Vm` 的读写请求。
- User atomic load/compare-exchange and their task-context fault resolution.

## unsafe 代码清单

- `user_copy(...)`：
  依赖调用前的用户地址范围检查、逐页复制边界和 exception-table fixup；普通复制
  不在异常内回填，而是在返回任务上下文后处理缺页并重试当前页。
- `user_atomic_load_u32(...)`：
  依赖 4 字节对齐与用户地址范围检查；只读取用户字，并通过 exception-table
  fixup 报告 fault。
- `user_atomic_cmpxchg_u32(...)`：
  Requires four-byte alignment, a validated user range and a writable kernel
  output slot. Exception fixup returns before any fault resolution or retry.

## 内存安全不变量

- `check_access()` 只允许访问用户空间有效区间。
- `atomic_cmpxchg_u32()` / `atomic_load_u32()` / `atomic_u32_eq()` 额外要求地址 4 字节对齐。
- `access_user_page()` resolves faults only for the current process thread,
  after checking sleepability and rejecting active exception context. Neither
  copies nor atomics may wait for I/O with IRQs or preemption disabled.
- Only `Resolved`, `Retry` and `CowConflictRetry` permit another access attempt.
  Unmapped, permission, bus, OOM, no-progress and generic failures return
  `NoAccess`. No trap callback retries a faulting instruction inside the exception.
- 普通复制允许同一页在 resolved 后继续缺页，以完成分阶段映射和写权限准备。
  每页的全部缺页处理调用共用 16 次预算，包括 resolved 和 retry-class outcome；
  显式 `NoProgress` 立即失败，预算耗尽仍不能复制则返回 `NoAccess`。
  不能把部分初始化缓冲区报告为完整成功；读写分别使用 READ/WRITE fault 权限，
  不能绕过只读映射。
- 字符串 helper 只在成功读取完整字节流后再做 UTF-8 解释。
- Atomics share the copy path's per-page fault budget. A failed assembly attempt
  never publishes its output. Compare-exchange retries both the load and the
  conditional store with the original expected value; it cannot replay a stale
  unconditional store after MM resolution.

## 线程安全

- 不持有跨线程共享可变状态。
- No thread-local access-window flag is retained. Fault resolution obtains the
  calling thread's address space; the architecture atomic operation owns the
  word update, independently of the MM lock.

## 威胁分析

- 越界地址：由 `check_access()` 拒绝。
- 缺页或未映射页：转交地址空间 typed 缺页处理；无法解析为 resolved/retry 时向上传播失败。
- 非 UTF-8 输入：映射为 `IllegalBytes`，避免把脏数据当路径或参数继续处理。

## 故障模式与影响分析（FMEA）

- Fault resolution without a sleepable process thread returns `NoAccess` before
  acquiring the MM lock; an already accessible word does not require resolution.
- 用户地址在访问过程中失效：返回 `MemError::NoAccess` 或等价 `KError`。
- An incorrect retry classification could repeat an unrecoverable fault;
  `fault_outcome_allows_retry()` centralizes that decision for both access paths.

## 故障管理

- 地址/映射问题走 `Result` 返回。
- User-fault handling reports context and access failures through `MemResult`.

## 已知限制

- 当前 helper 只覆盖字符串装载与 `u32` 原子 load/cmpxchg；复杂结构体解析仍由各调用方自行组织。
- 原子原语目前仅提供 32-bit load/cmpxchg；更大宽度或其它 RMW 操作按需再加。
