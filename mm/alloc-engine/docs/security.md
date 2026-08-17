# alloc-engine — 安全与可靠性分析

## 信任模型

`alloc-engine` 位于内核内部的 allocator 边界。它信任调用者提供的管理内存、
`Layout` 和释放配对；它不直接接收用户指针或设备输入。

`SlabCache` 的边界尤其明确：safe `allocate` 信任已注册的 page metadata，
而 `add_slab` 与 `deallocate` 是 unsafe API，调用者负责 span 所有权、对象来源
和对象生命周期。

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
| `src/slab_cache.rs: SlabList` | page 只在一个状态表中，所有 links 在同一串行化域内修改 | 私有 API、状态转换集中在 `move_page` / `take_empty_slab` |
| `src/slab_cache.rs: unsafe impl Send for SlabCache` | cache 可移动，但注册 span 在 detach 前仍映射且不发生无锁共享 | 类型只保存 span 内 metadata 指针；所有 mutation 需要 `&mut self`；刻意不实现 `Sync` |
| `src/slab_heap.rs` 与 buddy/TLSF 适配器 | 释放地址和 `Layout` 与原分配匹配 | 上层 `ByteAllocator` unsafe deallocation contract |

## 内存安全不变量

1. 一个 registered slab span 只属于一个 `SlabCache` 和一个 `SizeClass`。
2. span 大小及其 base 均为 power-of-two span 对齐，因此由 object 地址掩码恢复的
   header 正确。
3. 每个 `SlabPage` 恰好处于 `Partial`、`Full`、`Empty` 或 `Detached` 之一；只有
   `Empty` 可以 detach。
4. `free_count` 与该 page freelist 中对象数一致；`total_blocks`、`free_blocks`
   与所有 registered page 的计数一致。
5. live object 不在 freelist 上；其首个 word 只有在释放后才能被 allocator 覆写。
6. 被 detach 或仍含 live object 的 span 不得归还给页分配器。

## 线程安全

`SlabCache` 不提供内部同步。它仅以受审计的 `unsafe impl Send` 支持锁保护容器
转移其独占所有权，刻意不实现 `Sync` 或允许无锁共享访问。内核上层把它放进共享
容器时，必须用单一的外部串行化规则覆盖 list、header、counter 和 free-object
link 的全部访问。`take_empty_slab` 返回后，页的释放必须不与任何仍可能访问该
cache 的路径并发。

该层没有原子、不会 sleep，也不会主动关闭 IRQ；这不表示上层可从硬中断或 NMI
无条件调用 allocator。适用 execution context 由整合它的内核 allocator 明确
规定和审计。

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
| F-03 | 底层页分配失败 | 内存压力或 buddy 无连续 span | 无法 refill | allocation 返回 NoMemory 或触发上层 reclaim | 2 | `SlabHeap::shrink` 可归还已空 span；更高层应在 OOM 前接入 flush/reclaim |
| F-04 | live object 长期不释放 | 调用者泄漏或缓存策略保留对象 | page 不能变 Empty | 可用内存下降 | 3 | ownership 审计、统计与上层 shrink 机制 |

## 故障管理

geometry 或计数错误通过 `AllocError::InvalidInput` 返回；普通 cache 耗尽通过
`allocate` 的 `None` 与页面分配失败区分。构造期不合法的 page geometry 会 panic，
因为它是编译期/集成期配置错误而不是可恢复的 runtime input。

`SlabHeap::shrink` 可显式归还空 span，但本 crate 不自行触发 OOM reclaim 或全 CPU
flush。将 shrink 接入全局 page allocator、并定义 per-CPU cache 的 flush protocol，
仍是拥有 execution-context policy 的上层职责。

## 隐私分析

本模块不处理用户数据、标识符或遥测数据。free object 中保留的旧内容不是本模块
主动收集或导出的数据；对象复用的初始化/清零策略由上层分配器与调用者决定。

## 已知限制

- 不检测 production double free、错误 `Layout` 或跨 cache free。
- 不含 per-CPU magazine、remote-free 队列、NUMA 策略或 CPU offline 生命周期。
- `SlabCache` 不自行 shrink；`SlabHeap` 只提供显式 shrink，尚未自动接入 OOM 前
  reclaim。
- 不提供内部锁；使用者必须建立并验证并发协议。

## 审计清单

- 每个传给 `add_slab` 的 span 是否来自正确的页分配器、独占且按
  `slab_bytes()` 对齐？
- 所有 `deallocate` 调用是否使用原始 allocation 对应的 cache 和 `Layout`？
- cache 的所有 list/counter/free-link 访问是否由同一同步协议保护？
- 页归还前是否只 detach `Empty` span，并确保不存在 outstanding object？
- refill 与 reclaim 是否在合适的 execution context 中进行，而非把慢速页操作
  放在不允许的临界区或中断路径？
