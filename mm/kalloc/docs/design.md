# kalloc — 设计文档

## 定位

`kalloc` 是内核的全局内存分配入口。它把全局页 buddy、选择的字节 allocator、页级
per-CPU page cache（PCP）、使用量 accounting 和 RAII 页包装组合为静态
`GlobalAllocator`。

它提供四类服务：

- `GlobalAlloc` 所需的任意 `Layout` 字节 allocation/deallocation；
- 连续 4 KiB 页、DMA32 页和指定地址页的显式 allocation；
- `GlobalPage`，用于以 RAII 方式持有连续页；
- 可选的 allocation tracking 与 `dice`/`tee` 构建中的 C allocator shims。

`kalloc` 负责内核级配置、同步和执行上下文选择；`alloc-engine` 只提供下层 buddy、
TLSF 和 slab 算法。物理内存发现、虚拟映射建立、CPU lifecycle、NUMA policy 和系统级
memory pressure policy 不属于本 crate。

## 范围

```text
mm/kalloc/
├── src/lib.rs          # GlobalAllocator、配置路由、页接口、usage accounting
├── src/pcp.rs          # 1–4 页的 per-CPU page cache
├── src/page.rs         # GlobalPage RAII 所有权包装
├── src/kmem_cache.rs   # slab class central cache、reserve 与 flush access state
├── src/magazine.rs     # slab task/softirq 的 per-CPU 双 magazine
├── src/tracking.rs     # 可选 allocation backtrace tracking
├── src/ffi.rs          # dice/tee 构建中的 C allocation shims
└── docs/               # 设计与安全可靠性分析
```

正常内核配置在 `slab`、`buddy` 和 `tlsf` 中选择一种；源码至少要求启用一个 allocator
feature。`ipi` 只为 slab magazine flush 接入跨 CPU provider；`tracking` 只启用诊断性
allocation tracking。

## 全局架构

```text
                 GlobalAlloc / page APIs / GlobalPage / optional C shims
                                           │
                                           v
                                  GlobalAllocator
              ┌────────────────────┼────────────────────┐
              │                    │                    │
              v                    v                    v
  BuddyPageAllocator<4 KiB>   usage accounting      selected byte route
              │                                      ┌───────┼────────┐
              ├─ direct page requests                v       v        v
              ├─ DMA32 / fixed-address pages       slab    buddy    TLSF
              └─ PCP for standard 1–4 pages          │       │        │
                                                       v       └─ DefaultByteAllocator
                                                KmemCacheSet
                                                ├─ class central SlabCache
                                                ├─ task/softirq magazines
                                                └─ hardirq reserves
```

所有配置都使用 `BuddyPageAllocator<4 KiB>` 作为 backing page allocator。不同之处只在
字节 allocation：

- **slab**：`GlobalAllocator` 直接拥有 `KmemCacheSet`；可表达为 `SizeClass` 的小对象
  由 class-owned slab cache 管理，其他 layout 直接使用页；
- **buddy / TLSF**：`GlobalAllocator` 持有受 `SpinNoIrq` 保护的
  `DefaultByteAllocator`。小 layout 从该 byte allocator 取得对象，较大或高对齐 layout
  直接使用页；byte allocator 需要更多空间时由全局页 allocator 提供 heap region。

`DefaultByteAllocator` 在 slab 构建中仍作为公开 type alias 存在，但 runtime
`GlobalAllocator` 不把它作为 slab 小对象路径。

## 初始化与 region 生命周期

`GlobalAllocator` 在任何 allocation 前必须先取得至少一个已映射、可写、独占的内核
虚拟地址 region：

1. `init()` 初始化页 buddy；buddy/TLSF 配置还从页 allocator 取得 bootstrap heap 并
   初始化 byte allocator；
2. `init_page_allocator()` 只初始化或扩展页 buddy；它不会立即建立非 slab byte heap；
3. `add_memory()` 把新 region 加入页 buddy。非 slab 配置若尚无 byte heap，会建立首次
   bootstrap heap；之后 byte allocator 在需要时从页 buddy 扩展；
4. region 不得与现有页 buddy region 重叠，也不得在仍有 allocation 时被外部回收或
   重映射。

`page_ready` 表示页 allocator 已可用；非 slab 配置额外维护 `heap_ready`。slab 不拥有
独立 heap：每个 backing span 直接来自 `palloc`。

```text
uninitialized
      │ init / init_page_allocator / add_memory
      v
page-ready
      │ buddy or TLSF: bootstrap a byte heap when needed
      v
byte-heap-ready (non-slab only)
```

## 页分配路径

### 普通连续页

