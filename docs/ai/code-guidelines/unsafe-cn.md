# Unsafe 规则

## 规则格式

每条规则使用以下字段：

- **规则 ID**：稳定编号，review 评论和后续统计可引用。
- **规则**：应遵守的编码要求。
- **等级**：`MUST`、`SHOULD` 或 `MAY`。
- **适用对象**：需要检查该规则的代码对象。
- **判定方法**：如何判断代码是否符合规则。
- **例外**：不按本规则报告问题的情况。
- **反例**：违反规则的典型代码。
- **正例**：符合规则的典型代码，或推荐修改方式。

## CG-UNSAFE-001：unsafe block 应有相邻的 SAFETY 证明

- **规则**：每个新增或修改的 `unsafe {}` block 前应有相邻的 `// SAFETY:`
  注释，说明该 unsafe 操作所依赖的不变量及其建立来源。
- **等级**：MUST
- **适用对象**：所有新增或修改的 `unsafe {}` block。
- **判定方法**：
  1. 检查 unsafe block 前是否有相邻 `// SAFETY:` 注释。
  2. 检查注释是否说明具体 unsafe 操作。
  3. 检查注释是否说明所需不变量，例如指针有效性、对齐、生命周期、
     aliasing、初始化状态、ABI 或并发同步。
  4. 检查注释是否指出这些不变量由哪个检查、guard、锁、状态、所有权规则
     或调用者契约建立。
  5. 缺少任一关键内容时，违反本规则。
- **例外**：
  - 本次 PR 未新增、移动、扩大或改变该 unsafe block 依赖的不变量。
  - 生成代码的 unsafe 证明由同一生成器或周边文档统一维护，且本次未改变该证明。
- **反例**：

```rust
// SAFETY: checked above.
let header = unsafe { &*(ptr as *const Header) };
```

- **正例**：

```rust
if ptr.is_null() || !ptr.is_aligned_to(core::mem::align_of::<Header>()) {
    return Err(Error::InvalidArgument);
}

// SAFETY: `ptr` was checked non-null and aligned for `Header`; the caller
// guarantees that the buffer is valid for reads of `size_of::<Header>()`.
let header = unsafe { &*(ptr as *const Header) };
```

## CG-UNSAFE-002：公开 unsafe API 应声明调用者或实现者义务

- **规则**：公开 `unsafe fn`、`unsafe trait` 和 `unsafe impl`
  应在直接文档或相邻注释中声明调用者或实现者必须满足的安全义务。
- **等级**：MUST
- **适用对象**：新增或修改的公开 `unsafe fn`、公开 `unsafe trait`
  和 `unsafe impl`。
- **判定方法**：
  1. 对公开 `unsafe fn` 和公开 `unsafe trait`，检查其直接 rustdoc 是否包含
     非空 `# Safety` 小节。
  2. 对 `unsafe impl`，检查相邻注释或文档是否说明该类型为什么满足 unsafe trait 的要求。
  3. 安全义务只写“调用者保证安全”“实现是安全的”等空泛表述时，视为不合格。
  4. 文档应说明相关 validity、lifetime、aliasing、initialization、ABI
     或并发义务中适用于该 API 的部分。
- **例外**：
  - private unsafe item 的义务完全由本地 `SAFETY:` 注释覆盖。
  - 生成代码的 unsafe 契约由同一生成器或周边文档统一维护，且本次未改变该契约。
- **反例**：

```rust
/// Creates a buffer from a raw address.
pub unsafe fn from_raw(addr: usize, len: usize) -> UserBuffer {
    UserBuffer { addr, len }
}
```

- **正例**：

```rust
/// Creates a buffer from a raw address.
///
/// # Safety
///
/// `addr..addr + len` must be a valid user memory range for the returned
/// buffer's lifetime, and the caller must not create overlapping mutable access
/// through another buffer.
pub unsafe fn from_raw(addr: usize, len: usize) -> UserBuffer {
    UserBuffer { addr, len }
}
```
