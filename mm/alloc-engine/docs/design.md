# alloc-engine — 设计文档

## 定位

`alloc-engine` 提供 X-Kernel 可复用的内存分配算法和统一的 allocator contract。它
管理调用者交给它的内存区间，但不负责发现物理内存、建立页表、选择全局 allocator，
也不定义锁、CPU-local storage、IRQ 或内存压力策略。

crate 对外定义三类接口：

- `ByteAllocator`：按 `Layout` 分配字节对象；
- `PageAllocator`：按连续基页分配内存；
- `IdAllocator`：供拥有者实现连续 ID 的分配语义。

`BaseAllocator` 统一这些接口的 region 初始化与扩展契约。Buddy、TLSF 和 slab 是
`ByteAllocator` 的不同实现；`BuddyPageAllocator` 是 `PageAllocator` 的实现。`IdAllocator`
目前只定义公共 contract，不持有全局 ID 状态。

## 范围

```text
mm/alloc-engine/
├── src/lib.rs          # allocator trait、错误类型、可选 Allocator API 适配器
├── src/buddy_alloc.rs  # 通用页 buddy 算法与 page flags
├── src/buddy_page.rs   # BuddyAllocator 的 PageAllocator 适配器
├── src/buddy.rs        # buddy_system_allocator 的 ByteAllocator 适配器
├── src/tlsf.rs         # rlsf TLSF 的 ByteAllocator 适配器
├── src/slab_trait.rs   # slab SizeClass 与对象/池接口
├── src/slab_cache.rs   # 单一 SizeClass 的 span ownership 与对象 freelist
├── src/slab_heap.rs    # 由内部 buddy 支撑的九个 SlabCache
└── src/slab.rs         # SlabHeap 的 ByteAllocator 适配器
```

`buddy-page`、`buddy`、`tlsf`、`slab` 和 `allocator_api` 都是独立 feature。上层选择
哪些实现参与构建；算法 crate 不替上层选择默认策略。

## 公共契约与内存区间生命周期

所有 allocator 都以调用者提供的、已映射、可写、独占且在 allocator 生命周期内持续
有效的 region 为输入：

1. `init_region()` 建立第一个 managed region；
2. `add_region()` 仅接收与既有 region 不重叠且满足对应算法约束的扩展；
3. allocation 返回的对象、页或 ID 在释放前仍由该 allocator 管理；
4. deallocation 必须回到原 allocator，并使用原始 pointer/地址、大小与对齐语义。

`AllocError` 区分无效输入、重复初始化、region overlap、未初始化、未找到的地址和
`NoMemory`。具体 wrapper 对不满足初始化前置条件的行为由其 rustdoc 定义；调用者不应
把 region 初始化错误当作可恢复的普通 allocation miss。

## 架构

```text
                         caller-owned mapped memory regions
                                      │
                                      v
                              BaseAllocator contract
                                      │
          ┌───────────────────────────┼───────────────────────────┐
          │                           │                           │
          v                           v                           v
   ByteAllocator                 PageAllocator                IdAllocator
          │                           │                     (interface only)
  ┌───────┼────────┐                  v
  │       │        │          BuddyPageAllocator
  v       v        v                  │
Buddy    TLSF   SlabByteAllocator      v
wrapper wrapper       │          BuddyAllocator<PAGE_SIZE>
                       v
                  SlabHeap
                 ┌────┴────┐
                 v         v
       SlabCache[SizeClass] BuddyAllocator<4096>
```

`BuddyAllocator<PAGE_SIZE>` 是本 crate 的页粒度基础算法：它维护 region 中的 buddy
chunks、分裂与合并。`BuddyPageAllocator` 直接把它适配为页接口；`SlabHeap` 则把它
作为自身 backing span 与非 slab layout 的来源。

`BuddyByteAllocator` 和 `TlsfByteAllocator` 是各自算法实现的薄适配器，负责把
`BaseAllocator` / `ByteAllocator` 的 region、`Layout` 和统计语义映射到其后端。
它们不获得 slab 的 class、span 或对象缓存策略。

可选的 `AllocatorRc<A>` 用 `Rc<RefCell<A>>` 把单个 `ByteAllocator` 适配给 Rust
`Allocator` API；它只提供该包装器范围内的可变借用串行化，并不是跨 CPU 同步原语。

## 页与字节分配算法

### Buddy 页分配

`BuddyAllocator` 接收页对齐 region，将可管理范围拆成合法的 power-of-two chunks。
请求时它选择满足页数与对齐要求的 chunk，并按需分裂；归还时按 buddy 关系合并。非
power-of-two 页数的 caller-facing 请求仍由该算法完成相应的 chunk 管理，调用者必须
以原始页数和地址归还。

`BuddyPageAllocator` 只暴露连续页语义。`allocate_pages_at()` 额外要求目标地址属于
managed region 并满足指定对齐。`PageFlags` 是兼容性 hook；当前 `BuddyAllocator` 不保存
逐页 flags，因此上层不能把它当作持久 page metadata。

### Byte allocator 适配器

