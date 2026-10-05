# Lint 政策与规则覆盖

本文件是 lint 的唯一政策入口：等级在根 `Cargo.toml` 的 `[workspace.lints]`，
参数（阈值、禁用列表、测试豁免）在根 `clippy.toml`。
lint 已强制的规则不再写进 `docs/ai/skills/code-guidelines/`。
`missing_docs` 和 `rustdoc::*` 归 module-docs 轨道（`docs/ai/review/module-docs/`）。

## 等级与检查范围

- MUST 对应 `deny`，SHOULD 对应 `warn`；`make clippy` 带 `-D warnings`，两者都阻塞合入。
- `make clippy` 带 `--cfg unittest` 检查选定平台的依赖图（本地库、proc-macro、host `build.rs`），
  不带 `--all-targets`。因此 `cfg(not(unittest))` 代码、未选中的测试/bench target 从不被 lint。
- `xtask` 是独立 workspace，不继承根 lint；单独运行
  `cargo clippy --manifest-path xtask/Cargo.toml -p xkmake --no-deps`。

## 例外写法

- 在最窄作用域写 `#[expect(<lint>, reason = "...")]`，reason 说明此处为何不适用，不写 "needed"、"legacy" 等空话。
- `#[allow]` 只用于 `expect` 不可靠处（只在部分 cfg/feature/平台命中、宏展开、
  rust-bindgen 等生成代码），同样带 reason。
- reason 写法：`TODO(cg): ` 应修未修，写明修复方式；`test scenario: ` 测试宏未覆盖的测试代码；
  `Fatal boot: ` 写明启动失败条件、无恢复办法、无错误返回通道；内部不变量写明不变量及其成立原因。
- rustc 忽略宏调用语句（`panic!();`、`todo!();`）上的 `#[expect]`，改放在最小的外层项（`match` 分支或函数）。

## 按 crate 关闭

- 只允许 case (a) 规则不适用（如以终止构建为预期失败的 host `build.rs`、纯测试 crate），
  或 case (b) 存量密集：Jenkins 各 `make clippy` 配置诊断并集中每 crate ≥10 处（单条 lint 可更严）。
- 在每个实际被 lint 的 target 根写 `#![allow(<lint>, reason = "case (a|b): <具体原因>")]`，
  不用 `#![expect]`、不要求 issue，未被 lint 的 target 不预先豁免；并登记在下表该 lint 行。
- 逐点修复优先，门槛不是新增配额；case (b) crate 的新增代码仍按原规则审查。
- 另可在 crate 根写过渡豁免 `#![expect(<lint>, reason = "TODO(cg): legacy cleanup, tracked in <issue>")]`，须有 issue。

## 测试代码

- `#[def_test]` / `#[mod_test]`（`util/macros`）豁免 `unwrap_used`、`expect_used`、`panic`、`too_many_lines`，
  `#[mod_test]` 另豁免 `missing_docs`。测试代码不手写 `allow`/`expect`；内联测试模块用 `#[mod_test]`。
- 文件模块（`mod name;`）不能用属性宏（E0658）：写成 `#[mod_test] mod tests { include!("tests.rs"); }`
  （被 include 的文件不重复声明 cfg 或模块文档），或在辅助函数上逐点写 `test scenario: ` 例外。
- host 侧 `#[test]` 由 `clippy.toml` 的 `allow-*-in-tests` 覆盖。

## 新增 lint 与工具链升级

- 一条 lint 一个提交：启用（含 `clippy.toml` 参数）、处理全部存量、在下表登记、从 skill 删除被覆盖的原文。
- 验证：`Jenkinsfile` 中每个 `make clippy` 配置；aarch64 `qemu_defconfig` 的 `make clippy UNITTEST=y`；
  命中随 cfg/feature 变化的 lint 另分别开、关可选 feature 跑。
  loongarch64 目前有无关的既有失败，比较启用前后的错误集合即可。
