# 并发规则

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

## CG-CONC-001：static lock 应使用 static_lock!

- **规则**：静态锁对象应使用 `static_lock!` 定义，使 `KFEAT_LOCK_STAT`
  能为该锁绑定可跟踪的 lock class。
- **等级**：MUST
- **适用对象**：新增或修改的 `static` / `static mut` 锁对象，包括
  `Mutex`、`RwLock`、`SpinNoIrq`、`SpinNoPreempt`、`SpinRaw`，
  以及包裹这些锁的静态 wrapper。
- **判定方法**：
  1. 找到新增或修改的静态锁对象。
  2. 如果静态 item 直接初始化为 `Mutex::new(...)`、`RwLock::new(...)`
     或 spin lock `::new(...)`，检查它是否位于 `static_lock! { ... }` 内。
  3. 如果静态 wrapper 形如 `static FOO: Wrapper = Wrapper(Mutex::new(...))`，
     则违反本规则；应把内部锁拆成独立 `static_lock!` item。
  4. 如果在静态初始化中使用 `const_new`，必须说明为什么需要无跟踪 const 初始化。
- **例外**：
  - heap 分配或运行时 lazy 初始化的锁，例如 `Arc::new(Mutex::new(...))`
    或 `Lazy::new(|| Mutex::new(...))`。
  - 确实必须使用无跟踪 const 初始化，并且代码注释说明了原因。
- **反例**：

```rust
static TABLE: Mutex<Table> = Mutex::new(Table::new());
```

- **正例**：

```rust
use ksync::{Mutex, static_lock};

static_lock! {
    static TABLE: Mutex<Table> = Mutex::new(Table::new());
}
```
