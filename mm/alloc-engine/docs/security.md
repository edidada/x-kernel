# alloc-engine — 安全与可靠性分析

## 信任模型

`alloc-engine` 位于内核内部的 allocator algorithm 边界。它信任 owner 提供的 managed
region、`Layout`、页数、地址与释放配对；不信任这些前置条件被自动满足。违反 contract
的内核调用者可破坏 free list、allocator metadata 或其他 live object。

各实现共享以下信任前提：

- region 已映射、可写、独占，不与现有 managed region 重叠，并在 allocator 使用期间
  持续有效；
- allocation 只由原始 allocator 释放，pointer/地址、`Layout`、页数和对齐与原始请求
  匹配；
- 对同一 allocator 状态的 mutation 已由 owner 串行化；
- owner 在释放整个 backing region 前已释放其中全部对象、页或 ID。

`SlabCache` 还信任 prepare 的 span 具有匹配 `SizeClass` 的几何形状。safe `allocate`
只访问已注册 metadata；`prepare_slab`、`add_slab` 和 `deallocate` 的 unsafe contract
承担 backing span、对象来源与对象生命周期的证明责任。

## 外部边界 / 攻击面

本 crate 不直接解析用户内存、MMIO/PIO、DMA buffer、设备输入、文件系统、网络、IPC、
bootloader 或 firmware 数据，也没有对外 C ABI。攻击面来自传入 allocator 的内核地址、
region、`Layout` 和释放操作，以及 BuddyByteAllocator/TLSF 所封装的下层 allocator。

`AllocatorRc` 是可选的 Rust `Allocator` API 适配器；它把 pool slice 交给内部
`ByteAllocator`，仍继承同样的 region 与 deallocation contract。它不是线程安全容器，
不得用作跨 CPU 共享 allocator 的同步边界。

## unsafe 代码清单

| 路径 | 依赖的不变量 | 保护方式 |
|---|---|---|
| `BuddyByteAllocator::{init_region,add_region}` | 传入范围可写、独占且在下层 heap 生命周期内有效 | `BaseAllocator` contract；unsafe 调用局限于 wrapper |
| `TlsfByteAllocator::{init_region,add_region}` | pool slice 有效、可写、不与已插入 block 重叠 | `BaseAllocator` contract；`rlsf` 拒绝无效 free-block 插入 |
| `SlabHeap::{new,add_memory}` | backing region 有效、独占、持续映射 | unsafe constructor/扩容 contract；内部 buddy 验证 region geometry |
| `SlabCache::{prepare_slab,add_slab}` | span 大小、对齐和独占性匹配 class，并且不与其他 prepared/registered span 重叠 | unsafe contract、geometry 检查和不可复制 `PreparedSlab` token |
| `SlabCache::deallocate` | object 来自同一 cache、仍为 live 且仅释放一次 | unsafe contract；debug 模式检查 class/状态/对象边界 |
| `SlabCache` 的 raw header / intrusive-link helpers | 仅访问初始化的 span 或 free object；list state 一致 | 私有 helper、`&mut self` 和集中状态转换 |
| `unsafe impl Send for SlabCache` | 移动 cache 不会使 registered span 提前失效，且没有无锁共享 mutation | cache 不实现 `Sync`；owner 必须建立外部串行化 |
| `AllocatorRc` 的 `Allocator::deallocate` | Rust `Allocator` caller 保证 pointer/layout 配对 | `RefCell` 串行 wrapper 内部访问；继续转发原始 contract |

## 内存安全不变量

1. Buddy/TLSF/slab 管理的 region 在使用期间不重叠、不被外部释放或重映射。
2. Buddy allocation 的地址与原始页数必须配对归还；在 chunk 分裂/合并期间，同一 free
   chunk 只出现于一个 free list。
3. Byte allocation 的 `Layout` 必须在 deallocation 时保持一致；错误 size 或 alignment
   可能破坏下层 allocator metadata。
4. 每个 prepared 或 registered slab span 只属于一个 `SizeClass` 和一个 `SlabCache`；
   span base 与 span 大小满足 power-of-two geometry。
5. `SlabPage` 恰好处于 `Empty`、`Partial`、`Full` 或 `Detached` 之一。只有 `Empty`
   span 可 detach；live object 不在 freelist 上。
6. `PreparedSlab` 要么被匹配 cache 消费，要么由 owner 用 `into_backing()` 取回并归还；
   不得遗留半发布 span。
7. allocator algorithm 本身不提供并发安全；所有 list、counter、region 和下层 heap
   state 的读写必须处于 owner 的同一同步域。

## 线程安全

`BuddyAllocator`、BuddyByteAllocator、TlsfByteAllocator、SlabHeap 和 SlabCache 都不
持有全局同步原语。它们的 mutable API 依赖 `&mut self`，但内核 owner 若将它们放入
interior-mutability 容器或自旋锁，必须让同一规则覆盖 allocation、deallocation、region
扩展与 shrink。

