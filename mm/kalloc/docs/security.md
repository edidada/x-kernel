# kalloc — Security and Reliability Analysis

## Test Probe Boundary

The audit scope includes guest fixtures in `ut-tests/lib.rs`, `ut-tests/pcp.rs`,
`ut-tests/slab_cache.rs`, and `ut-tests/tracking.rs`, mounted in their original
modules under `cfg(unittest)`. The slab and tracking feature gates are unchanged;
test extraction does not change allocation ownership or unsafe invariants.

The unittest-only allocation probe borrows a `NoPreemptIrqSave` guard and reads
an existing per-CPU atomic counter through a published CPU area. It does not
change allocator metadata or expose mutable references. The guard prevents task
migration and ordinary local IRQ attribution during measurement; NMI attribution
and remote allocation are outside the contract. No production API is added.

## Overview

`kalloc` sits at the foundational layer of kernel memory ownership. It converts untyped virtual-address regions into
Rust heap objects, contiguous pages, and DMA32 pages, and maintains these resources through raw pointers, address arithmetic, low-level allocator
contracts, and per-CPU mutable state. Once the invariants here are broken, the impact is usually not
a local allocation failure but freelist corruption, double allocation of pages, out-of-bounds DMA, or kernel undefined behavior.

This document covers the entire crate: the central byte/page allocator,
page-level PCP, slab object caches, `PageRef`, usage accounting and tracking.
The per-CPU slab cache is one of these safety boundaries.

## Trust Model

```text
boot memory map / kernel callers / C components / DMA users
                         |
                         | region, Layout, pointer, page count, address
                         v
                    kalloc safe/unsafe API
                         |
          +--------------+---------------+
          |                              |
          v                              v
  alloc-engine contracts          per-CPU storage + IRQ model
          |                              |
          +--------------+---------------+
                         v
               allocator-owned kernel memory
```

`kalloc` trusts the following conditions:

1. The `[va, va + size)` passed in by the boot flow is mapped, writable, and exclusively owned, contains at least one complete 4 KiB page,
   and does not overlap with other regions; `va` is page-aligned, and `p2v` / `v2p` are valid for these addresses.
2. The deallocation caller uses the starting address, layout, or page count of the original allocation, frees it only once, and
   keeps the same `UsageKind`.
3. The addresses returned by `alloc-engine` satisfy the requested size, alignment, non-nullness, and unique ownership, and accept
   the deallocation that matches the allocation.
4. The per-CPU storage of the current CPU is installed; ordinary interrupt and preemption constraints are enforced as the API requires;
   NMIs do not enter the allocator.
5. The DMA caller hands the returned DMA range only to devices permitted to access that region, and the upper layer handles page attributes,
   share/unshare, and device synchronization.

`kalloc` does not verify whether an arbitrary address actually belongs to some allocator, and does not keep per-allocation metadata sufficient to
detect all wrong-layout frees, double frees, or cross-allocator frees in production.

## External Boundaries / Attack Surface

| Boundary | Input | Current handling | Residual risk |
|------|------|----------|----------|
| Boot memory | Free regions that `kruntime` filters from the firmware/boot memory description and passes in via `p2v` | The underlying buddy rejects input that is not page-aligned, is too small, exceeds the region-count limit, or overlaps an existing region; the first region establishes the page/byte allocator | Cannot verify whether the address is actually mapped, writable, belongs to free RAM, or conflicts with an owner outside the allocator; endpoint arithmetic still relies on a trusted range |
| Rust kernel callers | `Layout`, raw allocation pointer, page count, alignment, fixed address, `UsageKind` | Allocation returns `Result`; some size computations and summary usage subtraction use checked arithmetic | Some deallocation APIs that look safe still depend on the caller contract, and wrong arguments may cause UB |
| DMA / devices | Contiguous page count, alignment, virtual-to-physical address translation result | After allocation, checks that the entire physical range is below 4 GiB, and returns it if not | There is no DMA32 zone; a wrong `v2p`, range overflow, or device overreach can still cause erroneous DMA |
| Tracking/observability | enable/disable, generation range, visitor | Feature-gated, global lock, and recursion guard | Backtraces/addresses may leak the kernel layout; a visitor re-entering the tracking API may deadlock or panic |

