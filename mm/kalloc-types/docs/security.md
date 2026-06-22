# kalloc-types — 安全与可靠性分析

## 信任模型

`kalloc-types` 信任：

- `mm/kalloc` 作为底层分配后端的正确性；
- Rust `Box<T>` 当前封装的内存安全语义；
- 调用者会根据后续公开 API 的文档处理 `AllocError`。

本 crate 当前不直接信任外部输入数据内容，因为骨架阶段只定义类型边界，
不解析用户数据、不访问设备、不操作页表。

## 外部边界 / 攻击面

当前骨架阶段的外部边界很窄：

- 公共类型 `AllocError`
- 公共类型 `AllocContext`
- 公共包装类型 `KBox<T>` / `KVec<T>`
- 公共 helper `try_collect_vec`
- 公共 helper `map_alloc_error`

当前不涉及：

- 用户指针
- MMIO / PIO
- DMA
- FFI
- 中断处理
- 文件系统或网络输入解析

## unsafe 代码清单

- `src/kbox.rs`
  - `Box::from_raw`：把 `kalloc::global_allocator()` 返回的所有权重新包装成
    `Box<MaybeUninit<T>>` / `Box<T>`
  - `KBox<MaybeUninit<T>>::assume_init`：在调用者证明初始化完成后完成类型收窄
- `src/kvec.rs`
  - `slice::from_raw_parts` / `from_raw_parts_mut`：把当前已初始化前缀暴露为切片
  - `ptr::copy_nonoverlapping`：扩容时搬运已初始化元素到新缓冲区
  - `ptr::write`：向未初始化尾槽写入新元素
  - `ptr::drop_in_place`：销毁已初始化前缀

## 内存安全不变量

- `KBox<T>` 必须始终独占一个有效的 `Box<T>`。
- `KBox<MaybeUninit<T>>` 在调用 `assume_init` 前，底层分配必须仍然属于同一
  个 `Box`，且调用者必须保证对象已完全初始化。
- `KVec<T>` 必须始终维护以下不变量：
  - `ptr` 要么指向与 `cap` 对应的 live allocation，要么在零容量 / ZST 情况下
    为悬空哨兵；
  - 前 `len` 个元素始终已初始化，区间 `[len, cap)` 始终未初始化；
  - `try_reserve` / `try_push` 失败时不得破坏已有元素、长度和缓冲区所有权。
  - `pop` / `truncate` / `clear` / `swap_remove` 必须对被移除元素执行恰好一次
    所有权转移或析构，不能泄漏也不能双 drop。
- `try_collect_vec` 失败时不得向调用方暴露部分收集成功的 `KVec<T>`。
- 公共 API 不暴露内部所有权表示，避免调用者绕过后续 fallible 语义边界。
- `AllocError` 与 `AllocContext` 不携带悬垂引用或原始指针。

## 线程安全

- 当前骨架不声明新的并发不变量。
- `KBox<T>` / `KVec<T>` 的线程安全属性取决于内部元素类型和未来实现。
- 本 crate 不包含内部共享可变全局状态，因此没有额外锁顺序问题。

## 威胁分析

- 威胁：调用方在未初始化完成前错误调用 `assume_init`。
  影响：可能把未初始化对象当作 `T` 使用，形成未定义行为。
  缓解：接口使用 `MaybeUninit<T>` 显式建模未初始化状态，并把转换保留为
  `unsafe fn`，要求调用者承担初始化完成证明。

- 威胁：调用方误以为 `KVec::try_push` 失败时会返还输入值。
  影响：上层在 OOM 路径上错误复用已被释放的业务状态。
  缓解：当前 API 文档明确说明 phase 1 仅保证容器旧状态不变，不返还输入值；
  如后续需要保值错误类型，应新增显式错误枚举而不是隐式改变语义。

- 威胁：错误类型过于贫乏，未来扩展上下文语义时破坏 API。
  影响：后续实现需要重写调用边界。
  缓解：当前保留 `UnsupportedContext` 和多种 `AllocContext` 变体。

## 故障模式与影响分析（FMEA）

| 故障模式 | 触发条件 | 局部影响 | 系统影响 | 当前缓解 |
|---|---|---|---|---|
| 调用者未采用新类型层 | 继续直接使用 `Vec`/`Box` infallible API | 该路径仍可能 panic-on-OOM | 迁移范围外行为不变 | 后续任务按路径迁移 |
| `AllocContext` 语义被误解为已实现 | 调用者依赖 `NoWait`/`Atomic` | 语义假设错误 | 可能引入未来设计返工 | 文档明确仅 `Sleepable` 会在后续阶段支持 |
| 包装类型暴露过多内部细节 | 公共字段或过宽 API | 难以保持未来不变量 | 后续扩展受阻 | 当前字段私有，API 极窄 |

## 故障管理

- 当前骨架阶段不引入 panic 路径。
- `KBox::try_new` / `try_new_uninit` 在失败时返回 `AllocError`，不会把半初始化
  堆对象暴露给调用方。
- `try_collect_vec` 在失败时丢弃局部构建中的向量，不暴露部分成功状态。
- `AllocError::into_kerror` 统一映射到 `KError::NoMemory`，避免调用方各自散写。
- 后续真正的 fallible 构造/扩容 API 必须保持“失败时不暴露半初始化状态”。

## 隐私分析

本 crate 当前不处理用户数据、文件内容、网络载荷或设备隐私数据。

## 已知限制

- 当前 `KBox<T>` 已提供真正的 fallible 构造，`KVec<T>` 已通过 direct
  `kalloc` raw-buffer 管理提供真正的 fallible 预分配/扩容/追加。
- `try_collect_vec` 当前固定使用 `Sleepable` 上下文；如果未来需要显式传播
  分配上下文，需要新增独立签名而不是静默改变现有 helper 语义。
- 当前已补 UT 覆盖成功路径、上下文拒绝、RAII drop、ZST、基础删除语义，
  以及 test-only failure injection 下的确定性 OOM 路径。
- 当前没有 `Arc`、`String` 等配套类型。
- 当前没有 no-wait / atomic / reserve-backed 真实行为。

## 审计清单

- 检查公共字段是否仍然保持私有。
- 检查新增 API 是否继续把分配上下文建模为显式契约。
- 检查未来引入的 `unsafe` 是否为后续功能所必需，并补充 `SAFETY:` 说明。
- 检查调用方是否把 `UnsupportedContext` 误当作成功可用语义。
