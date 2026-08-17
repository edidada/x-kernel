# kalloc — 安全与可靠性分析

## 信任模型

`kalloc` 位于内核内部 allocator 边界。它信任：

- boot/平台内存管理在初始化时提供有效、已映射、独占的虚拟内存区域；
- `GlobalAlloc` 和 `GlobalAllocator::dealloc()` 调用者使用原始 allocation 的
  pointer 与 `Layout`，且每个对象只释放一次；
- `alloc-engine` 的 buddy、TLSF、`SizeClass` 和 `SlabCache` 分别维护其文档中的
  allocator contract；
- 调用方不在一个仍有 live allocation 的 span 上执行外部释放或重映射。

本 crate 负责在 slab 配置下把每个 class 的 page ownership、对象 freelist 状态和
central reclaim 绑定在一起，并保证 backing page 操作不发生在 KmemCache metadata
临界区内。

## 外部边界 / 攻击面

`kalloc` 不直接解析用户内存、文件系统、网络、IPC、设备输入、MMIO/PIO、DMA、
FFI、inline assembly、device tree、ACPI、bootloader 或 firmware 数据。

其输入边界是内核调用者传入的 `Layout`、初始化区域和页地址。未受信任数据只能
在更高层被复制/验证后间接影响 allocation size；不能把用户指针直接作为
`GlobalPage::from_raw()`、页 allocator 初始化或 slab backing span 使用。

## unsafe 代码清单

| 路径 | 依赖的不变量 | 保护方式 |
|---|---|---|
| `src/lib.rs: GlobalAllocator::alloc_slab_class` | 页分配成功返回非空、正确对齐、独占且可写的 class span | `alloc_pages()` 使用 class 的页数与 span 对齐；紧邻的 `SAFETY:` 注释将该范围传给 `NonNull` 和 `prepare_slab` |
| `src/lib.rs: GlobalAllocator::dealloc` | pointer 是同一 `SizeClass` 的 live object，layout 与 allocation 匹配，未重复释放 | `GlobalAlloc` contract；该 crate 只依据 canonical `SizeClass` 路由，底层 `SlabCache::deallocate` 仍保留 unsafe contract |
| `src/kmem_cache.rs: KmemCacheSet::prepare_slab` | `base` 是正确大小、按 span 对齐、独占的 backing allocation | 将 `SlabCache::prepare_slab` 的 contract 原样转发；返回的不可复制 `PreparedSlab` 只能 publish 或交还页 allocator |
| `src/kmem_cache.rs: KmemCacheSet::{deallocate,try_deallocate_to_irq_reserve,deallocate_irq}` | object 来自本 cache set 的 matching class 并保持 live | class lock 提供 central metadata/reserve 排他访问；unsafe obligation 由本方法调用者承担 |
| `src/kmem_cache.rs: KmemCacheSet::{try_allocate_from_task_magazine,deallocate_to_task_magazine,install_task_refill}` | 调用方处于 normal task/softirq，object/batch 属于 matching class | `percpu::with_current` pin 当前 CPU；hardirq 被分流到同一 KmemCacheSet 的 reserve，NMI allocation 不受支持；batch 只在同一 class 的 central/local storage 间转移 |
| `src/kmem_cache.rs: frozen magazine remote drain` | 目标 CPU 已对同一 generation 发布 `Frozen`，且 session 尚未 unfreeze | pending-active 状态只能由本地 task 在释放 mutable borrow 后推进；IPI 只完成 pending-idle；`remote_ref_mut_raw()` 的 SAFETY 注释把 state、CPU range 和 session ownership 作为前置条件 |
| `src/lib.rs: unsafe impl GlobalAlloc` | 调用方遵守 Rust `GlobalAlloc` allocation/deallocation 配对规则 | 统一路由到 `GlobalAllocator`；失败调用 `handle_alloc_error`，不返回伪造 pointer |
| `src/page.rs: GlobalPage::{from_raw,fill,as_slice,as_slice_mut}` | owner 唯一、页范围有效且未提前归还 | `from_raw` 是 unsafe constructor；其余方法由 `GlobalPage` 的所有权和 `&mut self` 保证范围与排他性 |

这些 raw-pointer 路径属于「raw ownership pointers」场景：`NonNull` 表示成功页分配
后的非空 pointer，但安全性还取决于其完整 span 的有效性、对齐、生命周期和独占性，
不能仅由非空性质推出。

## 内存安全不变量

1. slab span 只由一个 `SizeClass` 的 `SlabCache` 注册，直到它作为 `Empty` span
   detach。
2. 每个 registered span 都保持映射、可写且独占；其 base 和页数满足该 class 的
   `slab_pages()`/`slab_bytes()` geometry。
3. `Partial` 或 `Full` span 含有至少一个 live object，永不交还 page allocator；
   `Empty` 是唯一可回收状态。
4. 一个 prepared span 只能 publish 一次；publish 失败时其 backing pages 必须在
   cache lock 外归还。
