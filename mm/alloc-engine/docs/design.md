# alloc-engine — 设计文档

## 定位

`alloc-engine` 提供 x-kernel 内存分配算法的无平台实现：页级 buddy、字节级
buddy、TLSF 与 slab 适配器。它只维护已交给分配器的内存，不负责建立内核页表、
选择物理内存区域或定义全局锁策略。

本次引入的 `SlabCache<PAGE_SIZE>` 是固定 `SizeClass` 的 slab-page ownership
核心。它让 slab 的对象、backing page 和可回收状态保持在同一所有权层，而不是
只保留一个跨 page 的对象 freelist。

## 范围

```text
mm/alloc-engine/
├── src/buddy_alloc.rs    # 通用页 buddy 算法
├── src/buddy*.rs         # buddy 适配器
├── src/slab_trait.rs     # SizeClass 与兼容 trait
├── src/slab_cache.rs     # 单 size class 的 page ownership 核心
├── src/slab_heap.rs      # 现有 slab byte heap
├── src/slab.rs           # SlabByteAllocator 适配器
└── src/tlsf.rs           # TLSF 适配器
```

`SizeClass` 是 slab 的唯一 class 定义。它根据 `Layout` 的 size 和 alignment
选择最小可满足 class，并同时决定每个 slab span 使用的基页数。

## 架构

`SlabCache` 不向页分配器发出请求；它接收调用者取得的、对 span 大小对齐的内存，
并在该 span 首部写入 metadata：

```text
page allocator / owning heap
        │ acquire aligned slab span
        v
prepare_slab()              (不接触 cache 状态)
        │
        v
PreparedSlab ── publish_slab() ──> SlabCache<SizeClass>
 ├─ Empty list   : 所有对象均在 central freelist，可 detach
 ├─ Partial list : 同时有 live 和 central-free 对象
 └─ Full list    : 没有 central-free 对象
        │
        └─ take_empty_slab() -> caller returns pages outside cache mutation
```

`SlabHeap` 持有九个这样的 cache 及内部 `BuddyAllocator<4096>`：小对象进入
对应 cache，cache miss 时从 buddy 获得一个正确对齐的 span，`SlabHeap::shrink`
则先 detach 空 span 再归还 buddy。非 slab layout 仍走直接 buddy 路径。

每个 backing span 的首部是 `SlabPage`。剩余空间按该 `SizeClass` 对齐并切分为
对象；空闲对象的首个 machine word 作为 intrusive next link。因此对象被分配给
调用者期间，其内容不会被 cache 访问或改写。

`SlabHeap` 已使用 `SlabCache` 替代原先只含跨 page global freelist 的 slab
实现。更高层的内核 per-CPU cache 仍应建立在 `SlabCache` / `SlabHeap` 提供的
page ownership 之上，而不能把同一策略施加给任意 `ByteAllocator`。

## 调用约束 / 执行上下文

- `SlabCache` 不含锁，也不禁用 IRQ 或抢占。所有方法要求调用者串行化访问；将它
  放入何种锁、是否适合中断上下文，属于上层分配器的职责。
- `prepare_slab` 的 backing span 必须由调用者独占、可写，并按 `slab_bytes()`
  对齐；prepared token 必须被 publish 或归还其页分配器。span 注册后必须持续映射，
  直到 `take_empty_slab` 将其 detach。
- `allocate` 只操作已注册的 metadata 和 freelist。cache 空时返回 `None`，调用者
  应在 cache mutation 之外取得页并再次调用 `add_slab`。
- `take_empty_slab` 只返回没有 live object 的 span。页面归还也应在 cache mutation
  之外完成，避免把可能较慢的底层页分配操作混入临界区。
- 本模块不依赖 current task、调度器、MMIO、DMA 或 firmware 初始化；但 backing
  地址必须已在当前内核地址空间可访问。

## 状态机

```text
                       allocate
  Empty ─────────────────────────> Partial ── allocate last free object ─> Full
    ^                                  │                                      │
    │ deallocate last live object      │ deallocate                            │ deallocate
    └──────────────────────────────────┴──────────────────────────────────────┘
    │
    └── take_empty_slab() ──> Detached (不再属于 cache)
```

只有 `Empty` 状态的 span 可被 detach。`Partial` 与 `Full` 表示至少有一个对象
仍被上层持有，因此不能在这两个状态归还 backing pages。

## 算法流程

1. `SlabHeap` 根据 `SizeClass::from_layout` 路由小对象。
2. refill 在 cache 锁外取得 backing span 并调用 `prepare_slab` 初始化 page header
   和 free-object links；之后在短临界区内以 `publish_slab` 挂入 `Empty` list。
3. `allocate` 优先从 `Partial` 的 freelist 弹出对象；没有 partial page 时将一个
   `Empty` page 移至 `Partial` 后再弹出。最后一个 free object 被弹出时转为 `Full`。
4. `deallocate` 通过对齐 span 掩码恢复 `SlabPage`，将对象压回该 page 的 freelist。
   `Full` 首次释放转为 `Partial`；最后一个 live object 释放后转为 `Empty`。
5. `SlabHeap::shrink` 循环调用 `take_empty_slab`，再把已 detach 的 spans 交给其
   内部 buddy。上层可将它接入内存压力和 OOM 前回收策略。

计数器只统计 cache 已经注册的 span；`add_slab` 在写入 span 前先验证所有可能
溢出的全局计数，避免错误返回后留下未被 cache 记录的半发布状态。

## 并发模型

`SlabCache` 本身是非并发数据结构。它实现 `Send`，使一个拥有它的锁保护容器
可以在执行上下文之间转移；它刻意不实现 `Sync`。`&mut self` 使普通 Rust 调用
具有排他性，但内核上层若通过内部可变性或自旋锁共享它，必须确保每次状态转换、
freelist 读写和空 span detach 都由同一串行化规则保护。

该层有意不持有页分配器引用，因而也不会在 metadata 临界区内分配或释放 backing
pages。`PreparedSlab` 进一步把 free-list 初始化移到 publication 之前。这为上层
实现短临界区、per-CPU magazine 和跨 CPU flush 提供了边界。

## 设计决策

- **page 属于具体 size class**：header 记录 `SizeClass`、容量、free count 和
  list links，使回收能判断某个 span 是否真的没有 live object。
- **partial/full/empty 分表**：避免 shrink 扫描所有对象或误回收仍被使用的 page。
- **对象内 intrusive link**：不增加每对象的外部 metadata；代价是 free object
  的首个 machine word 不能保留旧内容，这是 allocator 的正常语义。
- **不在此层内置 per-CPU 或 IRQ 策略**：这些依赖内核 execution context，不能
  隐式施加给 TLSF、buddy 或一般 `ByteAllocator`。

## Drop / 资源释放

`SlabCache` 不拥有 backing memory，因此没有自动归还页的 `Drop` 实现。
`SlabHeap::shrink` 显式归还空 span；销毁 heap 或回收其管理的内存前，调用者仍须
保证不存在 live object。若 cache 仍含 `Partial` 或 `Full` span，调用者必须保留
其映射和所有权。
