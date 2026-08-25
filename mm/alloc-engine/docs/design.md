# alloc-engine slab cache v1 — 设计文档

## 定位

`alloc-engine` 提供 X-Kernel 的无平台内存分配算法。本次 v1 只重构
`SlabHeap` 的小对象管理：每个固定 `SizeClass` 拥有一个 central
`SlabCache`，每个 cache 明确记录其 backing span 及其中的 live/free 对象。

这版参考 Asterinas 的：

- `osdk/deps/heap-allocator/src/slab_cache.rs`：`Empty / Partial / Full`
  三类 slab；
- `osdk/deps/heap-allocator/src/allocator.rs`：固定 size class 路由。

v1 **不引入** Asterinas 的 CPU-local `ObjectCache`，也不引入 X-Kernel
实验分支中的 magazine、IRQ reserve、IPI flush、GFP 风格参数或独立
`KmemCacheSet`。目标是先得到一个边界小、能独立 review 和回归的 central
slab cache。

## 范围

```text
mm/alloc-engine/
├── src/slab_trait.rs     # 唯一的 SizeClass 定义与 layout 路由
├── src/slab_cache.rs     # 单个 size class 的 span 状态机
├── src/slab_heap.rs      # 九个 cache 与内部 buddy 的组合
├── src/slab.rs           # ByteAllocator 适配器
└── docs/                 # 本设计与安全分析
```

本 PR 不修改 `kalloc`、`kirq`、`kipi`、页级 PCP 或任何调用方 API。

## 架构

在 slab 配置下，现有集成关系为：

```text
kalloc::GlobalAllocator
└─ balloc: SpinNoIrq<SlabByteAllocator>       现有全局锁
   └─ SlabHeap
      ├─ SlabCache<8>
      ├─ SlabCache<16>
      ├─ ...
      ├─ SlabCache<2048>
      └─ internal BuddyAllocator<4096>

kalloc global page buddy
  └─ 在 byte heap 扩容时把新 region 交给 SlabHeap
     （v1 没有从 SlabHeap 反向退页的通道）
```

`SlabCache` 不主动申请内存。`SlabHeap` 从自己的内部 buddy 取得一个按完整
span 大小对齐的 backing allocation，再交给目标 cache 初始化。span 首部保存
`SlabSpan` metadata，剩余空间按 class size 切成对象；free object 的第一个
machine word 保存 intrusive next link。

每个 cache 维护三条 intrusive list：

```text
Empty   所有对象均空闲，可 detach
Partial 同时存在 live object 和 free object，优先用于分配
Full    没有 free object，只能等待对象归还
```

`SlabBacking` 是 detach 后的唯一移交 token。它不是 `Copy`/`Clone`，避免安全
代码无意复制一次回收权。

## 调用约束 / 执行上下文

### alloc-engine 层

- `SlabCache` 不含锁、不会关闭 IRQ、不会关闭抢占，也不会 sleep；所有 mutation
  都要求调用者持有唯一的 `&mut SlabCache`。
- `add_slab` 的 backing span 必须独占、可写、按 `slab_bytes()` 对齐，并持续映射
  到 `take_empty_slab` 将其 detach 为止。
- `deallocate` 要求 object 来自同一 cache、仍为 live object 且只释放一次。
- 本层不依赖 current task、调度器、MMIO、DMA、firmware 或进程上下文。

### 当前 kalloc 集成

`kalloc` 仍以一个 `SpinNoIrq<SlabByteAllocator>` 串行化整个 byte heap。
`SpinNoIrq` 同时关闭本 CPU IRQ 和内核抢占，因此普通 task、softirq 与 hardirq
不会在同一 CPU 上重入该状态；不同 CPU 通过同一自旋锁互斥。NMI 不在该保证内。

这只说明现有路径在锁与内存安全上保持一致，**不代表 v1 提供了合格的 hardirq
allocation latency 或 atomic allocation 语义**。cache miss、span 初始化、内部
buddy reclaim，以及 `kalloc` 外层可能发生的 byte-heap 扩容，都可能在该全局
IRQ-off 临界区中执行。v1 也没有依据 task/softirq/hardirq 选择不同策略。

## 状态机

```text
                         allocate
  Empty ───────────────────────────> Partial
    ^                                    │
    │ deallocate last live object        │ allocate last free object
    │                                    v
    └────────────────────────────────── Full
                     deallocate

  Empty ── take_empty_slab() ──> Detached ──> internal buddy
```

状态不变量：

- `Empty`: `free_count == capacity`；
- `Partial`: `0 < free_count < capacity`；
- `Full`: `free_count == 0`；
- `Detached`: 已不属于任何 list，cache 不得再访问其 metadata。

只有 `Empty` span 可以 detach。`Partial` 和 `Full` 至少仍有一个上层持有的对象，
不得归还 backing pages。

## 算法流程

### 小对象分配

1. `SizeClass::from_layout()` 同时考虑 size 与 alignment，选择最小可满足 class。
2. cache 优先从 `Partial` span 弹出一个 free object。
3. 没有 `Partial` 时，取一个 `Empty` span 转为 `Partial` 后分配。
4. cache 完全没有 free object 时，`SlabHeap` 从内部 buddy 申请一个新 span。
5. 如果内部 buddy 返回 `NoMemory`，`SlabHeap` detach 所有 `Empty` span、归还给
   内部 buddy，然后只重试一次。