5. 释放必须选择原 allocation 的 class。错误 layout、跨 cache free 或 double free
   违反 allocator contract，可能破坏 free-object intrusive link。
6. `Usages` 统计 requested bytes；slab cache 统计 class-rounded bytes。两者用途
   不同，不能将前者当作 span 可回收性的证据。
7. task magazine 中的对象仍被所属 `SlabCache` 视为 checked out。只有全 CPU flush
   对同一 generation 观察到 `Frozen` 后，发起 CPU 才能把其远程转为 central free
   object；pending-active 状态绝不能作为 remote borrow 的证据。hardirq reserve 同样
   独立于 central freelist，且只能由 hardirq 或 normal task 的受控补充路径访问。

## 线程安全

每个 central class cache、hardirq reserve 和页 buddy 由 `SpinNoIrq` 保护。
requested-byte 统计是 per-CPU atomic delta：更新时只 pin 当前 CPU，跨 CPU free
在当前 CPU 记负 delta，`usages()` 扫描所有 CPU 形成可并发变化的诊断快照。
normal task/softirq 只在 `percpu::with_current` 所给的 current-CPU pin 下访问双
magazine；normal hardirq 被 `is_in_hardirq_fast()` 分流到 reserve，从而不会并发
触碰 task magazine。NMI allocation 不受支持。

central refill、drain 与 reserve 补充每次仅转移一个对象，释放 class lock 后才处理
下一个对象。页申请、span 初始化以及 detached page 的归还都在 class lock 外进行。

slab 路径的锁顺序刻意避免 KmemCache 与 `palloc` 的嵌套：fast allocation/free
完成 class lock 后才做 usage accounting；refill/reclaim 访问 page allocator 前
没有持有 class lock。实现或审计 future magazine 时必须保持该属性。

`SpinNoIrq` 不是“任意长度 IRQ-off 都可接受”的保证。slab task hit 现在无需获取
central lock，但 reserve、refill/drain 与 span transition 仍有短 IRQ-off 临界区。
global flush 的 IPI handler 只处理原子状态，真正的 remote magazine drain 在发起
task 的 IRQ 开启上下文中逐对象执行。PCP 仍有独立的批处理临界区；slab 尚无 CPU
offline 或 NUMA protocol。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 错误 backing span 覆写 slab header 或对象链接 | 高 | 平台传入未映射、未对齐、大小不匹配或非独占页 | class geometry 从 `SizeClass` 派生，`prepare_slab` contract 和 runtime geometry 检查；残余风险是恶意/错误内核调用者违反 unsafe contract |
| T-02 | double free 或跨 class free 使 live 数据进入 freelist | 高 | allocator caller 传错 pointer/layout 或重复释放 | `GlobalAlloc` 配对 contract、class 路由和 `SlabCache` debug 检查；当前没有 production double-free quarantine，残余风险保留 |
| T-03 | 归还包含 live object 的 span 造成 use-after-free | 高 | 回收仅依据粗粒度全局 freelist 或错误页状态 | span header 维护 `Empty`/`Partial`/`Full`；只允许 `take_empty_slab()` detach |
| T-04 | refill/drain 或 hardirq path 在 IRQ-off class lock 内扩大 IRQ latency | 中 | miss/full 时调用底层页 allocator、批量初始化或硬中断直接走 task cache | page work/初始化均在锁外；task batch 每次只转移一个对象；hardirq 只使用 bounded reserve，耗尽时返回 `NoMemory` |
| T-05 | 空 span 或可用对象留在其他 CPU cache 导致不必要 OOM | 中 | 目标 class miss、页 buddy 暂无可用 span | `NoMemory` 后 staged/frozen/drain 所有已启动 CPU 的 magazine、drain reserve、再 detach `Empty` span；并发 hardirq 或 live object 仍可正确保留对应 span |
| T-07 | IPI 与尚未释放的 local `&mut` magazine borrow 并发 | 高 | 远程 CPU 正在 task magazine fast path 时进入 shrink | active target 先变为 pending-active；IPI 不可推进它，只有本地 closure 结束后以 Release 转为 frozen，之后才允许 remote raw mutable borrow |
| T-08 | 延迟 IPI 在 flush 结束后再次冻结 CPU | 中 | IPI 到达晚于 unfreeze | request 在发 IPI 前已写入 generation-tagged pending state；handler 只把 matching pending-idle 推进 frozen，看到 idle/frozen 旧状态时无副作用 |
| T-06 | 多 CPU 竞争 central class lock 使延迟升高 | 低 | 高频小对象 workload 或 reserve refill | task magazine hit 不取 central lock；refill/drain/transition 仍可能竞争，需用实际 SMP lock/latency 数据评估 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|---|---|---|---|---|---|---|
| F-01 | central cache 无空对象 | 全部 span 为 Full 或尚未 refill | allocation miss | 请求页或返回 `NoMemory` | 3 | lock 外申请并 prepare span，发布后重试 |
| F-02 | backing 页不足 | buddy 没有满足对齐/页数的 span | refill 失败 | allocation 可能 OOM | 2 | OOM 前 detach 全部 central Empty span，重新检查 cache 后重试页来源 |
| F-03 | prepared span 无法发布 | class/token/counter 校验失败 | span 未可见 | allocation 返回 `InvalidInput` | 3 | 归还 token 的 backing pages，不留下半发布 cache 状态 |
| F-04 | shrink 无法释放内存 | 对象仍 live，span 为 Partial/Full，或并发 hardirq 重新持有 free object | 该 span 保留 | 内存压力持续 | 3 | staged full magazine flush 后只 detach `Empty`；宁可保留不可证明为空的 span，也不冒 use-after-free 风险 |
| F-05 | central lock contention | 多 CPU refill/drain/transition 同一 class | 增加等待与 IRQ-off 时间 | 吞吐/p99 降低 | 4 | task hit 用 local magazine；central transfer 每次只处理一个对象；基准验证后再宣称改善 |
| F-06 | hardirq reserve 耗尽 | hardirq burst 超过预留对象数，或任务尚未来得及补充 | allocation 返回 `NoMemory` | 调用方可能丢弃可选工作或走既有错误路径 | 3 | reserve 容量固定且有显式耗尽语义；normal task local miss/free 机会性补充，不在 hardirq 申请页 |