- `BuddyByteAllocator` 将 `Layout` 直接交给 `buddy_system_allocator::Heap`；
- `TlsfByteAllocator` 将 region 插入 `rlsf::Tlsf`，并以调用者的 `Layout` 完成
  allocation/deallocation；
- `SlabByteAllocator` 持有一个 `SlabHeap`。能由 `SizeClass` 表示的 layout 进入 slab，
  其余 layout 直接从 `SlabHeap` 内部 buddy 获取连续页。

这些实现的可用字节和已用字节均是 allocator accounting，不是跨 allocator 的全局
内存压力判断。

## Slab 数据模型

`SizeClass` 是 slab 唯一的 class 定义：它按 `max(layout.size(), layout.align())`
选择 8、16、32、64、128、256、512、1024 或 2048 字节中的最小可满足 class；超过
`SLAB_MAX_SIZE` 的 layout 不属于 slab。class 同时给出一个 span 的基页数，避免上层
复制 size-to-pages 几何规则。

`SlabCache<PAGE_SIZE>` 管理一个固定 class。它不申请页，而是接收调用者独占取得且
按 span 大小对齐的 backing span：

```text
owning heap / page allocator
        │ obtain an aligned span
        v
prepare_slab() ──> PreparedSlab ──> publish_slab()
                                      │
                                      v
                         SlabCache<SizeClass>
                         ├─ Empty   : no live object
                         ├─ Partial : live and free objects coexist
                         └─ Full    : no central free object
```

`PreparedSlab` 是 publication 前的所有权 token：header 与 free-object links 在
`prepare_slab()` 中写好，随后只由匹配 class 的 `publish_slab()` 接收。这样调用者可
把较长的 span 初始化与其自身的同步临界区分开。

每个 span 的首部是 `SlabPage`，记录 class、容量、free count 和状态表链接。空闲对象
的首个 machine word 保存 intrusive next link；对象被调用者持有时不在 freelist 中，
allocator 不读取或改写它。

```text
                       allocate
  Empty ─────────────────────────> Partial ── allocate last free object ─> Full
    ^                                  │                                      │
    │ deallocate last live object      │ deallocate                            │ deallocate
    └──────────────────────────────────┴──────────────────────────────────────┘
    │
    └── take_empty_slab() ──> Detached
```

只有 `Empty` span 可 detach。`Partial` 与 `Full` 均至少包含一个 live object，不能将
其 backing pages 归还。

`SlabHeap` 持有所有九个 `SlabCache<4096>` 和一个内部 buddy。cache miss 时它从该
buddy 取得 class 对齐的 span，通过 `add_slab()` 完成 prepare/publish 后重试；`shrink()`
detach 全部空 span 并把它们归还内部 buddy。它不自行决定何时触发 shrink。

## 调用约束与并发模型

- 算法层不依赖 current task、调度器、MMIO、DMA、firmware 或 CPU topology；但所有
  backing 地址必须在当前内核地址空间可访问。
- `BuddyAllocator`、各 `ByteAllocator` wrapper、`SlabHeap` 和 `SlabCache` 都通过
  `&mut self` 暴露 mutation，不提供内部全局锁。跨线程或跨 CPU 使用时，owner 必须建立
  覆盖整个算法状态的外部串行化规则。
- `SlabCache` 可移动（`Send`），但刻意不提供无锁共享（不实现 `Sync`）。同一 cache
  的 state list、counter、header 和 free-object link 必须由同一外部同步域保护。
- `prepare_slab()` / `add_slab()` 的 unsafe caller 必须保证 span 独占；registered
  span 在 `take_empty_slab()` 返回前持续映射。`deallocate()` 的 unsafe caller 必须
  提供来自该 cache 的 live object，且只能释放一次。
- 页申请、页归还、per-CPU cache、IRQ 约束、remote free、NUMA 与 OOM policy 都由
  使用 `alloc-engine` 的上层 owner 定义，而不隐含在任一算法中。

## 设计决策

1. **接口与策略分离。** trait 描述 region、字节、页和 ID 的 contract；全局 allocator
   决定配置、锁、执行上下文与回收时机。
2. **Slab class 与 span ownership 同属一个 cache。** `SizeClass`、对象 freelist 和
   `Empty`/`Partial`/`Full` 状态在同一层，才能给出可验证的空 span 回收条件。
3. **Slab 不成为通用 byte-cache 模型。** Buddy 与 TLSF 有自己的内存组织方式，不要求
   它们采用 slab class、batch 或 span 生命周期。
4. **初始化与 publication 分离。** `PreparedSlab` 把可能较长的 header/link 初始化
   放在 cache publication 之前，允许上层维持短临界区。
5. **算法 crate 不暗含 execution-context policy。** 是否可在 hardirq 使用、如何做
   CPU-local caching 或 CPU offline cleanup，必须由具备该生命周期所有权的上层实现。

## Drop / 资源释放

allocator 不拥有 caller 提供的虚拟映射，也不会在 `Drop` 中归还整个 region。各 allocation
必须在 owner 释放或销毁 region 前先归还；`SlabHeap::shrink()` 只能归还已证明为空的
span。若仍存在 `Partial` / `Full` span 或其他 live allocation，owner 必须保留其 backing
region、映射和生命周期。
