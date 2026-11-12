# ktime - Security and Reliability Analysis

## Trust Model

`ktime` trusts `khal` to provide monotonic, stable hardware clock readings, and trusts callers to
complete privilege checks at the syscall boundary. Callers of sleep compensation must also confirm the sleep boundary, handle the underlying
MONOTONIC baseline, and guarantee single settlement. `set_realtime_checked` has a built-in wall-clock range check (see External Boundary),
so the syscall layer is no longer responsible for the CLOCK_MONOTONIC constraint. The RTC driver only provides samples and does not modify
timekeeper state directly.

## External Boundary / Attack Surface

The crate does not access user memory, networks or filesystems. Its RTC
adapter invokes the trusted `PersistentClock` provider, which may access device
registers or perform sleepable I/O. The PM caller must restore device access
and obey the provider's execution-context requirements; sampling runs outside
timekeeper and observer locks. Other external inputs enter through
the `SystemTime` sample passed to `initialize_realtime`/`set_realtime_checked`,
and the `TimeSpan` passed to `inject_sleeptime`;
`SystemTime` construction itself normalizes and validates component ranges. `set_realtime_checked` performs a
built-in range check on the sample: the wall clock must not be earlier than the current `CLOCK_MONOTONIC` value, nor
exceed the upper bound `MAX_SETTABLE_UNIX_SECONDS` (Linux `KTIME_SEC_MAX`); on validation failure it
returns a `RealtimeOutOfRange` error and applies no partial modification.

`TimeSpan` excludes negative increments by type; before injection, the representable range of the accumulated sleep value and BOOTTIME is validated,
as are the REALTIME addition and the existing wall-clock upper bound. On failure it returns
`SleepTimeError::OutOfRange` with no partial modification. When the monotonic reading is earlier than the associated baseline, it returns
`SleepTimeError::InvalidMonotonicBaseline`, so callers can distinguish range errors from underlying baseline anomalies. This interface is not exposed directly to user space,
and is not responsible for authenticating RTC samples, detecting real suspend, or deduplication.

## Memory Safety Invariants

There is no `unsafe` code (`#![deny(unsafe_code)]`). The state consists of ordinary Rust values, and
`SpinRwNoIrq` guarantees mutually exclusive access.

## Thread Safety

The association of `TIMEKEEPER` and the accumulated sleep time are protected by the same read-write lock, and can be accessed safely from multiple CPUs and IRQ context;
the atomic flag `REALTIME_INITIALIZED` guarantees that initialization takes effect only once.

`realtime_and_monotonic` returns owned values rather than a lock guard. Sampling and the association conversion both occur in the same
read critical section, so a later wall-clock update cannot change the value pair already cached. This read-only interface adds no authorization
or user-memory boundary, and does not write global time.

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | Unprivileged process modifies wall-clock time | High | Direct call to `set_realtime_checked` | The syscall layer requires effective `CAP_SYS_TIME`; `set_realtime_checked` performs range validation (the wall clock must not precede `CLOCK_MONOTONIC` or exceed `MAX_SETTABLE_UNIX_SECONDS`) and returns `RealtimeOutOfRange`, which the syscall layer maps to `EINVAL` |
| T-02 | 初始化输入超范围 | 中 | 越界 SystemTime sample | `SystemTime` 构造规范化分量；读取路径 checked/saturating 兜底 |
| T-03 | IRQ 上下文死锁 | 高 | 中断读取 realtime 时写者持锁 | 读写均使用 `SpinRwNoIrq`（关中断），写临界区极短 |
| T-04 | 墙钟回拨导致负 duration | 中 | realtime 被设置到过去 | `duration_since` 返回错误而非下溢，deadline 换算按已过期处理 |
| T-05 | 重复注入或注入已计入 MONOTONIC 的时间 | 高 | PM 调用方违反结算契约 | 调用方维护休眠事务和来源选择；此阶段未接入生产路径 |
| T-06 | 补偿溢出导致部分状态更新 | 高 | 过大 delta 或累计值 | 所有计算成功后才提交两个状态；失败保持原值 |
| T-07 | 时间更新后仍使用旧 deadline | 高 | 未来调用方未处理 timer 恢复 | 状态提交时发布 generation，解锁后唤醒；消费者保留原域截止时间并重新判断 |

## Audit Checklist

- Whether new write paths go through privilege and range checks.
- Whether lock guards remain IRQ-safe (sleeping while holding a lock is not allowed).
- Whether realtime and monotonic conversions always use the same snapshot.
- Whether the sleep compensation source is trusted, has been excluded from MONOTONIC, and is settled only once.
- Whether the order of timer notification and recovery execution satisfies the calling contract.

## Concurrency and Resource Contract for Clock Notification

- The notification source is initialized only on the task subscription path; the time write path must not trigger the first allocation.
- Under the TIMEKEEPER lock, only the generation is published and wake sources are cloned; wakers are invoked outside the lock.
- Wait registrations are managed through PollRegistrations and cleaned up on cancellation; allocation failure returns an error.
- Generation checks before and after registration prevent missing an already published change; the time predicate is still checked after wakeup.
- The injection source and exactly-once settlement are still guaranteed by the future PM caller; notification cannot prove real suspend.