- 修改 `rust-toolchain.toml` 时跑同一矩阵，并确认本文件中的 lint 名和 `clippy.toml` 键仍存在。

## 与 AI 审查的分工

- full 的规则不再审查；partial 只审"覆盖"列留下的判断。
- case (b) 关闭的 crate，新增代码仍按该行规则审查；case (a) 按不适用范围处理。
- 未被 lint 覆盖的规则以 skill 为准。

## 已启用 lint

"规则"列为已从 skill 删除的原文要点；"（默认）"指 rustc/clippy 默认开启、删除原文时所依赖的 lint。

| lint | 等级与参数 | 规则 | 覆盖 | 例外；关闭的 crate |
|---|---|---|---|---|
| `clippy::panic`<br>`clippy::unwrap_used`<br>`clippy::expect_used` | warn；`allow-*-in-tests` | "avoid `.unwrap()` and hidden panics where failure is a legitimate possibility"（`error-handling-and-logging.md`、`SKILL.md`） | partial：失败是否合法由审查判断 | 合法失败（输入、资源、环境）返回错误。逐点例外：内部不变量、`Fatal boot: `（不为此改启动接口或关闭启动 crate）、`test scenario: `；`panic` 另允许 `TODO(cg): `（失败合法但现接口无法上报）。<br>`panic` (b)：`acpi`、`kcpu`。<br>`unwrap_used` (a)：`kbuild_config`、`ksignal` 的 `build.rs`；(b)：`alloc-engine`、`kworkerpool_ng`、`virtio`、`kworkqueue_ng`、`kext4`、`ksyscall`。<br>`expect_used` (a)：`kbuild_config`、`backtrace` 的 `build.rs`，`unittest_support`；(b)：`kalloc`、`khal`、`ktask`、`kworkerpool_ng`、`memspace`、`virtio`、`kworkqueue_ng`、`kvfs`、`kprocess`、`knet`、`posix-process`。<br>(b) 均只在 `src/lib.rs`；`kernel-boot`、`fs_boot` 不关闭 |
| `clippy::todo`<br>`clippy::unimplemented` | deny | 同上 | partial | `todo`：仅 `TODO(cg): ` 写明缺失实现，保留 `todo!` 不改成静默空操作；`unimplemented`：无 |
| `clippy::allow_attributes`<br>`clippy::allow_attributes_without_reason` | deny | "suppress lints at the narrowest practical scope"（`modules-and-attributes.md`） | partial：不查内层 `#![allow]`；作用域与 reason 是否属实由审查判断 | 必需的外层 `allow` 在同一作用域配 `#[expect(clippy::allow_attributes, reason)]` 说明 `expect` 为何不可靠（Kconfig emitter 统一生成）。`expect(dead_code)` 只用于简单、有意、很可能被用上的死代码，reason 说明何时使用。(b)：`kext4`、`tee_kernel`（两条均关，门槛 ≥20） |
| `clippy::multiple_unsafe_ops_per_block`<br>`unsafe_op_in_unsafe_fn` | deny | "keep each `unsafe` block as small as practical"，`unsafe fn` 内也用最小 `unsafe {}`（`unsafety.md`） | partial：块内夹带的安全逻辑按 `unsafety.md` Invalid Patterns 审查 | 宏内 unsafe 序列用最窄 `expect`（不可靠时 `allow`） |
| `clippy::undocumented_unsafe_blocks` | deny | 每个 `unsafe` 块前有 `// SAFETY:`，`unsafe impl` 有安全论证（`unsafety.md`） | partial：注释内容按 `unsafety.md` SAFETY Comment Standard 审查 | 无 |
| `clippy::missing_safety_doc` | deny；`check-private-items = true` | `unsafe fn` / `unsafe trait` 必须有 `# Safety`（`unsafety.md`） | full（含私有项） | 仅 bindgen 绑定模块 `allow` |
| `clippy::disallowed_methods`<br>`invalid_value`（默认）<br>`clippy::uninit_assumed_init`（默认） | warn；`disallowed-methods` 禁 `core::mem::zeroed` | 不对位模式未经类型契约证明的类型用 `mem::zeroed()` / `assume_init()`（`unsafety.md`） | partial：其余 `assume_init()` 由 `// SAFETY:` 证明已初始化 | 优先安全构造；`zeroed` 的 reason 说明全零对该类型有效且由类型契约保证 |
| `unreachable_pub` | warn | "default to the narrowest visibility that works"（`modules-and-attributes.md`、`naming-and-structure.md`） | partial：`pub(crate)` 能否收窄由审查判断 | cfg/feature 下的公开 API 和导出宏辅助项保留可达性；宏生成项（如 `unittest::tests_name!`）、bindgen 模块用 `allow`；需 feature 扫描 |
| `clippy::too_many_lines` | warn；阈值 100 | "keep functions small and focused"（`api-design.md`） | partial：单一职责由审查判断 | 逐函数，只限：线性分发/注册表（一个 `match` 或一串独立注册，每分支约 10 行内，不共享可变状态，reason 以 "linear ... table" 开头）；测试场景；`TODO(cg): legacy` 写明拆分方式（拆分另行提交并审查锁、事务、清理顺序）。(b)：`kext4` |
| `clippy::excessive_nesting` | warn；阈值 6（`mod`/`impl`/函数体计层） | "minimize unnecessary nesting"（`api-design.md`） | partial | 先用提前返回、`let ... else`、提取函数展平，不改变锁作用域与清理顺序；例外只限测试场景和 `TODO(cg): legacy` |
| `clippy::fn_params_excessive_bools` | warn；`max-fn-params-bools = 3` | "avoid boolean arguments when the call site becomes unclear"（`api-design.md`） | partial：阈值以下的单个 `bool` 由审查判断 | 无；拆成独立入口或改用 enum/flags |
| `clippy::linkedlist` | warn | 只在元素无法自带链接时才用 `alloc::collections::LinkedList`（`performance-and-resources.md`） | partial | reason 说明为何不能用 `linked_list_r4l` |
| `clippy::upper_case_acronyms`<br>`non_camel_case_types`（默认） | warn；`upper-case-acronyms-aggressive = true` | 类型名用 CamelCase，缩写也按词首大写（`IoMemoryArea`；`naming-and-structure.md`） | full | 镜像 Linux/硬件 ABI 名称的类型 |
| `clippy::question_mark`（默认） | warn | "use `?` for ordinary error propagation"（`error-handling-and-logging.md`） | partial：只识别可直接改写的提前返回 | — |
| `clippy::unused_result_ok` | warn | 无原文：不用 `.ok();` 丢弃 `Result` | full | 无；有意丢弃写 `let _ = expr;`，不明显时注释原因 |
| `clippy::used_underscore_binding` | warn | 无原文：被使用的绑定不加 `_` 前缀 | full | 无 |
| `unexpected_cfgs` | warn；`check-cfg = ["cfg(unittest)"]` | 无原文：未知 cfg 名或 feature 值 | full | 无 |

`avoid-breaking-exported-api = false` 让上表及其他默认 clippy lint 也检查 pub 项。

## 辅助性 lint

只查原文的一个侧面，原文保留在 skill：

- `static_mut_refs`（默认）：只拦对 `static mut` 取引用，不拦声明。
- `improper_ctypes` / `improper_ctypes_definitions`（默认）：只查 FFI 签名类型。
- `clippy::wrong_self_convention`（默认）：只查 `as_`/`to_`/`into_` 前缀与 `self` 形式。
- `clippy::too_many_arguments`（默认）：只数参数个数。
- `unsafe_code`：由 safe-only crate 自行在根部 `#![forbid(unsafe_code)]`，不统一配置。

## 不采用的 lint

`clippy::let_underscore_must_use`：有意丢弃就写 `let _ = expr;`，原因不明显时注释说明；
`.ok();` 已由 `unused_result_ok` 禁止。Rust-for-Linux 两者都未启用。