## 故障管理

正常 cache miss 不直接是错误：它在 class lock 外 refill。`AllocError::NoMemory` 会
触发 central empty-span reclaim，并在重新检查 cache 后重试页分配；若仍失败，错误
向上返回。`GlobalAlloc::alloc` 将最终错误记录后进入 `handle_alloc_error`。

`shrink()` 返回已归还的 base page 数；在 Buddy/TLSF build 中返回零，因为没有
central slab span。slab runtime build 会先 flush 已启动 CPU 的 task magazine 并 drain
hardirq reserve；它仍不是全系统 memory-pressure policy：没有 CPU offline 清理，也
没有触及 PCP。

## 隐私分析

本 crate 不处理、持久化或导出用户数据、身份标识或遥测。free object 的旧内容会被
intrusive freelist link 覆盖；对象重新分配前是否清零属于调用者/上层 allocator 的
语义，不应被误认为数据隔离机制。

## 已知限制

- slab task magazine hit 使用 per-CPU 双 magazine，不获取 central `SpinNoIrq`；
  reserve under-full 时只在 local miss 或 normal task free 上机会性补充。
  refill/drain、reserve 补充和 span transition 仍会竞争 central lock。
- 已启动 CPU 的 task magazine 由 generation-based IPI handshake flush；但不提供
  CPU offline、NUMA 或 memory cgroup protocol。NMI allocation 不受支持；hardirq
  reserve 耗尽时 allocation 返回 `NoMemory`。
- 仅回收确认 `Empty` 的 span；不能回收 live object、partial/full span 或 PCP。并发
  hardirq 在 reserve/central cache 中保留的对象会使对应 span 正确地继续保留。
- PCP 的 refill/drain 仍可能在 IRQ-off page allocator 临界区执行，未被本次 slab
  ownership 重构改变。
- 不提供 production double-free、错误 layout 或跨 cache free 检测。

## 审计清单

- [ ] slab layout 是否只通过 `SizeClass::from_layout()` 选择 class，而没有新建
  私有 bucket 表？
- [ ] span 页数、对齐和 header 初始化是否均来自同一个 `SizeClass` geometry？
- [ ] 是否没有在 class lock 内调用页 allocator、批量初始化或页归还？
- [ ] 是否只 detach `Empty` span，并在 detach 后、锁外归还 backing pages？
- [ ] 对 `unsafe deallocate` 的调用点是否能证明 pointer、class、layout 和对象
  生命周期匹配？
- [ ] 是否保持 KmemCache 与 `palloc` 不嵌套的锁序？
- [ ] task magazine 是否只在 current-CPU pin 下访问，hardirq 是否仅走独立 reserve，
  且 NMI contract 是否明确？
- [ ] refill/drain/reserve 补充是否没有在一次 IRQ-off 临界区内批量执行 page work
  或初始化？
- [ ] shrink/OOM 是否只回收经全 CPU 协议确认可回收的对象，而不把 local magazine
  或 reserve 当成 central free object？
- [ ] IPI handler 是否只推进 generation 状态，而把 remote mutable borrow 和 batch
  drain 留在确认 frozen 后的 task 上下文？
- [ ] CPU offline、新 CPU online 或 NUMA policy 是否在复用该协议前拥有独立的
  lifecycle ownership，而没有把 present-CPU snapshot 当成永久拓扑？
- [ ] 性能结论是否包含 SMP 吞吐、lock wait、p99 和最大 IRQ-off latency，而不是
  只报告 cache hit rate？
