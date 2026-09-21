# 模块、可见性与依赖规则

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

## CG-MOD-001：程序项可见性应不宽于当前非测试调用方所需范围

- **规则**：新增或扩大程序项可见性时，应使用能满足当前非测试调用方的最窄可见性。
- **等级**：MUST
- **适用对象**：`fn`、`struct`、`enum`、`trait`、字段、模块、
  `pub use`、关联函数、关联类型、常量等程序项的可见性。
- **判定方法**：
  1. 搜索该程序项的所有当前调用方、引用方和 re-export。
  2. 排除 `#[cfg(test)]`、测试模块、mock、示例和文档代码。
  3. 根据剩余调用方位置判断所需最窄可见性：
     当前模块内使用应为 private；
     父子模块范围内使用应为 `pub(super)` 或 `pub(in path)`；
     当前 crate 内使用应为 `pub(crate)`；
     crate 外非测试代码使用才需要 `pub`。
  4. 如果实际可见性宽于上述范围，则违反本规则。
- **例外**：
  - 该程序项必须满足外部 trait、宏展开、平台入口、链接符号或导出 ABI 的可见性要求。
  - 该程序项虽当前未被生产代码调用，但同一 PR 同步新增了明确的公开契约文档和预期外部入口。
  - 本次 PR 未新增、扩大或依赖该可见性。
- **反例**：

```rust
pub fn rebuild_process_table(table: &mut ProcessTable) {
    table.clear_cached_slots();
}
```

该函数只在当前 crate 内被调用，却使用了 `pub`。

- **正例**：

```rust
pub(crate) fn rebuild_process_table(table: &mut ProcessTable) {
    table.clear_cached_slots();
}
```

## CG-MOD-002：workspace 已管理的依赖应使用 workspace 声明

- **规则**：成员 crate 使用 workspace 已声明的依赖时，应通过 workspace dependency
  形式引用，不应在成员 crate 中重复声明版本或 feature。
- **等级**：MUST
- **适用对象**：成员 crate 的 `Cargo.toml` 依赖声明。
- **判定方法**：
  1. 读取成员 crate 的 `Cargo.toml`。
  2. 读取 workspace 根 `Cargo.toml`。
  3. 如果依赖已在 workspace 中声明，而成员 crate 又单独写版本、path
     或重复 feature，且没有说明必须偏离 workspace 声明，则违反本规则。
- **例外**：
  - 该依赖未被 workspace 管理。
  - 成员 crate 有明确注释说明必须使用不同 package、版本、path 或 feature 组合。
- **反例**：

```toml
[dependencies]
bitflags = "2.6"
```

- **正例**：

```toml
[dependencies]
bitflags.workspace = true
```
