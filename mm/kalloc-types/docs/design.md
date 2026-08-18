# kalloc-types — 设计文档

## 定位

`kalloc-types` 是 `mm/kalloc` 之上的类型边界层。它不拥有底层分配器状态，
只负责定义“显式可失败堆分配”这一公共语义的类型入口，供 `core`、
`process`、`fs`、`net`、`drivers` 等多个 peer crate 共享。

## 背景

当前 x-kernel 的底层分配器已经可以返回错误，但大量 Rust 堆对象仍然经由
全局分配器走 infallible 接口；一旦分配失败，就会在更高层触发 panic。
要把“内存不足”改造成可传播错误，首先需要一个稳定的公共类型边界，把
错误模型和后续可失败堆对象 API 固定下来。

## 范围

涉及的源文件：

```text
mm/kalloc-types/
├── Cargo.toml
├── src/
│   ├── alloc_error.rs
│   ├── collect.rs
│   ├── context.rs
│   ├── fault_injection.rs
│   ├── karc.rs
│   ├── kbinaryheap.rs
│   ├── kbox.rs
│   ├── kbtree/
│   │   ├── mod.rs
│   │   ├── map.rs
│   │   ├── node.rs
│   │   ├── search.rs
│   │   └── set.rs
│   ├── khashmap.rs
│   ├── kstring.rs
│   ├── kvec.rs
│   ├── kvecdeque.rs
│   ├── lib.rs
│   └── try_traits.rs
└── docs/
    ├── design.md
    └── security.md
```

当前阶段覆盖：

- `AllocError` / `AllocContext`
- `KBox<T>`
- `KVec<T>` / `KVecDeque<T>`
- `KArc<T>` / `KWeak<T>`
- `KString`
- `KHashMap<K, V>` — fallible 哈希表，包装 `hashbrown::HashMap`
- `KBTreeMap<K, V>` / `KBTreeSet<T>` — fallible B 树，fork 自 std 实现
- `KBinaryHeap<T>` — fallible 优先队列
- `TryClone` — fallible clone trait
- `kvec![]` — fallible 向量构造宏

当前已实现的 fallible API：

- `KBox::try_new`
- `KBox::try_new_uninit`
- `KVec::try_with_capacity`
- `KVec::try_reserve`
- `KVec::try_push`
- `KVec::try_extend_from_slice`
- `KVec::try_extend_cloned_from_slice`
- `KVec::pop`
- `KVec::truncate`
- `KVec::clear`
- `KVec::swap_remove`
- `KArc::try_new`
- `KArc::try_new_cyclic`
- `KArc::downgrade` (→ `KWeak<T>`)
- `KWeak::new` / `Default` (non-allocating empty weak owner)
- `KWeak::upgrade` (→ `Option<KArc<T>>`)
- `KArc::try_unwrap`
- `KArc::ptr_eq` / `KWeak::ptr_eq`
- `KArc::strong_count` / `KArc::weak_count`
- `KString::try_with_capacity`
- `KString::try_from_str`
- `KString::try_reserve`
- `KString::try_push` / `KString::try_push_str`
- `KString::try_push_fmt`
- `KVecDeque::try_with_capacity`
- `KVecDeque::try_reserve`
- `KVecDeque::try_push_back` / `try_push_front`
- `KVecDeque::pop_front` / `pop_back`
- `KHashMap::try_with_capacity` / `try_reserve`
- `KHashMap::try_insert` / `get` / `remove` / `iter`
- `KBTreeMap::new` / `try_insert` / `try_entry`
- `KBTreeMap::get` / `remove` / `iter` / `range`
- `KBTreeSet::try_insert` / `contains` / `remove` / `iter`
- `KBinaryHeap::try_with_capacity` / `try_reserve` / `try_push` / `pop` / `peek`
- `TryClone::try_clone`
- `kvec![a, b, c]` — 返回 `Result<KVec<T>, AllocError>`
- `try_collect_vec`
- test-only failure injection for deterministic OOM-path coverage

## 公开 API 契约矩阵

下面按**所有公开 API 的分配语义**归类；同一格中的多个方法共享同一个
失败/上下文契约。这里的“任意上下文”仅表示该次调用能够证明不触发 allocator，
并不放宽其调用者本身的锁、IRQ 或对象生命周期约束。