`SlabCache` 仅以受审计的 `unsafe impl Send` 支持拥有者迁移独占状态，刻意不提供
`Sync`。它不含原子、不会 sleep，也不会关闭 IRQ；这些事实不构成任意中断上下文可调用
的保证。是否能在 hardirq/NMI 运行，以及如何处理 CPU-local 或 remote-free，对本 crate
而言都是上层执行上下文策略。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 无效、重叠或提前失效的 managed region | 高 | owner 传入错误地址/长度，或外部重用该范围 | region contract；Buddy region overlap 检查；残余风险是违规内核调用者 |
| T-02 | 错误 `Layout`、页数或 allocator 的释放 | 高 | double free、跨 allocator free、错误 alignment | API contract；slab debug 检查；下层 allocator 的 metadata 检查范围有限 |
| T-03 | 无同步并发 mutation | 高 | 多 CPU 同时修改同一 heap/cache | 算法不声称 `Sync`；owner 必须使用单一串行化域 |
| T-04 | 伪造或错误对齐的 slab span 覆写 header | 高 | unsafe prepare/add 违反 backing-span contract | `SizeClass` geometry、运行时对齐/容量检查和不可复制 prepared token |
| T-05 | slab live object 被提前归还 | 高 | 仅根据粗粒度 freelist 回收或错误状态转换 | `Empty`/`Partial`/`Full` 状态机；只允许 `take_empty_slab` detach |
| T-06 | free object 的首 word 被当作仍有效数据 | 中 | caller 在释放后继续读取/写入对象 | intrusive link 只写入 free object；caller contract 禁止 use-after-free |
| T-07 | Buddy region 或计数溢出 | 中 | 极端 region 大小、地址或对象数量 | 输入与 geometry 检查、checked accounting；不可表示的请求返回错误 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 应对措施 |
|---|---|---|---|---|---|
| F-01 | 页或字节 allocation 无可用空间 | region 耗尽或碎片化 | 返回 `NoMemory` | 上层决定扩容、回收或失败处理 | `AllocError::NoMemory`；算法不自行执行全局 reclaim |
| F-02 | region 不符合算法 geometry | 地址未对齐、长度不足或重叠 | 初始化/扩展失败 | allocator 不发布错误 region | `InvalidInput` / `MemoryOverlap`；owner 修正输入 |
| F-03 | SlabCache 无空对象 | 所有 span 为 `Full` 或尚未注册 span | `allocate` 返回 `None` | 上层需要取得新 backing span | 在 cache mutation 外 refill，prepare/publish 后重试 |
| F-04 | span 不能回收 | span 为 `Partial`/`Full` | backing pages 保留 | 内存压力持续 | `shrink` 只 detach `Empty`，由上层选择下一步策略 |
| F-05 | prepared span 无法 publish | class 不匹配或计数无法表示 | span 不可见 | 当前 allocation 失败 | 返回 token，owner 归还 backing pages，不留下半发布状态 |

## 故障管理

普通资源不足通过 `AllocError::NoMemory` 返回；无效 region/geometry 通过
`InvalidInput`、`MemoryOverlap` 或其他明确错误返回。`BuddyAllocator::init_region()` 会
重置其已有 region，因此 owner 不得把它当作无副作用的重复初始化接口。

`SlabHeap::shrink()` 仅归还空 span。本 crate 不触发 OOM reclaim、不遍历 CPU-local
storage，也不代替上层决定何时扩容、回收或终止请求。

## 隐私分析

本 crate 不处理、持久化或导出用户数据、身份标识或遥测。free object 的旧内容会被
allocator metadata 或 intrusive link 覆写；对象复用前是否清零由上层和调用者定义，
不能把 allocator freelist 当作数据隔离机制。

## 已知限制

- 不提供 production double-free、错误 `Layout` 或跨 allocator free 检测。
- 不提供内部锁、per-CPU magazine、remote-free、NUMA、CPU lifecycle 或系统级
  memory-pressure policy。
- `SlabHeap` 只提供显式 `shrink()`；是否将它接入全局 reclaim 属于上层 owner。
- `PageFlags` 是兼容性 hook，当前 BuddyAllocator 不保存逐页 flags。
- `IdAllocator` 是公共 trait，具体 ID allocator 的生命周期和并发模型由实现者定义。

## 审计清单

- 每个 `init_region` / `add_region` 输入是否已映射、可写、独占且不重叠？
- allocation 与 deallocation 是否保持同一 allocator、地址、页数和 `Layout`？
- 同一算法实例的所有 mutation 是否由一个完整的同步协议保护？
- slab span 是否只由 `SizeClass` 几何创建、只 publish 一次，并在空时才 detach？
- 是否在页归还前排除了所有 live object、prepared token 和并发 cache access？
- 上层是否在合适 execution context 处理 refill、shrink、CPU-local cache 与 OOM，
  而没有把这些 policy 假设放进算法 crate？