This crate does not directly parse user addresses, does not directly access MMIO/PIO, and does not directly handle file system, network, or IPC
payloads. User workloads can indirectly drive allocation pressure; when optional tracking information is exposed by upper-layer
interfaces such as `/dev/memtrack`, the upper layer must separately establish a permission boundary.

## Unsafe Code Inventory

### U-01: Rust `GlobalAlloc` and raw allocation pointers

Location: `unsafe impl GlobalAlloc for GlobalAllocator` and `large_alloc()` in `src/lib.rs`.

Main operations:

- Construct a `NonNull<u8>` from the non-zero integer address returned by the page allocator;
- Implement the raw pointer ABI of `GlobalAlloc::alloc` / `dealloc`;
- Hand allocation failure over to `handle_alloc_error()`.

Relied-on invariants: the underlying allocator returns an address that is valid, aligned, exclusive, and covers the complete layout; the pointer/layout
received by dealloc must match the original allocation. The internal `balloc` / `palloc` locks and the
per-CPU guard protect metadata against concurrency, but cannot repair a pointer forged by the caller.

### U-02: per-CPU slab intrusive list

Location: `ObjectCache::pop()`, `push_unchecked()`, and `try_push()` in `src/slab_cache.rs`.

Main operations: cast the starting address of a cached object to `Option<NonNull<u8>>`, and read or write the next link from the first word.

Relied-on invariants:

- The object is a live, writable, exclusive allocation of the corresponding canonical class layout;
- The caller has finished all accesses, and the object is not in any other cache/freelist;
- The object size/alignment is sufficient to hold the link; a compile-time assertion verifies that the link does not exceed the smallest 8-byte class;
- Local IRQs stay disabled during list mutation, and remote CPUs do not modify this CPU's list.

The crate-private `AllocLocalOwner` holds a `NoPreemptIrqSave` and serves as a required
parameter of all PCP/slab cache operations. The cache module constructs temporary references only within the owner's mutable borrow scope and does not return references to callers;
therefore raw per-CPU mutable access is concentrated in two owner-backed helpers. This protocol does not support NMIs entering the
allocator; NMI paths must use preallocated storage.

`GlobalAllocator::alloc_from_percpu_slab()` and `dealloc()` establish an IRQ guard and route by
`SizeClass`; `with_object_cache!` keeps the enum class consistent with `ObjectCache<N>`.

### U-03: slab refill/drain ownership

Location: `ObjectCache::drain()`, `PerCpuSlabCache::{try_free,drain}`
in `src/slab_cache.rs`, and the crate-private current-CPU cache API.

Main operations: transfer raw objects among the per-CPU cache, the caller, and the central `ByteAllocator`, and call the central deallocation with the
canonical layout.

Relied-on invariants: the central allocator lock is already held by the caller; all cached objects were originally allocated by the same central
allocator with the same class layout; the current-CPU mutable reference is not re-entered on the same CPU. Violating these conditions may put an object into the wrong freelist or create a mutable alias.

### U-04: Fresh exact zeroing and reference-range transfer

Locations: alloc_zeroed_pages_exact_ref and PageRef::from_raw in src/page.rs.
Zeroing writes only the checked selected range while new, exclusive and unpublished.
Exact surplus inventory is already returned and is never written by this clear.
The single-page helper uses the same path. No safe slice API is exposed.

Raw transfer includes selected PA and count. Restoration requires the original
pair and lifetime holds for every selected independent page or one encompassing
compound. Metadata validates a homogeneous range but cannot prove provenance,
unique consumption, initialization, access exclusivity or external retirement.
A shared PageRef cannot grant an exclusive content reference. Single-page PA
adapters may omit count only under their explicit length-one allocation contract.

### U-05: per-CPU usage shared references

Location: `with_current_usage_counters()` and `usage_snapshot()` in `src/lib.rs`.

Main operations: obtain a shared reference to the per-CPU usage slot through `kcpu_slot::area()` and `PERCPU_USAGE_COUNTERS.shared_at()`, and access its atomic counters from the allocation/free hot paths or the cross-CPU aggregation path.