`alloc_pages()` 接收页数、power-of-two alignment 和 `UsageKind`。标准对齐的 1–4 页
请求先访问当前 CPU 的 PCP；PCP miss 时从全局 buddy 填入相应 page-count cache，PCP
满时把一批缓存块归还全局 buddy。其它页数或特殊 alignment 直接进入全局 buddy。

PCP 的对象是连续的 1、2、3 或 4 页块，不是 slab object cache。它的本地访问要求
local IRQ 已屏蔽，refill/drain 在页 buddy 锁保护下进行；页级 cache 的回收与 slab
magazine reclaim 是两套独立协议。

`dealloc_pages()` 必须使用原始地址、页数和 `UsageKind`。小块优先回到当前 CPU PCP；
不能缓存或缓存已满时回到全局 buddy。

### DMA 与指定地址页

`alloc_dma_pages()` 从全局 buddy 取得连续页后以 `v2p` 检查整个范围低于 4 GiB；不满足
DMA32 限制的页会立即归还并报告 `NoMemory`。`alloc_pages_at()` 要求指定地址由页 buddy
管理且满足 alignment。两条路径均不使用 PCP。

### GlobalPage

`GlobalPage` 是连续页的唯一 RAII owner。`alloc`、`alloc_zero` 和
`alloc_contiguous` 从 `GlobalAllocator` 取得页；`Drop` 使用相同页数归还。`from_raw`
仅用于从一个已分配且没有其他 `GlobalPage` owner 的页范围重建该所有权。

## 字节分配路径

### Buddy 与 TLSF 配置

当 layout 的 size 或 alignment 超过半页时，`alloc()` 直接调用页 allocator；其它
layout 由 `DefaultByteAllocator` 处理。byte allocator 耗尽时，`GlobalAllocator` 从
页 buddy 取得新的 heap region 并扩展它，然后重试 allocation。deallocation 根据同一
layout 使用与 allocation 对应的 byte 或 page 路径。

### Slab 配置

`SizeClass::from_layout()` 是小对象唯一的 class 选择规则。它无法表达的 layout 直接
走页 allocator，不会由通用 byte cache 猜测 class 或对齐。

每个 `SizeClass` 对应一个 central `SlabCache`，其 span header、对象 freelist 和
`Empty` / `Partial` / `Full` 状态由 `alloc-engine` 保持。`KmemCacheSet` 在这个
central state 之外拥有两类 execution-context-specific storage：

- task/softirq：当前 CPU 的 active + spare 双 magazine；
- normal hardirq：每个 class 的有界 shared reserve。reserve 的基础预算按 class
  设定，再乘以编译期 `NR_CPUS`，覆盖每个配置 CPU 各有一个基础 burst 的情况；它仍由
  class lock 保护，不是 task magazine。

normal task/softirq 先访问 magazine。local magazine 无对象时，调用者在 IRQ 开启的
上下文中从 central class 逐对象取得 refill batch；central 也无对象时，在 class lock
外从页 buddy 获取 class 对齐 span、prepare 后再短暂 publish。normal hardirq 优先从
reserve 取得对象；reserve 为空时最多从 central class 取一个已有 free object 作为
有界兜底，不执行页 allocation、span 初始化或 task magazine 操作。reserve 和 central
都没有对象时才返回 `NoMemory`。NMI allocation/deallocation 不受支持。

释放使用相同 `SizeClass`：hardirq 回到 reserve；normal task/softirq 优先补充需要的
reserve，否则放入当前 CPU magazine。两本 magazine 都无法容纳对象时，摘出的 batch 在
IRQ 开启状态逐对象返回 central class。对象从 magazine 或 reserve 返回 central 后才
能参与 span 的空闲判定。

```text
SizeClass -> KmemCache
             ├─ central SlabCache: Empty / Partial / Full spans
             ├─ task/softirq: per-CPU active + spare magazines
             └─ hardirq: shared NR_CPUS-scaled reserve -> central one-object fallback
```

## Slab reclaim 与系统边界

`shrink()` 仅在 slab 配置下回收页：它先 flush 所有已启动 CPU 的 task magazine、drain
hardirq reserve，再从每个 central class detach `Empty` span 并在 class lock 外归还 backing
pages。Buddy/TLSF 配置没有 central slab span，`shrink()` 返回零。

slab refill 在页 allocation 返回 `NoMemory` 时也执行一次相同的 reclaim，然后重新检查
central cache 和页 buddy。它不会回收 `Partial` / `Full` span 或调用者持有的 object。