## Persistent Observer Constraints

Observer callbacks may overlap, and may run in an order different from the commit order. Consumers must use the committed snapshot
and reject updates older than the currently armed one. `ClockChangeId` uses half-range sequence number comparison,
and a single live arming must not span 2^63 updates.
No time lock or registry lock is held during callbacks. Observer callbacks and final destruction must not sleep or allocate;
timerfd retains the runtime through pre-prepared work captures, and captured resources are reclaimed later in task context.

Insertion under the IRQ lock requires the Arc to be unique and the Vec to have spare capacity; this path neither grows nor cleans up slots.
Copying, geometric growth, cleanup, and freeing the old table are all performed outside that lock.
A notification snapshot that has already been taken forces registration to use copy-on-write, and the snapshot remains unchanged during registration.
A separate regression check verifies that a new registration does not change a table already obtained, but it does not replace real concurrent IRQ verification.

## Suspend Settlement Credential and Resume Boundary

The transaction identifier, the current epoch, and the time offsets are serialized under the same time lock. The credential fields are private, and no
constructor or integer conversion is provided. Duplicate, cancelled, or old-epoch credentials must not be committed; identifiers do not wrap around and get reused. Resume validates all arithmetic
on a copy before committing; failure must not consume a valid credential or modify any clock. A zero commit sends no notification.
A nonzero legacy-style injection is rejected while a transaction is active, to avoid bypassing that epoch's settlement; wall-clock setting is still allowed and is not overwritten by resume.
All notifications run after the time lock is released, following the existing IRQ callback contract. No lock spans the suspend, and there is no first allocation.

The credential only provides in-kernel settlement ownership; it does not verify actual suspend, RTC stability, or the authenticity of host events.
The caller is responsible for mutually excluding PM flows, restoring the MONOTONIC baseline, and excluding sleep time; this layer only rejects resume samples earlier than
the start snapshot or the current wall-clock association baseline, and cannot identify seemingly normal forward counter jumps.
Dropping the credential automatically cancels only if the active identifier of the issuing Timekeeper still matches; it triggers no time compensation or notification,
and does not affect subsequent transactions. Destruction takes the time write lock, and must not be executed while holding that lock or in an NMI; it neither allocates nor invokes callbacks.
A normal error return can restore settlement ownership; explicitly bypassing destruction with mem::forget is unsupported, and the PM owner must
separately revoke its own hardware preparation.
Unit tests with an independent Timekeeper do not modify global time, and do not replace real PM end-to-end verification.

## RTC fallback boundary and failure handling

`src/rtc.rs` adds no unsafe, device pointers or userspace entry. It calls a
trusted `PersistentClock` provider outside timekeeper locks and checks sample
latency, monotonic ordering, resolution consistency, RTC backsteps and the
owner's maximum accepted RTC compensation. The provider/device remains an
external time authority: a plausible forward edit within the bound is not
independently authenticated. PM must establish cross-suspend continuity and
prevent RTC writes; source trust is not inferred from a successful read.

| ID | Threat | Impact | Control and remaining obligation |
|---|---|---|---|
| RTC-01 | Duplicate sleep injection from multiple sources | Medium | A session privately owns one generic SuspendToken. Preferred-source success consumes it before any RTC fallback; untracked nonzero injection is rejected while active. Separate later transactions still require correct PM ownership. |
| RTC-02 | Quantization creates false or accumulating compensation | Medium | Cross-cycle BOOTTIME/RTC phase correction, bounded sample windows and a precision-scaled reference reset; errors below declared precision remain unavoidable. |
| RTC-03 | Abort before actual freeze injects rounding noise | Medium | PM must explicitly mark the reached boundary; estimation rejects unmarked sessions. PM still has to make that assertion truthfully. |
| RTC-04 | Unstable/stopped/edited RTC gives wrong elapsed time | Medium | Failed reads, backsteps, excessive deficits and excessive positive deltas fail without settlement. Plausible changes remain a source-trust risk. |
| RTC-05 | I/O stalls or reenters under an IRQ lock | Medium | Sampling is outside locks in sleepable context. The latency budget is checked after return and cannot recover a nonreturning provider; backend access and progress are PM obligations. |

Failed estimates/commits leave accounting active for retry or cancellation and
retain the previously committed phase. Explicit cancel and unfinished Drop
invalidate phase; token Drop cancels accounting only. No automatic hardware
recovery occurs. Lost/forgotten sessions can strand accounting, as for #1093;
PM must retain ownership and must not forget them. Type borrowing keeps one
source fixed through each session. No manual Send/Sync implementations or new
global locks are introduced. Tests use isolated state rather than global time
injection and do not establish real S3, MMIO mapping or NMI safety. No user
private data is processed by the adapter.