6. 新 span 初始化并进入 `Empty` list，随后重试对象分配。

v1 一次 miss 只新建一个 span，不像 Asterinas 当前实现那样顺带预建一批 empty
slab；这减少了首次 miss 的工作量和短期内存占用。

### 释放与回收

1. `deallocate` 由 object 地址按 span 对齐掩码恢复 `SlabSpan` header。
2. `Full` 首次收到 free object 时转为 `Partial`。
3. 最后一个 live object 归还时，span 转为 `Empty`。
4. 正常 free 只保留 `Empty` span，不立即做 buddy 操作。
5. `SlabHeap::shrink()` 或内部 buddy 的首次 `NoMemory` 会回收所有 `Empty` span。

因此 v1 在内部 buddy 压力下不会因为空 slab 占着页而直接失败，但 normal path
仍保留 warm empty span，避免 alloc/free 立即反复拆建 slab。

### 非 slab layout

不能由 size class 表达的 layout 直接走 `SlabHeap` 内部 buddy；该路径同样在首次
`NoMemory` 时回收 empty slab span 后重试一次。`kalloc` 仍保留自己的大对象直达
全局页分配器规则，本 PR 不改变该边界。

## 并发模型

- `SlabCache` 是非并发数据结构；list、header、counter 和 object freelist 必须由
  同一个外部串行化域保护。
- v1 的实际串行化域是 `kalloc::GlobalAllocator::balloc` 的单个 `SpinNoIrq`。
  九个 class 之间也会互相竞争。
- cache 与内部 buddy 同属 `SlabHeap`，所以 refill、压力回收和 span 初始化都在
  该外层锁内完成。v1 没有新的锁顺序，但扩大操作仍继承现有全局锁的延迟问题。
- `SlabCache` 只实现 `Send`，不实现 `Sync`；共享访问必须经过上层锁。

## v1 的已知设计缺陷与未来工作

这些是本次有意接受的边界，不应在 PR 描述中宣称已经解决：

| v1 缺陷 | 当前影响 | 后续方向 |
|---|---|---|
| 单个全局 `SpinNoIrq` | 所有 CPU、所有 class 串行；IRQ-off 临界区可能较长 | 在 execution-context 契约明确后增加 per-CPU magazine，并缩短 central lock 范围 |
| 没有 GFP/atomic 风格输入 | task、softirq、hardirq 使用相同策略，调用者无法禁止 page work 或 reclaim | 先实现 X-Kernel 所需的最小 flag 子集，再逐步扩展，不照搬完整 Linux GFP |
| hardirq 没有独立成功保证 | miss 时可能做 span 初始化、buddy 操作或最终失败；没有 reserve | 定义 atomic allocation contract；再决定 IRQ reserve、预留页或其他有界兜底 |
| byte heap 与全局 page buddy 两级所有权 | 空 span 只能回到 `SlabHeap` 内部 buddy；已交给 byte heap 的 region 不能退回全局 page buddy | 将 central cache 直接接到全局页 allocator，或增加可验证的 region shrink/transfer 协议 |
| empty span 只在压力或显式 shrink 时回收 | byte heap 内可能保留较多 warm span | 增加经过 workload 验证的 high/low watermark，而非本次拍脑袋设常数 |
| 没有 per-CPU 生命周期协议 | 无 remote free、CPU offline drain、全 CPU flush、NUMA policy | 与 magazine 一起设计 owner、迁移、hotplug 和 reclaim 协议 |
| release 构建不检测 invalid free | 错 layout、跨 cache free、double free 仍依赖 unsafe caller contract | 增加可选 debug/hardening metadata，不能把昂贵检查默认放进 fast path |
| 无内存清零/poison | free object 保留旧内容 | 由安全策略决定是否提供 debug poison、敏感对象清零或专用 cache |

建议演进顺序：先合入本 v1 并建立正确性/回归基线；随后解决全局页回收与 allocation
context；再引入 per-CPU fast path；最后根据 IRQ 延迟和真实 workload 决定 reserve、
批量参数与更复杂 reclaim。当前大重构分支可以继续承载这些后续方案，但不应把它们
塞回本 PR。

## 设计决策

1. **先建立 span ownership，再优化 fast path。** 没有 per-span live-object 计数，
   无法安全回收 backing pages，也无法可靠实现后续 magazine flush。
2. **只保留一套 `SizeClass`。** layout 路由、span 页数和 object 对齐从同一来源
   派生，避免 `kalloc`、cache 和 per-CPU 层复制 bucket 表。
3. **metadata 与 link 均为 intrusive。** 不依赖 heap 分配 metadata，避免 allocator
   自递归；代价是 span 首部占用空间，free object 的首个 word 会被覆写。
4. **压力回收只重试一次。** 这能回收本 heap 已持有的确定空闲内存，同时避免无
   退出条件的 reclaim loop。
5. **不在 v1 引入新的公共 cache API。** `SlabCache` 是 `alloc-engine` 的私有实现
   细节；外部仍通过现有 `SlabHeap` / `ByteAllocator` 契约使用它。

## Drop / 资源释放

`SlabHeap` 所管理的 region 由上层长期持有，通常与全局 allocator 同生命周期。
`SlabCache` 不在 `Drop` 中自行归还 backing pages。运行期只有 `Empty` span 能通过
`shrink` 或压力回收回到内部 buddy；含 live object 的 span 必须持续映射。