Relied-on invariants: while the current-CPU area is resolved, `NoPreempt` pins the executing CPU; an unpublished area is skipped (early-boot accounting is dropped, and `usage_snapshot()` skips it) rather than assumed initialized; `usage_snapshot()` iterates CPU indexes below `kbuild_config::NR_CPUS`; areas are published monotonically and never unpublished; all concurrent access goes through atomic fields and shared references, and the runtime does not concurrently reset per-CPU areas.

## Memory Safety Invariants

The following conditions must always hold:

1. 每个加入 allocator 的 region 都有页对齐 base，覆盖至少一个已映射、可写、独占的
   完整 4 KiB 页，且其实际纳入部分与既有 region 不重叠；非整页尾部不被管理。
2. 每个 allocation 在任意时刻只有一个 owner：caller、某个 per-CPU cache，或中央
   allocator freelist；不能同时处于两个状态。
3. byte heap backing page 一旦交给 `balloc`，在其整个生命周期内不得再由 `palloc`
   分配给其他用途。
4. page allocation/free 使用相同起始地址和页数；byte allocation/free 使用能映射到
   同一底层分配类别的 layout。
5. PCP 中每个地址都表示对应 `NR_PAGES` 的完整、页对齐、连续 block；3 页等非
   power-of-two block 的 buddy rounding slack 已经归还。
6. slab cache 中每个 object 都属于对应 `SizeClass`，使用 canonical layout 分配，其首
   word 只在 caller 已经 relinquish ownership 后作为 next link。
7. current-CPU cache mutation 不得与同 CPU interrupt/NMI 或 remote walker 并发。
8. Each PageRef owns a nonempty homogeneous selected range: one hold per
   independent page, or one compound hold covering the entire range. The count
   stored in the handle is its selected length, not the retained compound size.
   Content permissions and business byte lengths require separate contracts.
9. usage slot 只通过 atomic shared access 更新；per-CPU area 不得在 accounting 运行时
   被重新初始化。
11. usage/tracking metadata 只是诊断信息，不得用来替代 allocator provenance 或
    ownership 判断。

## Thread Safety

| Shared state | Synchronization condition | Cases not covered |
|----------|----------|----------------|
| `DefaultByteAllocator` | `balloc: SpinNoIrq<_>` | NMI reentrancy, incorrect raw pointer/layout |
| `BuddyPageAllocator` | `palloc: SpinNoIrq<_>` | NMI reentrancy, incorrect region/page contract |
| PCP | local IRQ off + current-CPU storage | NMI, CPU offline/remote inspection |
| slab object cache | local IRQ off + current-CPU storage | NMI, CPU offline/remote inspection |
| ready flags | Release store / Acquire load | High-level protocol errors from concurrent duplicate initialization |
| Usage of each kind | per-CPU cumulative atomics; free Release / snapshot Acquire | Cross-CPU/cross-kind linearizable snapshots, allocator metadata publication, immediate wrong-kind free detection |
| tracking map | `SpinNoIrq<GlobalState>` + per-CPU recursion guard | Recursive acquisition of tracking state by a visitor |

The tracking recursion guard uses a per-CPU `AtomicBool::swap` and acquires
`NoPreemptIrqSave` before reading the flag. RAII destruction guarantees that both the normal return and unwind paths clear the flag of the original CPU, preventing
an old plain bool from being left on the wrong CPU after migration or early exit. The cost is that IRQ/preemption stay disabled during the tracking callback, backtrace,
and tracking lock; this is a feature-gated diagnostic cost, and
tracking data must not be used as a normal runtime latency baseline.

The established nested lock order is tracking state (when enabled) → `balloc` → `palloc`. Byte heap growth is
`balloc -> palloc`; no other path may hold `palloc` and then acquire `balloc`. Usage accounting adds no
lock nesting. Ordinary IRQs are excluded through `SpinNoIrq` or an explicit `IrqSave`; NMI is not supported.

Allocator initialization is completed serially by the boot CPU. The ready atomics are used to publish the completed state; they do not turn multiple
concurrent `global_init()` / `add_memory()` calls into a safe region initialization protocol.

