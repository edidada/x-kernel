# kalloc — 设计文档

## 定位

`kalloc` 是内核全局分配入口。它把全局页 buddy、页级 per-CPU cache（PCP）和
选定的字节分配策略组合为 `GlobalAllocator`，同时提供 `GlobalPage` RAII 包装。

当配置选择 `slab` 时，小对象不再经过一个泛化的 `ByteAllocator` 缓存层；
`GlobalAllocator` 直接拥有 `KmemCacheSet`。每一个
`alloc_engine::SizeClass` 对应一个 `SlabCache`，因此对象 freelist、slab span
header、partial/full/empty 状态和空 span 的回收权处于同一所有权层。

当配置选择 `buddy` 或 `tlsf` 时，`GlobalAllocator` 保留原有的
`ByteAllocator` 加页分配器的两层模型。它们不获得 slab 的 size-class 语义。

## 背景

先前的 PCB（per-CPU byte cache）位于 `GlobalAllocator` 与任意
`ByteAllocator` 之间。该层复制了 slab bucket 表，并假设所有 byte allocator
都可安全地按相同 canonical layout 批量 refill/drain。这使 slab 的对象缓存
策略泄漏到 Buddy 和 TLSF，也无法表达一个 slab span 是否仍有 live object。

当前阶段先建立可回收的 slab page ownership 边界。per-CPU magazine、remote
free、CPU offline 和全 CPU flush 在该边界之上实现，而不是重新引入泛化 PCB。

## 范围

```text
mm/kalloc/
├── src/lib.rs          # GlobalAllocator、页分配入口与统计
├── src/kmem_cache.rs   # slab 专属的 central KmemCacheSet
├── src/pcp.rs          # 1–4 页的 per-CPU page cache
├── src/page.rs         # GlobalPage RAII 包装
└── docs/               # 本文档与安全分析
```

`src/kmem_cache.rs` 仅在 `slab` feature 下编译。固定 class 的 page header 和
状态机由 `mm/alloc-engine/src/slab_cache.rs` 实现；`kalloc` 负责全局页来源、并发
策略和回收时机。

## 架构

```text
GlobalAllocator
 ├─ palloc: BuddyPageAllocator
 ├─ usages: requested-byte statistics
 ├─ slab build
 │   └─ KmemCacheSet
 │       └─ [SizeClass -> SpinNoIrq<SlabCache>]
 │            └─ Empty / Partial / Full slab spans
 └─ buddy/tlsf build
     └─ ByteAllocator

slab allocation miss
  KmemCacheSet::try_allocate()
       │ miss; no KmemCache lock held
       v
  page allocator -> prepare_slab() -> publish_slab()
       │                                │ short class lock
       └──────────── retry ─────────────┘
```

`SizeClass::from_layout()` 是唯一的 slab class 选择规则。不能由该规则表达的
layout 直接走页分配器。span 的页数和对齐也从同一个 `SizeClass` 得出，因此
2 KiB class 的四页 span 之类的几何约束不会被本 crate 复制或猜测。

## 调用约束 / 执行上下文

- 页分配器必须在 `GlobalAllocator::alloc()`、`GlobalPage::alloc()` 或公开页分配
  接口使用前通过 `init`、`init_page_allocator` 或 `add_memory` 初始化。
- 这些接口不依赖 current process、文件系统、MMIO、DMA、firmware 或 scheduler
  状态；传给初始化接口的虚拟地址范围必须已映射、可写且未被其他 owner 使用。
- 当前 central slab metadata 操作由 `SpinNoIrq` 保护，因此保持现有全局 allocator
  可从 IRQ-like 上下文重入的兼容性。它不 sleep，也不在 class lock 内调用页
  allocator 或执行 span 初始化。
- 这不等于普通任务 fast path 已经采用只禁抢占模型，也不等于硬中断已经有独立
  reserve。后续 per-CPU magazine 阶段必须先定义 task/IRQ 的分流协议，不能把
  任意 refill 直接放入硬中断路径。
- 1–4 页的通用页接口仍使用 PCP，并在 PCP miss/full 时使用已有的 IRQ-off
  refill/drain 行为；这与已移除的 byte PCB 是独立问题，当前提交不宣称消除它。

## 状态机

单个 span 的状态由 `alloc-engine::SlabCache` 维护：

```text
              allocate                 allocate last free object
Empty ----------------> Partial --------------------------------> Full
  ^                        |                                       |
  | last live free         | deallocate                            | deallocate
  +------------------------+---------------------------------------+
  |
  +-- take_empty_slab() --> Detached --> return pages to palloc
```

