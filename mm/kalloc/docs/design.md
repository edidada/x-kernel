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

当前阶段在该 ownership 边界之上加入了 slab 专属的 task per-CPU 双 magazine 和
hardirq reserve。它们只服务 `KmemCacheSet`，而不是成为任意 `ByteAllocator` 的
中间层。全 CPU flush、shrink 协调、CPU offline 和 NUMA 仍需建立独立的跨 CPU
协议，不能通过重新引入泛化 PCB 来替代。

## 范围

```text
mm/kalloc/
├── src/lib.rs          # GlobalAllocator、页分配入口与统计
├── src/kmem_cache.rs   # slab 专属的 central KmemCacheSet
├── src/magazine.rs     # slab 专属的 task per-CPU 双 magazine
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
 ├─ per-CPU usages: requested-byte statistics
 ├─ slab build
 │   └─ KmemCacheSet
 │       ├─ current CPU task/softirq magazine pairs
 │       │    └─ [SizeClass -> active + spare magazine]
 │       └─ [SizeClass -> SpinNoIrq<KmemCache>]
 │            ├─ central SlabCache -> Empty / Partial / Full spans
 │            └─ bounded hardirq-only reserve
 └─ buddy/tlsf build
     └─ ByteAllocator

task slab allocation miss
  current CPU magazine pair
       │ miss; preemption re-enabled
       v
  KmemCacheSet::take_task_batch()  (one object / short class lock)
       │ miss; no KmemCache lock held
       v
  page allocator -> prepare_slab() -> publish_slab()
       │                                │ short class lock
       └──────────── retry ─────────────┘

hardirq slab allocation
  KmemCacheSet::hardirq reserve -> object | NoMemory
```

`SizeClass::from_layout()` 是唯一的 slab class 选择规则。不能由该规则表达的
layout 直接走页分配器。span 的页数和对齐也从同一个 `SizeClass` 得出，因此
2 KiB class 的四页 span 之类的几何约束不会被本 crate 复制或猜测。

## 调用约束 / 执行上下文

- 页分配器必须在 `GlobalAllocator::alloc()`、`GlobalPage::alloc()` 或公开页分配
  接口使用前通过 `init`、`init_page_allocator` 或 `add_memory` 初始化。
- 这些接口不依赖 current process、文件系统、MMIO、DMA、firmware 或 scheduler
  状态；传给初始化接口的虚拟地址范围必须已映射、可写且未被其他 owner 使用。
- normal task 和 softirq 只通过 `percpu::with_current` 访问当前 CPU 的 magazine；
  该 helper 禁止抢占，但不关闭 IRQ。由于 hardirq 从不访问 magazine，硬中断打断
  task/softirq 不会并发修改其中的 freelist。
- normal hardirq 由 `kirq::context::is_in_hardirq_fast()` 分流到 class 的独立
  reserve。该路径不分配页、不初始化 span、不批量 refill；reserve 为空时明确返回
  `NoMemory`。NMI allocation 仍不受支持。
- central slab metadata 仍由 `SpinNoIrq` 保护，但 task refill/drain 只在每次传输
  一个对象时短暂持有它；页 allocator、span 初始化和 detached page 归还均不在
  class lock 内。
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
2. normal hardirq 只从该 class 的 bounded reserve 获取对象；reserve 为空立即
   返回 `NoMemory`，不会进入 task magazine 或页分配路径。
3. task/softirq 先在当前 CPU 的 active/spare magazine pair 获取对象。这是不获取
   central class lock 的 fast path；reserve 的 under-full 状态不会使 magazine hit
   降级。
4. local miss 时，normal task 可先从已有 central object 补充 hardirq reserve，
   再以每对象一次短 class lock 的方式取得有界 refill batch。batch 的其余对象
   在当前 CPU 重新 pin 后安装到一个空 magazine；迁移导致无法安装的对象立即回到
   central cache。
5. central cache 也没有对象时，调用者离开该锁后向页分配器请求一个 class 对齐的
   span；`prepare_slab()` 在锁外写入 span header 和 free-object links，
   `publish_slab()` 只在目标 class 锁内挂入已准备的 span，然后重试第 4 步。
6. 成功后，`Usages` 记录调用者原始 `Layout` 的 requested bytes，而 central
   cache 的使用量按 class size 取整。

多个 CPU 可同时为同一 class 准备 span；这是允许的。每个 span 最终只有一次
publish，之后的 retry 从任一已发布的 span 获取对象。

### slab 释放和 shrink