## Threat Analysis

| ID | Threat description | Impact level | Trigger condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | 无效、重叠或保留内存 region 被加入 allocator | 高 | boot memory 分类、`p2v` 或 caller 参数错误 | `kruntime` 只传递标记为 `FREE` 的 region；底层 buddy 检查页对齐、最小大小和内部 region 重叠。映射有效性、外部 ownership 和端点范围仍由启动边界保证 |
| T-02 | Wrong layout, page count, double free or cross-allocator free corrupts a freelist | High | A kernel caller violates the allocation/deallocation pairing | API rustdoc and RAII `PageRef` constrain normal paths; aggregate usage underflow detects some accounting errors. Immediate, complete provenance validation is absent, so the residual risk remains high. |
| T-03 | slab intrusive link 被越界写、重复插入或并发修改 | 高 | stale alias、错误 size class、double free、NMI 重入 | canonical layout、compile-time link-size check、local IRQ guard 和中央锁保护正常路径；NMI 和伪造 pointer 明确不支持 |
| T-04 | PCP 保存错误页地址或页数，随后重复分配/错误合并 | 高 | caller 用错误 `num_pages` free，或 CPU-local 并发约束被破坏 | 固定的 1–4 页 cache 类型和 `split_to_chunks` 保持内部 bookkeeping；输入 provenance 仍由 caller 保证 |
| T-05 | DMA 地址超出设备可寻址范围或指向错误物理页 | 高 | `v2p` 错误、地址区间算术异常、caller/device 生命周期错误 | `alloc_dma_pages()` 检查结束地址低于 4 GiB 并在失败时归还；上层 `kdma` 管理页属性和平台 share/release，仍依赖正确设备授权 |
| T-06 | 锁递归、锁顺序反转或 NMI 打断导致永久自旋 | 中 | allocator 在 NMI 中使用，新增路径形成 `palloc -> balloc`，或 tracking visitor 再锁 tracking state | `SpinNoIrq` 排除普通 IRQ、文档固定锁序、tracker 有 allocation recursion guard；NMI 与 tracking callback 重入仍是剩余限制 |
| T-07 | per-CPU cache 隐藏可复用内存，促使中央 allocator 提前扩容或 OOM | 中 | CPU/class 工作集不均衡、内存压力、CPU 停止运行 | PCP 固定 entry 容量；slab cache 使用最多 32 对象的 batch 和不足两个 batch 的高水位；没有全 CPU reclaim，风险只做到有界而未消除 |
| T-08 | 大小、页数、usage 或地址算术溢出 | 中 | 极端 caller 输入、错误 region 或 accounting 不配对 | `pages_to_bytes`、per-CPU 累计量和 usage 汇总 subtraction 使用 checked arithmetic，`Layout` 验证部分输入；部分地址端点计算仍依赖可信内核范围 |
| T-09 | tracking 泄露内核地址/backtrace 或显著改变时序 | 中 | 未授权读取上层 memtrack 输出，或把 tracking 性能当作正常路径 | tracking 是 feature-gated 诊断功能；访问控制由暴露接口负责，性能分析必须区分 tracking 状态 |
| T-10 | 同时选择多个 byte allocator feature，实际实现与配置认知不一致 | 低 | 非 Kconfig 构建错误启用多个 feature | 正常构建由 Kconfig 选择一种；未选择会编译失败，同时选择目前依赖 `cfg_if!` 分支顺序，需由构建审计保证 |
| T-11 | 把非线性化 usage/central stats 用于安全或 reclaim 决策 | 低 | caller 把诊断快照误当精确 ownership | API 将其定位为统计；文档区分 caller-live、byte central 和 page central 视图，未提供强一致保证 |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure mode | Failure cause | Local effect | System effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | allocator 在 page region 就绪前被调用 | 启动顺序错误或无可用 `FREE` region | debug assertion、allocation failure 或访问未初始化 allocator | 早期启动失败 | 1 | `kruntime` 在普通内核服务前初始化；页表路径通过 `is_page_allocator_ready()` 断言 |
| F-02 | byte heap 或 page buddy 无法满足请求 | 内存耗尽、碎片化或 cache residency | 显式 API 返回 `AllocError` | Rust `GlobalAlloc` 路径记录 OOM 后进入 `handle_alloc_error`，通常终止当前内核运行 | 2 | byte heap 减小扩容请求重试；PCP/slab 允许 partial refill；显式 API 向上返回错误 |
| F-03 | PCP batch 大块申请失败 | buddy 碎片化 | 无法一次取得目标 batch | 性能下降，不一定 allocation 失败 | 4 | 自动退化为逐 block refill，只有一个 block 都拿不到才返回错误 |
| F-04 | slab refill 中央 allocator 中途耗尽 | byte heap 压力或碎片化 | partial cache | 当前 allocation 优先从 partial refill 返回；完全失败时扩展 byte heap | 3 | 保留已取得对象，不回滚；外层执行 heap growth 后重试 |
| F-05 | DMA allocation 落在 4 GiB 以上 | buddy 没有低端定向策略 | 本次 DMA allocation 被拒绝 | 设备初始化或 I/O 失败，即使低端内存可能仍存在 | 3 | 立即归还不合格页并返回 `NoMemory`；后续可引入 DMA32 zone |
| F-06 | usage overflow/underflow | lifetime 累计量溢出、wrong-kind/free 配对或计数异常 | 热路径累计溢出或汇总下溢时 panic | 可能导致内核停止 | 2 | checked arithmetic fail-fast；usage 不参与 allocator metadata mutation |
| F-07 | cache full 触发 batch drain | 正常突发 free | 最多 32 次中央 deallocation 持有锁并保持 IRQ-off | IRQ latency 或跨 CPU allocation latency 尖峰 | 3 | 对象数和水位提供明确上界；仍需 workload 实测和调参 |
| F-08 | 持锁 CPU 停滞 | panic、NMI、硬件问题或错误重入 | 其他 CPU 在中央 allocator 锁上自旋 | 系统级 allocation hang | 1 | 临界区不睡眠；普通 IRQ 被关闭；NMI allocation 禁止，新增路径必须遵守锁序 |
| F-09 | tracking visitor 重入不兼容 API | visitor 调用 `current_generation()` 等再次获取 `STATE` | 自旋死锁或 `None.unwrap()` panic | 诊断接口挂起，可能影响系统 | 2 | allocation recursion 由 per-CPU flag 跳过；visitor 必须短小且不得递归进入 tracking state |
| F-10 | Invalid PageRef raw restoration | Forged, duplicated or stale transfer | Consumes a hold the caller does not own | Premature free or memory corruption | 1 | Unsafe API requires the exact outstanding transfer from the same incarnation; safe lookup cannot grant ownership |

