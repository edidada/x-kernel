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
│   └── try_clone.rs
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
- `KWeak::upgrade` (→ `Option<KArc<T>>`)
- `KArc::try_unwrap`
- `KArc::ptr_eq` / `KWeak::ptr_eq`
- `KArc::strong_count` / `KArc::weak_count`
- `KString::try_with_capacity`
- `KString::try_from_str`
- `KString::try_push` / `KString::try_push_str`
- `KString::try_push_fmt`
- `KVecDeque::try_with_capacity`
- `KVecDeque::try_push_back` / `try_push_front`
- `KVecDeque::pop_front` / `pop_back`
- `KHashMap::try_with_capacity` / `try_reserve`
- `KHashMap::try_insert` / `get` / `remove` / `iter`
- `KBTreeMap::try_new` / `try_insert` / `try_entry`
- `KBTreeMap::get` / `remove` / `iter` / `range`
- `KBTreeSet::try_insert` / `contains` / `remove` / `iter`
- `KBinaryHeap::try_push` / `pop` / `peek`
- `TryClone::try_clone`
- `kvec![a, b, c]` — 返回 `Result<KVec<T>, AllocError>`
- `try_collect_vec`
- `map_alloc_error`
- test-only failure injection for deterministic OOM-path coverage

当前阶段不覆盖：

- OOM kill
- reclaim
- reserve-backed no-fail 路径

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

当前阶段 `KBox` 和 `KVec` 都提供真实的 fallible 分配入口。上下文约束也
因此开始变成运行时契约：当前实现仅接受 `Sleepable`，其他上下文显式返回
`UnsupportedContext`。`AllocContext` 仍保留了未来上下文维度：

- `Sleepable`
- `NoWait`
- `Atomic`
- `ReservedCritical`

后续真正的构造/扩容 API 在第一实现里只会保证 `Sleepable`。

## 并发模型

- 本 crate 不维护全局共享状态。
- `AllocError` 和 `AllocContext` 是纯值类型。
- `KBox<T>` / `KVec<T>` 的并发属性由内部元素类型决定；
  当前骨架不主动声明额外的 `Send` / `Sync` 行为。

## 设计决策

### 为什么放在 `mm/` 下而不是 `util/`

这层定义的是 `kalloc` 的公共语义延伸，而不是普通工具函数集合。
它与分配失败、上下文约束、后续 OOM/reclaim 集成直接相关，
因此应由内存管理域拥有。

### 为什么命名为 `kalloc-types`

`kheap` 容易和整个内核堆系统混淆；`kalloc-types` 更准确表达：
这里定义的是围绕 `kalloc` 的类型/API 边界，而不是新的分配器实现。

### 为什么骨架阶段先暴露类型名而不暴露构造 API

当前任务目标是冻结边界，而不是一次性完成可失败分配实现。
先固定类型名和错误模型，可以让后续任务在不改导入边界的前提下
逐步补齐真正的 fallible 行为。

## Drop / 资源释放

`KBox<T>` 通过 `kalloc::global_allocator()` 实现显式可失败构造，成功后转入
`Box<T>` 的 RAII 所有权模型。`KVec<T>` / `KVecDeque<T>` 直接拥有 raw buffer，
通过 `kalloc::global_allocator()` 实现 fallible 分配，失败时保持容器旧状态有效。
`KArc<T>` 通过原子引用计数管理共享所有权，`Drop` 时递减计数并在最后一个引用
释放时回收分配。`KBTreeMap` / `KBTreeSet` 内部使用 `KBox<T>` 管理 B 树节点，
删除操作自动回收节点内存。

## B 树移植说明

`KBTreeMap` / `KBTreeSet` 的实现移植自标准库 `std::collections::BTreeMap` /
`BTreeSet`（经 `fallible_collections` 适配）。标准库的 `BTreeMap` 每次插入
都可能在内部分配新节点，且没有 `try_reserve` 等 fallible API。移植版本将所有
内部分配点（`Root::new_leaf`、`node.push_level`、节点分裂）改为通过
`KBox::try_new` 分配，使 `try_insert` / `try_entry` 等方法能够返回
`AllocError` 而非 panic。

移植代码中保留了标准库的 unsafe 块，它们的正确性由上游保证。`node.rs`、
`map.rs` 中通过 `#![allow]` 屏蔽了部分移植代码特有的 lint（如
`undocumented_unsafe_blocks`），这些 unsafe 块的 SAFETY 注释需 B 树算法
专业知识，不宜机械添加。
