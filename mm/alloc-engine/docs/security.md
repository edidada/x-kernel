# alloc-engine — 安全与可靠性分析

## 信任模型

`alloc-engine` 位于内核内部的 allocator 边界。它信任调用者提供的管理内存、
`Layout` 和释放配对；它不直接接收用户指针或设备输入。

`SlabCache` 是 crate-private unsafe 边界：safe `allocate` 信任已注册的 span
metadata，而 `add_slab` 与 `deallocate` 要求其唯一调用者 `SlabHeap` 维护 span
所有权、对象来源和对象生命周期。v1 不把这些 unsafe 操作暴露成新的外部 API。

```text
owning heap / page allocator
      │ validated, exclusive aligned span
      v
SlabCache metadata + free-object links
      │ live object (caller-owned)
      v
kernel subsystem
```

## 外部边界 / 攻击面

本 crate 不直接访问用户内存、MMIO/PIO、DMA buffer、FFI、内联汇编、bootloader
或 firmware 数据，也不解析文件系统、网络、IPC 输入。其主要攻击面来自内核
调用者违反 allocator contract：伪造 backing address、错误 `Layout`、重复释放、
跨 cache 释放或在未串行化的情况下并发操作。

## unsafe 代码清单

| 路径 | 依赖的不变量 | 保护方式 |
|---|---|---|
| `src/slab_cache.rs: SlabCache::add_slab` | `base` 指向独占、可写、完整且按 span 对齐的 backing allocation | unsafe contract、运行时对齐/容量/计数检查；调用者在 detach 前保持映射 |
| `src/slab_cache.rs: SlabCache::deallocate` | object 来自同一 cache，仍为 live object，且只释放一次 | unsafe contract；page header 的 class 和对象边界在 debug 构建校验 |
| `src/slab_cache.rs: page_for_object` | span 的 size 为 power of two，base 按 span 对齐 | `SlabCache::new` 验证 geometry；`add_slab` 验证 base alignment |
| `src/slab_cache.rs: read_next/write_next` | free object 的首个 machine word 可读写 intrusive link | 仅在 publish、allocate、deallocate 后的非 live 对象上调用；class 对齐覆盖 link 对齐 |
| `src/slab_cache.rs: SlabSpanList` | span 只在一个状态表中，所有 links 在同一串行化域内修改 | 私有 API、状态转换集中在 `move_span` / `take_empty_slab` |
| `src/slab_cache.rs: unsafe impl Send for SlabCache` | cache 可移动，但注册 span 在 detach 前仍映射且不发生无锁共享 | 类型只保存 span 内 metadata 指针；所有 mutation 需要 `&mut self`；刻意不实现 `Sync` |
| `src/slab_heap.rs` 与 buddy/TLSF 适配器 | 释放地址和 `Layout` 与原分配匹配 | 上层 `ByteAllocator` unsafe deallocation contract |

## 内存安全不变量

1. 一个 registered slab span 只属于一个 `SlabCache` 和一个 `SizeClass`。
2. span 大小及其 base 均为 power-of-two span 对齐，因此由 object 地址掩码恢复的
   header 正确。
3. 每个 `SlabSpan` 恰好处于 `Partial`、`Full`、`Empty` 或 `Detached` 之一；只有
   `Empty` 可以 detach。
4. `free_count` 与该 page freelist 中对象数一致；`total_blocks`、`free_blocks`
   与所有 registered page 的计数一致。
5. live object 不在 freelist 上；其首个 word 只有在释放后才能被 allocator 覆写。
6. 被 detach 或仍含 live object 的 span 不得归还给页分配器。
7. detach 后的 `SlabBacking` 代表一次回收权；该 token 不可复制，消费其地址后
   只能向 owning buddy 归还一次。

## 线程安全

`SlabCache` 不提供内部同步。它仅以受审计的 `unsafe impl Send` 支持锁保护容器
转移其独占所有权，刻意不实现 `Sync` 或允许无锁共享访问。内核上层把它放进共享
容器时，必须用单一的外部串行化规则覆盖 list、header、counter 和 free-object
link 的全部访问。`take_empty_slab` 返回后，页的释放必须不与任何仍可能访问该
cache 的路径并发。