| 类型 / helper | 可分配入口（返回 `Result`） | 明确无新分配入口 | 失败原子性与上下文 |
|---|---|---|---|
| `KBox<T>` | `try_new`、`try_new_uninit` | `into_inner`、`into_raw`、`leak`、`from_raw`、`assume_init`、`Deref` | 非 ZST 仅 `Sleepable`；构造失败不暴露半初始化对象。ZST 不分配，可在任意上下文构造。 |
| `KArc<T>` / `KWeak<T>` | `KArc::try_new`、`try_new_cyclic` | `KWeak::new` / `Default`、`Clone`、`downgrade`、`upgrade`、`try_unwrap`、计数、`ptr_eq`、`Deref` | owner allocation 仅 `Sleepable`；强/弱引用复制不分配。空 weak 永不 upgrade。`try_new_cyclic` 在 data 初始化完成前不允许 weak upgrade。 |
| `KVec<T>` | `try_with_capacity`、`try_reserve`、容量不足时的 `try_push`、`try_extend_from_slice`、`try_extend_cloned_from_slice` | `new`/`Default`、容量足够时上述写入、`pop`、`truncate`、`clear`、`swap_remove`、slice/iterator API | 扩容失败保持旧 buffer、长度和已有前缀；`try_extend_cloned_from_slice` 先 staging，因此元素 clone 失败也不发布部分追加。 |
| `KVecDeque<T>` | `try_with_capacity`、`try_reserve`、容量不足时 `try_push_front` / `try_push_back` | `new`/`Default`、容量足够时 push、`pop_front` / `pop_back`、`clear`、`as_slices`、`iter` | 扩容失败保持 FIFO 顺序和原内容；预留后的环形队列写入可在任意上下文执行。 |
| `KString` | `try_with_capacity`、`try_from_str`、`try_reserve`、容量不足时 `try_push` / `try_push_str` / `try_push_fmt` | `new`/`Default`、容量足够时 append、`from_utf8`、`clear`、`as_str` / `as_bytes` | 以 `KVec<u8>` 保存 UTF-8；增长失败保持原字符串，`TryClone` 也是 fallible。 |
| `KHashMap<K, V>` | `try_with_capacity`、`try_reserve`、新 key 的 `try_insert` | `new`/`Default`、lookup/iteration/remove/clear、已有 key replacement、容量足够的新 key insertion | `try_reserve` 成功后才 insert；新 key OOM 保持既有 entries。已有 key replacement 不调用 allocator，可在任意上下文执行。 |
| `KBTreeMap<K, V>` | `try_insert` / `try_insert_in`、`try_entry` / `try_entry_in`、`split_off` / `try_split_off_in`、`TryClone` / `TryExtend` / `TryFromIterator` | `new`、lookup/range/iteration/remove/clear、已有 key 的 `try_insert_in` replacement、returned `Entry` 的已预留 insert | 新 key insertion 在 mutation 前预留 root/split 链，OOM 时 tree 不变；`try_entry_in` 因可能返回 vacant entry 而保守要求 `Sleepable`。 |
| `KBTreeSet<T>` | `try_insert` / `try_insert_in`、`replace` / `replace_in`、`try_split_off` / `try_split_off_in`、集合 `Try*` traits | `new`、lookup/range/iteration/remove/clear、duplicate `try_insert_in`、present-value `replace_in` | 复用 map 的 reservation；duplicate/replacement 不分配，新值 OOM 不发布。 |
| `KBinaryHeap<T>` | `try_with_capacity`、`try_reserve`、容量不足时 `try_push` | `new`/`Default`、容量足够时 push、`peek`、`pop`、`clear` | storage 直接是 `KVec`，heap sift 不分配；增长失败保留原 heap。 |
| `try_collect_vec`、`TryCollect`、`TryFromIterator`、`TryExtend`、`TryClone` | 所有结果构造可能分配 | 不含隐式 `collect` / `Clone` 回退 | trait convenience APIs 目前固定用 `Sleepable`；需要携带上下文的调用点应直接使用容器的 `_in` / `try_*(_, context)` API。 |
| `kvec!` / `vec!`、`kformat!` / `format!` | 宏返回 `Result`；`kformat!` 接收显式 context | 空 `kvec![]` 不分配 | `kvec!` / 导入的 `vec!` 默认 `Sleepable`；用户可达路径优先使用带 context 的方法或 `kformat!`，不得回落到 alloc 宏。 |

`KBTreeMap::try_entry_in()` 的返回 `Entry` 已完成可能需要的 root materialize；
因此随后 `VacantEntry::try_insert()` 不会再遇到 OOM。相反，`try_insert_in()`
先检查 existing key，所以调用方在 `Atomic` / `NoWait` 场景要更新已有值时不必因为
保守的 entry API 被错误拒绝。

`AllocError::NoMemory` 通过 `From<AllocError> for KError` 变成
`KError::NoMemory`；`UnsupportedContext` 则保留为 `KError::WouldBlock`。子模块
只返回 `KResult`，ABI/syscall 边界才把 `KError::NoMemory` 编码为 `-ENOMEM`。

