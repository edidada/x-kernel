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
│   ├── kbox.rs
│   ├── kvec.rs
│   └── lib.rs
└── docs/
    ├── design.md
    └── security.md
```

当前骨架阶段覆盖：

- `AllocError`
- `AllocContext`
- `KBox<T>`
- `KVec<T>`

当前已实现的第一批 fallible API：

- `KBox::try_new`
- `KBox::try_new_uninit`
- `KVec::try_with_capacity`
- `KVec::try_reserve`
- `KVec::try_push`
- `KVec::pop`
- `KVec::truncate`
- `KVec::clear`
- `KVec::swap_remove`
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

当前 `KBox<T>` 已经通过 `kalloc::global_allocator()` 实现显式可失败构造，
成功后仍然转入 `Box<T>` 的 RAII 所有权模型。`KVec<T>` 则直接拥有 raw
buffer，并通过 `kalloc::global_allocator()` 实现显式可失败的预分配、扩容和
追加入口，失败时保持容器旧状态有效；基础删除/截断操作也由 `KVec<T>`
自带 RAII 负责元素析构。Drop 语义分别由 `Box<T>` 和 `KVec<T>` 自身维护。