## Fault Management

- `GlobalAllocator::{alloc,alloc_pages,alloc_dma_pages,alloc_pages_at,add_memory}` use
  `AllocResult` to report errors such as `NoMemory` and `InvalidInput`.
- Rust `GlobalAlloc::alloc` cannot silently pass an ordinary failure to most Rust allocations; it records the
  layout and central heap state and then calls `handle_alloc_error()`.
- `global_init()`, `init_page_allocator()`, and some internal invariants use assert/expect; boot or
  allocator metadata errors are handled fail-fast, and the kernel does not attempt to keep running after such errors.
- PCP and slab refill accept partial progress, to avoid discarding blocks/objects already obtained
  just because the batch target was not reached.
- When the DMA32 check fails, the unqualified allocation is first returned to the buddy, and then an error is returned.
- Per-CPU usage cumulative counters use checked addition; a total freed amount exceeding the total allocated amount fails
  fast when summarized in `usages()`, but this is not an immediate or complete pointer provenance check.
- The spinlock has no recovery protocol for allocator corruption; once an unsafe invariant is broken, it cannot be assumed that
  subsequent error returns still keep the system safe.

### Capacity Query Boundary

`total_pages()` reads the managed capacity only under the global page-allocator lock. It does not take the byte-heap lock,
and it neither publishes nor releases pages. Using it for upper-layer filesystem quota does not constitute a physical-memory reservation; actual allocations
can still fail, and the capacity value must not be treated as remaining memory or as an admission guarantee.