为降低迁移成本，crate 同时导出与 alloc 生态同名的类型别名：`Vec`、
`VecDeque`、`String`、`Box`、`Arc`、`Weak`、`HashMap`、`BTreeMap`、
`BTreeSet` 和 `BinaryHeap`。这些别名只提供显式 `try_*` 的增长/构造入口；
调用方不能把普通 `push`、`insert` 当作无错误操作使用。

当前阶段不覆盖：

- OOM kill
- reclaim
- reserve-backed no-fail 路径
- `KBTreeMap` / `KBTreeSet` 的 `append`：在无 `Clone` 约束下，尚不能同时做到
  线性合并和 OOM 失败时完整保留两个输入树，因此不暴露看似可恢复的 API。
- `KArc<dyn Trait>` / unsized owner：当前 `KArc` 的 allocation layout 只支持 sized
  `T`，不能替代已有 `Arc<dyn MappingViewNotifier>` 等 trait-object owner；这类边界
  必须显式保留为后续迁移项，不能被误标为 fallible。

## 架构

```text
subsystem callers
    |
    v
kalloc-types
    |
    v
kalloc
    |
    v
alloc-engine / page allocator
```

`kalloc-types` 的责任是把“显式可失败分配”建模成公共类型层，而不是把
`kalloc` 直接暴露为高层业务 API。

辅助 helper 也留在本 crate 中，因为它们仍然属于这层 owned boundary：
避免 `collect::<Vec<_>>()` 隐式 panic，以及统一 `AllocError -> KError`
 的边界映射。

## 调用约束 / 执行上下文

所有公开容器的构造、预留和增长入口都使用真实的 fallible 分配。上下文约束
也是运行时契约：当前仅**真正发起新分配**的路径接受 `Sleepable`；零容量构造、
已经预留容量内的 `push`/`extend`、以及不改变表容量的 map value replacement
不发起分配，因此可在所有 `AllocContext` 中执行。需要新 allocation 的
`NoWait`、`Atomic` 和 `ReservedCritical` 请求会显式返回
`UnsupportedContext`，并在转换到 `KError` 时保留为 `WouldBlock`，不会伪装成
内存耗尽。`AllocContext` 仍保留了未来上下文维度：

- `Sleepable`
- `NoWait`
- `Atomic`
- `ReservedCritical`

后续真正的构造/扩容 API 在第一实现里只会保证 `Sleepable`；预留容量不是
no-wait allocator 的替代，而是调用方在睡眠上下文中先完成资源准备后的显式
使用模式。

## MM 迁移与验证矩阵

本阶段不以“把每一个 `alloc` 容器机械替换”为目标，而是把每个自定义类型放进
一个能保持原有锁与提交顺序的 MM 场景。真实路径和组合式验证分别标明，避免把
测试模型误说成完整内核迁移。

| `kalloc-types` 能力 | MM 落点 | OOM/语义验证 |
|---|---|---|
| `KVec<PhysAddr>` | `memspace::SharedPages::phys_pages` | 先预留所有 bookkeeping，再取得物理 frame；容量或 append owner 失败时没有泄漏 frame。 |
| `KArc<SharedPages>` | `SharedBackend::pages` | shared page-owner construction失败通过 `KResult` 回传，RAII 回收已取得 frame。 |
| `KArc<InvalidateSink>` / `KWeak<InvalidateSink>` | `MmSpace::invalidate_sink` 与 `InvalidateHandle` | `MmSpace::new_empty_*()` 的 owner allocation 可返回 `KError::NoMemory`；clone/downgrade 不分配。 |
| `KArc<Anon*ViewRegistration>` | shared/private anon view guard | guard 的 lifetime/unregister 语义不变，guard owner OOM 不会把 view 发布到对象。 |
| `KHashMap<MappingViewId, RegisteredView>` | `AnonSharedObject::views` | `try_insert` 成功前不发布；hash table OOM 返回 `KError::NoMemory`。 |
| `KBTreeMap<MappingViewId, RegisteredView>` 与内部 `KBox` node | `AnonPrivateObject::views` | `try_insert_in` 在 mutation 前完成 B-tree node reservation；OOM 后无 view。 |
| `KString` + `KArc<KString>` | file-backed `FileMappingInfo.path`，并由 procfs `/proc/<pid>/maps` 读取 | pathname snapshot 的 string 或 shared owner OOM 都变成 `KError::NoMemory`；VMA clone/split 与 procfs snapshot 只 clone owner。 |
| `KVecDeque` | MM mapping-plan 组合测试 | 真实 `InvalidateSink` 仍持有 `SpinNoIrq`，不能在锁内把可能 sleep 的扩容替换进去；测试验证预留后的 atomic FIFO 写入和 OOM 返回。 |
| `KBinaryHeap` | MM mapping-plan 组合测试 | 当前 MM 没有一个已有的 priority queue；测试验证它只经 `KVec` 分配、sift 不分配。 |
| `KBTreeSet` | MM mapping-plan 组合测试 | `pagecache::dirty_pages` 的现有 API 含无错误返回的 dirty reconciliation；在没有完整事务/`KResult` 传播前不强迁，测试验证 duplicate/new-value 语义。 |