该层没有原子、不会 sleep，也不会主动关闭 IRQ。当前 `kalloc` 集成通过一个
`SpinNoIrq<SlabByteAllocator>` 同时关闭本地 IRQ 与抢占，并在 SMP 上串行化所有
CPU；NMI 不在该保证内。这个同步方式防止同 CPU IRQ 重入和跨 CPU 数据竞争，
但不能提供 hardirq 有界延迟：refill、span 初始化和压力回收仍可能在外层
IRQ-off 临界区执行。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 伪造或错误对齐的 backing span 破坏 page header | 高 | unsafe `add_slab` 违反 contract | 对齐与容量检查；调用者必须只传页分配器返回的独占 span；残余风险是恶意内核代码可绕过 unsafe contract |
| T-02 | double free 或错误 cache/size class 释放覆写 live 数据 | 高 | unsafe `deallocate` 被误用 | API 明确要求原 cache 的 live object；debug 检查 class/范围；当前不提供 production double-free 检测 |
| T-03 | 未串行化访问损坏 intrusive list | 高 | 多 CPU 同时访问相同 cache 而没有外部锁 | `SlabCache` 不声称并发安全；整合层必须限定锁/CPU-local protocol |
| T-04 | 空 page 被错误归还导致 use-after-free | 高 | 释放仍含 live object 的 span | 仅 `Empty` 状态可 detach；partial/full page 永远不由 `take_empty_slab` 返回 |
| T-05 | 计数溢出导致错误可用性或回收判定 | 中 | 极端数量的 registered objects/pages | `add_slab` 预先使用 checked arithmetic，失败返回 `InvalidInput` |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|---|---|---|---|---|---|---|
| F-01 | cache 无可用对象 | 尚未注册 span 或全部 page 为 Full | `allocate` 返回 `None` | 上层需要 refill 或报告 OOM | 4 | 缓存层不扩页；上层在临界区外申请 backing span |
| F-02 | span 不符合 geometry | base 未按 span 对齐、header 后无对象空间 | `add_slab` 返回 `InvalidInput` | 当前请求失败，不发布坏 metadata | 3 | 构造期验证 page geometry，发布期验证 span |
| F-03 | 内部 buddy 首次分配失败 | 空 span 占页、内存压力或连续空间不足 | 当前 refill/direct allocation 暂时失败 | 可能向上返回 NoMemory | 2 | 先 detach 全部 Empty span 并向内部 buddy 归还，再有界重试一次 |
| F-04 | 内部 buddy 重试仍失败 | 没有可回收 Empty span，或仍无满足对齐的空间 | `SlabHeap` 返回 NoMemory | `kalloc` 可尝试扩展 byte heap，最终仍可能 OOM | 2 | 保留 typed error；不在 allocator 内无限 reclaim/retry |
| F-05 | live object 长期不释放 | 调用者泄漏或缓存策略保留对象 | span 不能变 Empty | 可用内存下降 | 3 | ownership 审计、统计与上层 shrink 机制 |

## 故障管理

geometry 或计数错误通过 `AllocError::InvalidInput` 返回；普通 cache 耗尽通过
`allocate` 的 `None` 与页面分配失败区分。构造期不合法的 page geometry 会 panic，
因为它是编译期/集成期配置错误而不是可恢复的 runtime input。

`SlabHeap::shrink` 可显式归还空 span。内部 buddy 首次返回 `NoMemory` 时也会调用
同一回收路径，然后只重试一次；没有可回收 span 时错误正常向上传播。该回收只把
pages 还给 `SlabHeap` 的内部 buddy，不能把 byte-heap region 退回 `kalloc` 的
全局 page buddy，也没有全 CPU flush。

## 隐私分析

本模块不处理用户数据、标识符或遥测数据。free object 中保留的旧内容不是本模块
主动收集或导出的数据；对象复用的初始化/清零策略由上层分配器与调用者决定。

## 已知限制

- 不检测 production double free、错误 `Layout` 或跨 cache free。
- 不含 per-CPU magazine、remote-free 队列、NUMA 策略或 CPU offline 生命周期。
- 不区分 task、softirq、hardirq allocation policy；没有 GFP/atomic flags 或 IRQ
  reserve，hardirq 路径没有有界延迟与成功保证。
- Empty span 只在显式 shrink 或内部 buddy 压力时回收；没有经 workload 调优的
  high/low watermark。
- 回收页不会离开 byte heap 的内部 buddy，不能缓解全局 page buddy 的压力。
- `SlabCache` 不提供内部锁；当前 `kalloc` 仍以单个全局 `SpinNoIrq` 串行化所有
  class，SMP 扩展性和最长 IRQ-off 时间尚未优化。

## 审计清单

- 每个传给 `add_slab` 的 span 是否来自正确的页分配器、独占且按
  `slab_bytes()` 对齐？
- 所有 `deallocate` 调用是否使用原始 allocation 对应的 cache 和 `Layout`？
- cache 的所有 list/counter/free-link 访问是否由同一同步协议保护？
- 页归还前是否只 detach `Empty` span，并确保不存在 outstanding object？
- refill 与 reclaim 是否在合适的 execution context 中进行，而非把慢速页操作
  放在不允许的临界区或中断路径？