## Privacy Analysis

The ordinary allocator treats the payload as opaque bytes and does not actively read the caller's live data. However:

- 普通 allocation 不保证清零；之前 owner 的内容可能残留，处理敏感数据的 caller 必须
  use alloc_zeroed_page_ref/alloc_zeroed_pages_exact_ref before publishing contents;
- slab cache 会在已经 free 的 object 首 word 写入 next link，其余内容不会自动擦除；
- PCP 缓存页也不会擦除 payload；
- tracking 保存 allocation 地址和 backtrace。上层若将其暴露给用户态，可能泄露内核
  地址布局、调用关系和工作负载信息，必须限制访问并考虑地址脱敏。

This crate no longer defines any C ABI symbol. `memcpy`/`memmove`/`memset`/`memcmp` are provided as weak symbols by the `compiler_builtins` that rustc ships for the target,
and their input contract is the joint responsibility of that upstream implementation and LLVM lowering;
they are no longer part of this crate's trust boundary. Note that any strong symbol that redefines these four names
with `#[no_mangle]` silently replaces the kernel-wide page-zeroing and page-copy implementations, so adding such a symbol must be reviewed
as an architecture-level performance change.

## Known Limitations

1. **无统一 reclaim**：byte heap backing 不整体归还 page allocator；PCP 和 slab cache
   没有全 CPU freeze/flush/shrink 接口。cache 中资源只有达到本地高水位时才批量归还。
2. **无 CPU hotplug lifecycle**：runtime offline CPU 前不会 drain 其 PCP 或 slab cache。
3. **不支持 NMI**：local IRQ guard 和 `SpinNoIrq` 不能排除 NMI 重入。
4. **无 allocation context policy**：不区分 task、softirq、hardirq，也没有 GFP-like
   nonblocking/reclaim flags；slow path 可能持中央锁、批量循环并扩展 heap。
5. **无 NUMA、memory cgroup 或 pressure-aware 水位**：所有中央内存共用一个 page buddy
   和一个 byte allocator。
6. **DMA32 采用 allocation 后检查**：没有低端 zone 或定向搜索，可能出现可用低端页
   尚存但请求仍失败的情况。
7. **deallocation provenance 不做 production 验证**：错误 pointer、layout、页数、
   `UsageKind` 或 double free 主要依赖 caller contract；部分公开 safe 方法的约束无法由
   类型系统强制。
8. **统计不是强一致 ownership 视图**：per-CPU cache、byte heap backing、页对齐和
   per-CPU 累计量的分阶段读取会使 `usages()`、`used_bytes()`、`used_pages()` 的含义
   不同；`usages()` 不保证跨 CPU/跨种类线性化，并发变化可能令结果暂时偏大或偏小。
9. **tracking 有全局锁和回调约束**：会改变 allocation latency；`allocations_in()` 要求
   tracking 已启用，visitor 在锁内运行且不得递归获取 tracking state。启停边界附近的
   allocation/free 记录不构成事务快照，停用期间的 free 不会清除既有记录。
10. **Memory is not zeroed by default**: Unless the caller uses
    `alloc_zeroed_page_ref` / `alloc_zeroed_pages_exact_ref` or explicitly clears
    the allocation, reused memory can retain a previous owner's data.
11. **slab object cache 水位尚未实测调优**：batch 同时受 2 页 payload 和 32 对象限制，
    高水位不足两个 batch；需结合 X-Kernel workload 的 hit rate、lock-stat、内存保留和
    IRQ latency 调整。

## Audit Checklist

When modifying `kalloc`, confirm the following:

- [ ] 新的 memory region 入口是否验证映射、页对齐、独占性、重叠和地址端点溢出？
- [ ] allocation/deallocation 是否在所有分支保持 pointer、layout、页数和 `UsageKind`
      配对？
- [ ] byte heap backing 是否只在 `balloc` 与 `palloc` 之间转移一次 ownership？
- [ ] 新增 page fast path 是否正确处理 alignment、非 power-of-two slack 和 buddy chunk？
- [ ] 每个 current-CPU PCP/slab mutable access 是否覆盖完整 local IRQ-off/preemption-safe
      区间？是否可能从 NMI 或 remote CPU 访问？