SMP slab 构建通过 `KmemCacheFlushIf` 与 `kipi` 协调：session 先以 generation stage
每个 present CPU 的 magazine state，IPI 只确认 idle-origin state；若目标 task 正持有
mutable magazine borrow，则它在该 borrow 结束后自行发布 frozen。发起 task 在确认全部
target frozen 后、IRQ 开启状态下 drain 远程 batch，随后解除 frozen state。没有 IPI
provider 的构建只处理当前 CPU。

该协议只覆盖启动时已建立的 present-CPU 集合。接入下列能力前必须由相应 subsystem
建立 lifecycle owner：

- CPU offline 前串行化 slab flush、drain 并解绑该 CPU 的 magazine，以及 CPU online
  时建立对应 storage；
- NUMA 的 node-local span ownership、remote-free 和 reclaim policy；
- 统一协调 slab、PCP 和其他可回收者的系统级 memory-pressure/reclaim framework。

## 诊断与可选接口

`UsageKind` / `Usages` 是 allocation usage 的诊断 accounting：runtime singleton 以
per-CPU signed delta 记录更新，`usages()` 扫描得到非线性化的聚合快照；显式构造的测试
allocator 使用自有锁保护的表。它不表示对象所有权，也不能用作可回收性判断。

启用 `tracking` 时，`GlobalAlloc` 在不递归进入自身的前提下记录 allocation 的 layout、
backtrace 和 generation，供内核诊断使用。启用 `dice` 或 `tee` 时，`ffi.rs` 提供 C
`malloc` / `calloc` / `free` 及内存操作 shims；它们仍通过同一个 `GlobalAllocator`
路由，C ABI 的 pointer contract 由调用者承担。

## 调用约束与并发模型

- 所有公开 allocation 接口均依赖页 allocator 已初始化，但不要求 current process、
  文件系统、MMIO、DMA 驱动状态或 scheduler 已就绪；传入 region 必须已映射且独占。
- 全局 page buddy 由 `SpinNoIrq` 保护。Buddy/TLSF 的 byte allocator 也由独立
  `SpinNoIrq` 保护；它们的 heap 扩展遵循各自的 region 生命周期。
- PCP 只在 local IRQ 屏蔽时访问，且只处理本 CPU storage。它没有跨 CPU flush、CPU
  offline 或全局 pressure 生命周期。
- runtime usage accounting 使用 per-CPU atomic delta；它允许 allocation 与跨 CPU free
  的诊断快照并发变化。可选 tracking 使用独立 state 与 per-CPU recursion guard。
- slab central metadata 每个 `SizeClass` 一把 `SpinNoIrq<KmemCache>`；页 allocation、
  span 初始化和 detached page 归还不在该 class lock 内。task magazine 只在 current-CPU
  pin 下访问；hardirq 不触碰 magazine，先访问 reserve，必要时从 central 取一个对象。
- slab `shrink()` 和 OOM reclaim 可能等待其它 CPU 完成短暂的 local magazine access，
  因此不得从 hardirq 或 NMI 调用。被冻结 CPU 的 task/softirq allocation/free 直接走
  central class，不等待 reclaim session。

## 设计决策

1. **页 allocator 是所有配置的共同 backing owner。** Byte allocation policy 不改变
   页 region 的初始化、DMA 和显式页接口。
2. **PCP 与 slab object cache 分离。** PCP 缓存固定大小的页块；magazine 缓存固定 class
   的 slab object，二者具有不同的执行上下文和回收语义。
3. **Slab policy 由 KmemCacheSet 持有。** `SizeClass`、span lifecycle、magazine 和
   hardirq reserve 不泄漏给 Buddy 或 TLSF。
4. **硬中断不执行 page work。** hardirq 先使用独立 reserve；reserve 耗尽时只允许从
   central class 取一个已有对象作为有界兜底，绝不把 task magazine 或 span refill 带入
   中断路径。
5. **仅回收可证明为空的 span。** `Empty` 是 backing page 可归还的唯一 slab 状态；
   所有 CPU magazine 与 reserve 必须先回到 central state。
6. **系统 lifecycle 不由 allocator 臆造。** CPU hotplug、NUMA 和系统级 reclaim 必须
   由拥有相应拓扑与资源生命周期的 subsystem 接入。

## Drop / 资源释放

`GLOBAL_ALLOCATOR` 是静态对象，不在运行时析构。非 slab byte heap 的 region 继续由
其 byte allocator 持有；slab backing pages 只在 `shrink()` 或 OOM reclaim 确认 span
为空后归还。`GlobalPage` 在 `Drop` 中归还其连续页；从 `from_raw` 建立 owner 时，调用者
必须保证没有第二个 owner。