只有 `Empty` span 能被 `KmemCacheSet::take_empty_slab()` detach。`Partial` 和
`Full` 仍可能包含调用者持有的对象，绝不能归还其 backing pages。

## 算法流程

### slab 小对象分配

1. `GlobalAllocator::alloc_slab()` 用 `SizeClass::from_layout()` 路由 layout。
2. `KmemCacheSet::try_allocate()` 在目标 class 的短 metadata 临界区弹出一个
   已注册对象。
3. cache miss 时，调用者离开该锁后向页分配器请求一个 class 对齐的 span。
4. `prepare_slab()` 在锁外写入 span header 和 free-object links。
5. `publish_slab()` 只在目标 class 锁内挂入已准备的 span；随后重试第 2 步。
6. 成功后，`Usages` 记录调用者原始 `Layout` 的 requested bytes，而 central
   cache 的使用量按 class size 取整。

多个 CPU 可同时为同一 class 准备 span；这是允许的。每个 span 最终只有一次
publish，之后的 retry 从任一已发布的 span 获取对象。

### slab 释放和 shrink

1. `dealloc()` 以同一个 `SizeClass::from_layout()` 找回 class。
2. 目标 cache 在短锁内把 live object 归还 span，并执行 full/partial/empty
   转换。
3. `shrink()` 或 slab refill 的 OOM 重试循环 detach 所有当前 `Empty` spans。
4. 每一个 detached `SlabBacking` 在所有 KmemCache lock 外直接归还全局 page
   allocator。

当一个新的 slab span 请求得到 `NoMemory` 时，分配器先执行上述 central reclaim，
再重新检查目标 cache 并重试页分配。这能回收其他 size class 已经完全空闲的 span；
不会回收 partial/full span 或尚未实现的 per-CPU magazine 内容。

### 非 slab allocator

Buddy 和 TLSF 的小对象直接进入各自 `ByteAllocator`。当其没有空间时，原有扩堆
逻辑从页分配器取得区域并交给 byte allocator。大尺寸或高对齐 layout 直接使用
页分配器；本路径没有 slab cache 或 PCB 参与。

## 并发模型

- 每个 slab `SizeClass` 有独立的 `SpinNoIrq<SlabCache>`。对象获取、释放、span
  publish 和 detach 只锁一个 class，不会持锁进入页分配器。
- `palloc` 和 `usages` 各自有独立的 `SpinNoIrq`。slab fast path 在释放 class
  lock 后才更新 `usages`；refill/reclaim 在没有 class lock 时访问 `palloc`。
  因此新的 slab 路径不形成 KmemCache -> page allocator 的嵌套锁序。
- `take_empty_slab()` 逐 class 短暂扫描。它只用于显式 shrink 或 OOM 失败路径，
  不属于对象 fast path。
- 此阶段每次 slab allocation/free 仍会获取 central class lock，并且 usage
  accounting 仍是全局锁。它修正所有权和 IRQ-off 临界区范围，但不是最终 SMP
  扩展性方案。
- 未来 magazine fast path 应仅依赖本 CPU 的状态并禁止抢占；其 refill/drain、
  remote free、CPU offline 与全 CPU flush 需要一个独立、可确认的协议。

## 设计决策

1. **KmemCache 持有 slab policy，而非通用 byte cache。** 这将 span 生命周期和
   `SizeClass` 留在 Slab 的 owner，Buddy/TLSF 不会被赋予不属于它们的策略。
2. **两阶段 prepare/publish。** backing span 的较长初始化在 class lock 外完成；
   token 发布只覆盖 metadata 状态转换。
3. **OOM 前只回收可证明为空的 span。** `Empty` 状态提供明确的回收安全条件，
   而不是基于全局 freelist 的猜测。
4. **保留 non-slab 分支。** 这次重构改变 slab 结构，不把无关 allocator 的内部
   模型一并重写。
5. **不提前宣称性能收益。** 后续需要使用真实 SMP workload 记录吞吐、lock wait、
   p99 和最大 IRQ-off latency，才能评价 magazine 的收益。

## Drop / 资源释放

`GlobalAllocator` 是静态全局对象，没有运行时析构。slab span 只有在没有 live
object 时通过 `shrink()` 或 OOM reclaim 显式归还；`Partial`/`Full` span 的映射和
backing ownership 必须保留到对象全部释放。

`GlobalPage` 拥有它分配的连续页，并在 `Drop` 中通过 `dealloc_pages()` 归还。
由 `GlobalPage::from_raw()` 重建 owner 时，调用者必须保证不存在另一个 owner。