`mapping_plan_composes_every_fallible_heap_type` 与串行
`mapping_plan_propagates_injected_oom_from_every_allocation_stage` 将上述所有 type
放进一个 VMA-like metadata plan。后者从每一个 test-only allocation hook 位置注入
失败，要求只得到 `AllocError::NoMemory` 或完整 plan，绝不返回部分 plan。各类型
自身还保留 unit tests，覆盖 grow、replace、split、ZST、weak upgrade 与 failure
atomicity。

`KVecDeque`、`KBinaryHeap`、`KBTreeSet` 的“组合测试”不是迁移完成宣言：它只是
验证 type surface 可以共同承载 MM metadata。把它们塞入不匹配的数据结构或持有
spinlock 的路径会引入比原 OOM 问题更严重的 context/locking bug，因此这些真实
迁移必须等待相应调用链获得 `KResult` 和 sleepable/reserved allocation contract。

## 并发模型

- 本 crate 不维护常规运行时全局共享状态；test-only OOM 注入使用一个串行测试
  使用的原子计数器。
- `AllocError` 和 `AllocContext` 是纯值类型。
- `KBox<T>`、`KVec<T>` 和 `KVecDeque<T>` 在元素满足对应 `Send` / `Sync`
  约束时可安全跨线程传递/共享。
- `KArc<T>` / `KWeak<T>` 使用原子强/弱引用计数；共享 `T` 仍要求 `T: Sync`。

## 设计决策

### 为什么放在 `mm/` 下而不是 `util/`

这层定义的是 `kalloc` 的公共语义延伸，而不是普通工具函数集合。
它与分配失败、上下文约束、后续 OOM/reclaim 集成直接相关，
因此应由内存管理域拥有。

### 为什么命名为 `kalloc-types`

`kheap` 容易和整个内核堆系统混淆；`kalloc-types` 更准确表达：
这里定义的是围绕 `kalloc` 的类型/API 边界，而不是新的分配器实现。

### 为什么以显式 `try_*` 作为能力边界

仅替换类型名不能阻止隐式扩容。每个可能分配的构造、预留、push/insert 或
collect 操作必须在 API 形状上返回 `Result`；容量已预留后的普通写入才可以
作为无分配步骤执行。这样调用方可以在提交状态改变前处理 OOM。

## Drop / 资源释放

`KBox<T>` 直接拥有由 `kalloc::global_allocator()` 分配的 raw storage，并通过
RAII 在 drop 时析构对象和释放 storage。`KVec<T>` / `KVecDeque<T>` 直接拥有 raw buffer，
通过 `kalloc::global_allocator()` 实现 fallible 分配，失败时保持容器旧状态有效。
`KArc<T>` 通过原子引用计数管理共享所有权，`Drop` 时递减计数并在最后一个引用
释放时回收分配。`KBTreeMap` / `KBTreeSet` 内部使用 `KBox<T>` 管理 B 树节点；
`try_insert` 会在第一次树结构变更前预留整条潜在 split 链所需节点，因此预留
失败时 map 保持不变。

## B 树移植说明

`KBTreeMap` / `KBTreeSet` 的实现移植自标准库 `std::collections::BTreeMap` /
`BTreeSet`（经 `fallible_collections` 适配）。标准库的 `BTreeMap` 每次插入
都可能在内部分配新节点，且没有 `try_reserve` 等 fallible API。移植版本将所有
内部分配点（`Root::new_leaf`、`node.push_level`、节点分裂）改为通过
`KBox::try_new` 分配。`try_insert` 在 mutation 前先按满节点路径预留 leaf、
internal 和新 root 节点；因此任一预留分配失败都发生在 tree 仍未改变时。
为避免把数据丢失伪装成 recoverable error，当前不导出 `try_append`；未来必须先
实现完整预留/提交或 rollback，再恢复该接口。

移植代码中保留了标准库来源的 raw-node 算法。它不是自动继承上游安全保证：
本 crate 仍需对布局、初始化、父子链接和 OOM 提交顺序单独审计。`node.rs`、
`map.rs` 中的历史 lint 豁免是待继续收敛的审计债务，不能作为新增 unsafe 的
豁免理由。