1. `dealloc()` 以同一个 `SizeClass::from_layout()` 找回 class。
2. hardirq 释放回 reserve；reserve 满时才在一次短 metadata 临界区把对象归还
   central slab。normal task 在 reserve 需要补充时优先捐赠一个对象，否则将对象
   放入当前 CPU magazine。
3. local pair 的两本 magazine 都满时，释放路径摘下一整本 spare magazine，并在
   IRQ 开启状态下逐对象归还 central slab；每次归还只持有一次短 class lock。
4. `shrink()` 或 slab refill 的 OOM 重试循环目前只 detach central `Empty` spans。
   每一个 detached `SlabBacking` 在所有 KmemCache lock 外直接归还全局 page
   allocator。

当一个新的 slab span 请求得到 `NoMemory` 时，分配器先执行上述 central reclaim，
再重新检查目标 cache 并重试页分配。这能回收其他 size class 已经完全空闲的 span；
不会回收 partial/full span，且在全 CPU flush 到位前也不会回收 task magazine 或
hardirq reserve 中的对象。

### 非 slab allocator

Buddy 和 TLSF 的小对象直接进入各自 `ByteAllocator`。当其没有空间时，原有扩堆
逻辑从页分配器取得区域并交给 byte allocator。大尺寸或高对齐 layout 直接使用
页分配器；本路径没有 slab cache 或 PCB 参与。

## 并发模型

- 每个 slab `SizeClass` 有独立的 `SpinNoIrq<KmemCache>`。central object 获取、
  释放、reserve 更新、span publish 和 detach 只锁一个 class，不会持锁进入页
  分配器。
- task/softirq magazine pair 是 current-CPU storage。其 hit path 仅 pin 当前 CPU，
  不取得 central class lock；hardirq 永不触碰该 storage，而只操作对应 class 的
  reserve。NMI allocator use 不受支持，不能假定这些保护覆盖 NMI。
- `palloc` 由独立的 `SpinNoIrq` 保护。requested-byte accounting 使用 per-CPU
  atomic delta：更新只触及当前 CPU，因此跨 CPU free 不会把对象所有权错误绑到释放
  CPU；`usages()` 才扫描所有 CPU 汇总诊断快照。
  slab fast path 在释放 class lock 后更新统计；refill/reclaim 在没有 class lock
  时访问 `palloc`。因此新的 slab 路径不形成 KmemCache -> page allocator 的嵌套锁序。
- `take_empty_slab()` 逐 class 短暂扫描。它只用于显式 shrink 或 OOM 失败路径，
  不属于对象 fast path。
- task magazine hit 不获取 central class lock；refill、drain、reserve 补充和 span
  状态转换仍会竞争该 class lock。reserve under-full 时只在 local miss 或 normal
  task free 上机会性补充。usage accounting 已不再形成全局串行点，但这还不是完整的
  SMP memory-pressure 方案。
- 全 CPU flush、shrink/OOM 对 magazine 的协调、CPU offline 和 NUMA 仍需要一个
  无分配、可确认的 IPI/生命周期协议；在该协议落地前，reclaim 不会把 local cache
  对象误当成可回收 central free object。

## 设计决策

1. **KmemCache 持有 slab policy，而非通用 byte cache。** 这将 span 生命周期和
   `SizeClass` 留在 Slab 的 owner，Buddy/TLSF 不会被赋予不属于它们的策略。
2. **两阶段 prepare/publish。** backing span 的较长初始化在 class lock 外完成；
   token 发布只覆盖 metadata 状态转换。
3. **OOM 前只回收可证明为空的 span。** `Empty` 状态提供明确的回收安全条件，
   而不是基于全局 freelist 的猜测。
4. **保留 non-slab 分支。** 这次重构改变 slab 结构，不把无关 allocator 的内部
   模型一并重写。
5. **task 与 hardirq 分流。** task/softirq fast path 用 current-CPU magazine；
   hardirq 用有界 reserve 并有明确耗尽语义，因而不会在中断中执行 page work 或
   批量 refill。
6. **不提前宣称性能收益。** 后续需要使用真实 SMP workload 记录吞吐、lock wait、
   p99 和最大 IRQ-off latency，才能评价 magazine 的收益。

## Drop / 资源释放

`GlobalAllocator` 是静态全局对象，没有运行时析构。slab span 只有在没有 live
object 时通过 `shrink()` 或 OOM reclaim 显式归还；`Partial`/`Full` span 的映射和
backing ownership 必须保留到对象全部释放。

`GlobalPage` 拥有它分配的连续页，并在 `Drop` 中通过 `dealloc_pages()` 归还。
由 `GlobalPage::from_raw()` 重建 owner 时，调用者必须保证不存在另一个 owner。