- [ ] slab cache object 是否始终使用对应 class 的 canonical layout，且首 word 只在 caller
      relinquish 后改写？
- [ ] 新增锁路径是否遵守 tracking → `balloc` → `palloc` 顺序，且不在锁内睡眠？
- [ ] 所有 unsafe block 是否有可追溯 `SAFETY:` 说明，safe wrapper 是否真正建立其前提？
- [ ] Do page holders preserve exact transfer ownership, and do content users
      separately establish mapping, aliasing, initialization and byte bounds?
- [ ] DMA 端点计算是否 checked，整段物理范围是否满足设备限制，失败是否完整回滚？
- [ ] 是否新增了以 `#[no_mangle]` 定义 `memcpy`/`memmove`/`memset`/`memcmp` 的强符号？
      这类定义会静默替换全内核（含每次缺页的页清零与 COW 拷贝）的 mem 原语，必须按架构级
      性能改动评审并给出与 `compiler_builtins` 实现的对比数据。
- [ ] tracking visitor 是否在持锁期间调用，是否存在递归 state lock 或敏感地址泄露？
- [ ] 新增统计是否明确计量单位和层次，且没有被用作 allocator correctness/reclaim 条件？
- [ ] 新增 cache 或 allocator 状态是否同步更新初始化、CPU hotplug、reclaim 和关机策略？

## Range-holder ownership and cleanup

PageRef is two machine words: first descriptor and selected page count. Range
mode resides in immutable-while-held page-meta geometry. No safe constructor
accepts arbitrary descriptors or mixes independent pages with compound members.
Subrange bounds are relative to the handle and preserve that shape. State/count
snapshots are diagnostics, not authority to construct a holder.

Exact admission validates the full selected prefix before claiming one count
per page. Inventory is obtained as a complete power-of-two block; unused pages
have no typed holds and are returned with exact one-page operations before
publication. On ordinary failure all raw backing and its charge are restored.
Zeroing clears only selected live pages after surplus is returned. Compound
admission is separately requested and validates its full group before publication.

Clone of an independent range retains the originals while incrementing every
member. A failed increment releases exactly the acquired prefix, which cannot
retire because its originals remain held. Compound clones increment one head.
Drop snapshots the first unit: it releases that one compound hold, or creates
one temporary single-page owner for each independent hold. Earlier returns may
be reused, so no freed page is consulted to determine later range bounds.
Last retirement clears all relevant group links before returning inventory and
never writes old metadata after publication.

Exact raw restoration requires the full transferred length; the first descriptor
cannot distinguish simultaneous holders with different lengths. Compound raw
views also preserve selected bounds. PA-only single-page adapters explicitly
restore length one. Wrong lengths, duplicated/stale transfers or forged ownership
can corrupt counters and remain unsafe caller violations despite validation.

try_split of an independent range only transfers existing holds and permits
other independent holders. Compound splitting freezes exactly one head hold and
requires external quiescence. A failed split returns the original selected view;
a successful split yields the whole compound, including pages outside a subview.
The old owner is consumed without decrementation and SplitPages owns every child
until transfer or Drop. Class/accounting is preserved. mem::forget and lost raw
transfers can still leak ownership.

Metadata remains resident. Raw ordinary/DMA free still has no descriptor scan,
so its original provenance/bounds/class obligations remain caller-enforced.
Independent claim/clone/drop and compound relation setup/cleanup/split visit their
relevant ranges; IRQ callers must bound work. Lifetime protection does not establish
initialization, exclusive contents, mapping permissions, COW or hardware retirement.

## Owned normal-memory zeroing

`DC ZVA` is used only for exclusive, unpublished, normal cacheable RAM owned by
the allocator. MMIO, device memory, shared/DMA-visible buffers and arbitrary C
buffers are outside the contract. The block size comes from `DCZID_EL0`; if the
register forbids `DC ZVA` or no complete aligned block fits, the helper falls
back to `write_bytes` over exactly the requested range.
