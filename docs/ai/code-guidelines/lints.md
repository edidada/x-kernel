# Lint 政策与待迁入表

lint 等级集中在根 `Cargo.toml` 的 `[workspace.lints]`，
lint 参数（阈值、禁用列表、测试豁免）集中在根 `clippy.toml`。
凡是 lint 能强制的规则，不再写进 `docs/ai/skills/code-guidelines/`，
AI 编写与审查也不重复检查。
本文件是这些 lint 的唯一政策入口。

## Lint 政策

- **等级映射**：规范中的 MUST 对应 `deny`，SHOULD 对应 `warn`。
  门禁 `make clippy` 带 `-D warnings`，两者都阻塞合入；
  等级只表达规则强度。
- **例外写法**：唯一写法是在最窄作用域上
  `#[expect(<lint>, reason = "...")]`。
  reason 要说明此处为什么不适用，不写 "needed"、"legacy" 之类的空话。
  `#[allow]` 只用于 `expect` 不可靠的场合：
  lint 只在部分 cfg/feature/平台下触发，或代码由宏展开或生成工具（如 rust-bindgen）生成；
  同样必须带 reason。
- **过渡豁免**：存量来不及清理时，只能在 crate 根写
  `#![expect(<lint>, reason = "TODO(cg): legacy cleanup, tracked in <issue>")]`，
  并且必须有对应 issue。
  清理完成后 `expect` 不再命中，会触发 `unfulfilled_lint_expectations`，届时删除。
- **新增 lint**：一条 lint 一个提交，提交内容包括：
  `[workspace.lints]` 一行（及需要的 `clippy.toml` 参数）、
  全部存量处理（修复、逐点 `expect` 或 crate 根过渡豁免）、
  从本文件待迁入表删除对应行；
  提交说明写启用动机和存量处理方式。
  落地前在 Jenkins 运行 `make clippy` 的六个配置上各跑一次 `make clippy`
  （以 `Jenkinsfile` 的构建检查阶段为准：四个架构的 `qemu_defconfig`，
  以及 aarch64 的 crosvm 和 virtcca 配置），
  并在 aarch64 `qemu_defconfig` 上跑一次 `make clippy UNITTEST=y`。
  lint 是否命中随 cfg/feature 变化时（如 `unreachable_pub`、只在可选 feature 下编译的代码、
  只在部分配置下命中的 `expect`），另做 feature 扫描：
  分别用打开和关闭相关可选 feature 的配置跑 `make clippy`。
  loongarch64 目前在主线上因无关原因失败，
  对比启用前后的错误集合，不新增即可。
- **工具链升级**：修改 `rust-toolchain.toml` 的 PR 必须按上条的配置集合附 `make clippy` 全量复核，
  并确认本文件引用的 lint 名和 `clippy.toml` 键在新版本中仍然存在。
- **测试代码**：`#[def_test]` / `#[mod_test]` 生成的测试函数和测试模块
  由 `util/macros` 统一豁免 `clippy::unwrap_used`、`clippy::expect_used`、`clippy::panic`。
  测试代码里不手写 `allow` / `expect`；
  裸 `#[cfg(unittest)] mod` 中的辅助函数命中时改用 `#[mod_test]`。
  host 侧 std `#[test]` 由 `clippy.toml` 的 `allow-*-in-tests` 覆盖。
  宏支持在启用 `clippy::panic` 之前落地。
- **范围**：`missing_docs` 和 `rustdoc::*` lint 归 module-docs 轨道
  （`docs/ai/review/module-docs/`），不在本文件管理。
- **与 AI 审查的分工**：`[workspace.lints]` 已启用的 lint，
  以及 clippy 默认组和 rustc 默认 lint 已强制的内容，不属于 code-guidelines skill 与 AI 审查的范围。
  待迁入表中的 lint 尚未启用；在它落地前，表中列出的原文仍是有效规则。

## 待迁入表

以下 lint 计划启用，对应原文已从 skill 中删除。
每落地一条，删除一行；最终此表为空。

- 来源行号指本表建立前（`origin/main` 0968f131）的文件。
- 存量来自 aarch64 QEMU defconfig 于 2026-09-24 在分支 4468f4b7 的 `--force-warn` 实测，
  主线落地前需重测；"未测" 表示尚无数据。
- 覆盖程度：full 表示 lint 单独即可替代原文；
  partial 表示 lint 只覆盖机械部分，
  判断标准写在"预计例外与判断标准"列（多为 `expect` 的 reason 须说明的内容），skill 中不再重复。

| lint | 组 | 1.95.0 默认 | 目标等级 | 覆盖的规则原文（逐字，可多条） | 来源 `文件:行` | 覆盖程度 | 需要的 clippy.toml 配置 | 预计例外与判断标准 | 存量 |
|---|---|---|---|---|---|---|---|---|---|
| `unsafe_op_in_unsafe_fn` | rustc | allow（workspace 现显式 allow） | deny | "inside `unsafe fn`, still isolate each unsafe operation in an explicit `unsafe {}` block rather than relying on the whole function body being implicitly unsafe;"<br>"`unsafe fn` bodies do not silently rely on implicit whole-body unsafety;"<br>"unsafe operations inside `unsafe fn` that are not wrapped in a minimal `unsafe {}` block;" | `unsafety.md:14-16`<br>`SKILL.md:114`<br>`unsafety.md:522-523` | full（`:522-523` 中 "minimal" 一半由 `clippy::multiple_unsafe_ops_per_block` 承担） | — | 无 | 0 |
| `clippy::missing_safety_doc` | style | warn（只查 pub 项） | deny | "every `unsafe fn` and `unsafe trait` must include a `# Safety` rustdoc section;"<br>"`unsafe fn` without `# Safety`;" | `unsafety.md:9`<br>`unsafety.md:521` | full（需配置；默认漏掉私有 `unsafe fn`） | `check-private-items = true` | 无 | 0（仅 pub）；私有项未测 |
| `clippy::linkedlist` | pedantic | allow | warn | "fall back to `alloc::collections::LinkedList` only when elements cannot own their linkage (for example the list must outlive or be decoupled from element ownership);" | `performance-and-resources.md:33-35` | partial（lint 标出每处 `LinkedList`） | `avoid-breaking-exported-api = false`（否则 pub 签名中的不报；见表后注） | 极少；reason 须说明元素无法自带链接（例如链表须比元素所有权活得久，或与之解耦），因而不能用 `linked_list_r4l` | 0 |
| `clippy::disallowed_methods` | style | warn（无配置时不触发） | warn | "`mem::zeroed()` or `assume_init()` for arbitrary `T`;"<br>"`mem::zeroed()` or `assume_init()` for types whose valid bit patterns are not proven by the type contract;" | `unsafety.md:208`<br>`unsafety.md:508-509` | partial（只接管 `mem::zeroed()` 一半；`assume_init()` 一半见下方默认 lint 表） | `disallowed-methods = [{ path = "core::mem::zeroed", reason = "..." }]` | 约 10 处逐点 `expect` 或改写；reason 须说明全零位模式对该具体类型有效，且由类型契约保证 | 未测（grep 约 10 处） |
| `clippy::todo` | restriction | allow | deny | "avoid `.unwrap()` and hidden panics where failure is a legitimate possibility;" | `error-handling-and-logging.md:9-10` | partial（只覆盖未完成代码里的 panic） | — | 无 | 5 |
| `clippy::unimplemented` | restriction | allow | deny | 同上 | `error-handling-and-logging.md:9-10` | partial | — | 无 | 0 |
| `clippy::used_underscore_binding` | pedantic | allow | warn | （skill 中无原文；`docs/ai/review/guidelines.md` 输出要求提到"下划线参数"，计划直接引入） | — | — | — | 宏展开误报时 `allow` | 32（测试 2） |
| `clippy::fn_params_excessive_bools` | pedantic | allow | warn | "avoid boolean arguments when the call site becomes unclear;"<br>"boolean flags that should be split into clearer entry points;" | `api-design.md:10`<br>`api-design.md:42` | partial（只数 `bool` 参数个数） | `max-fn-params-bools`（默认 3，阈值待定） | 少量；reason 须说明这些 `bool` 在调用点仍然清楚。阈值以下同样适用：单个 `bool` 参数让调用点不清楚时，拆成独立入口或改用 enum | 1 |
| `clippy::too_many_lines` | pedantic | allow | warn | "keep functions small and focused;" | `api-design.md:8` | partial（只量行数） | `too-many-lines-threshold`（默认 100，按实测分布定） | 启动/初始化长函数；reason 须说明函数仍只做一件事（如线性的初始化序列），否则按职责拆分 | 29（阈值 100，测试 1） |
| `clippy::excessive_nesting` | complexity | warn（阈值默认 0，即不触发） | warn | "minimize unnecessary nesting;" | `api-design.md:9` | partial（只量深度） | `excessive-nesting-threshold`（起议 4，按实测分布定） | 少量；reason 须说明嵌套没有掩盖主路径，且无法用提前返回或提取函数展平 | 未测 |
| `clippy::unused_result_ok` | restriction | allow | warn | （skill 中无原文） | — | — | — | 无 | 29（测试 12） |
| `clippy::multiple_unsafe_ops_per_block` | restriction | allow | deny | "keep each `unsafe` block as small as practical, so the audited region is obvious;"<br>"then use the smallest remaining `unsafe` block and `// SAFETY:` comment to discharge what cannot be encoded statically;"<br>"shrink unsafe blocks to the minimal audited operation;"<br>"unsafe operations inside `unsafe fn` that are not wrapped in a minimal `unsafe {}` block;" | `unsafety.md:12-13`<br>`unsafety.md:106-107`<br>`unsafety.md:163`<br>`unsafety.md:522-523` | partial（一块一个 unsafe 操作；块内夹带安全逻辑见 `unsafety.md` Invalid Patterns，仍靠审查） | — | 宏内 unsafe 序列 | 80（测试 2） |
| `clippy::let_underscore_must_use` | restriction | allow | warn | （skill 中无原文，计划直接引入；与 `unused_result_ok` 一起防止静默丢弃 `#[must_use]` 结果；`concurrency.md:166` 示例相应去掉了 `let _ =`） | — | — | — | 有意丢弃处 `expect` | 128（测试 2） |
| `clippy::allow_attributes_without_reason` | restriction | allow | deny | "suppress lints at the narrowest practical scope;"<br>"broad lint suppression;" | `modules-and-attributes.md:34`<br>`modules-and-attributes.md:58` | partial（强制 reason） | — | 无；审查时仍看例外的作用域是否最窄、reason 是否属实（见上方"例外写法"） | 350（测试 6） |
| `clippy::allow_attributes` | restriction | allow | deny | "suppress lints at the narrowest practical scope;"<br>"use `#[expect(dead_code)]` only when the dead code is simple, clearly intentional, and likely to become used."<br>"broad lint suppression;" | `modules-and-attributes.md:34-36`<br>`modules-and-attributes.md:58` | partial（强制外层 `#[allow]` 改 `#[expect]`；不检查内层 `#![allow]`） | — | cfg/宏/平台差异处带 reason 的 `allow`；`#[expect(dead_code, reason = "...")]` 只用于简单、明显有意且很可能会被用上的死代码，reason 说明它何时被用上 | 332（测试 5） |
| `clippy::panic` | restriction | allow | warn | "avoid `.unwrap()` and hidden panics where failure is a legitimate possibility;"<br>"unwrap-like failure paths hidden inside normal control flow;"<br>"error paths use `Result` and `?` rather than hidden panics;" | `error-handling-and-logging.md:9-10`<br>`error-handling-and-logging.md:36`<br>`SKILL.md:126` | partial（lint 标出每处） | `allow-panic-in-tests = true` | 合法失败（取决于输入、资源或环境）一律作为错误返回；`panic!` / `unwrap` / `expect` 只用于内部不变量被破坏，reason 说明是哪条不变量；测试经宏豁免 | 200（测试 69） |
| `clippy::expect_used` | restriction | allow | warn | 同 `clippy::panic` | 同 `clippy::panic` | partial | `allow-expect-in-tests = true` | 同 `clippy::panic` | 1187（测试 326） |
| `clippy::unwrap_used` | restriction | allow | warn | 同 `clippy::panic` | 同 `clippy::panic` | partial | `allow-unwrap-in-tests = true` | 同 `clippy::panic`；kext4 等 crate 根过渡豁免 | 3494（测试 1578） |
| `unreachable_pub` | rustc | allow | warn | "default to the narrowest visibility that works;"（两处）<br>"over-wide visibility;"<br>"helpers exposed more widely than required." | `modules-and-attributes.md:8`<br>`naming-and-structure.md:37`<br>`modules-and-attributes.md:49`<br>`naming-and-structure.md:48` | partial（只查从 crate 根不可达的 `pub`） | — | tee_kernel、knet、ksyscall 可能需过渡豁免；lint 不查 `pub(crate)`，审查时仍看它能否收窄为 `pub(super)` 或私有 | 1357（测试 4） |
| `clippy::upper_case_acronyms` | style | warn（只查全大写名） | warn | "follow Rust CamelCase and acronym capitalization for type names;"<br>"use `IoMemoryArea`, not `IOMemoryArea`" | `naming-and-structure.md:18`<br>`naming-and-structure.md:28` | full（与下方默认 lint 表中的 `non_camel_case_types` 合起来） | `upper-case-acronyms-aggressive = true`<br>`avoid-breaking-exported-api = false` | 镜像 Linux/硬件 ABI 名称的类型 | 未测 |

注：`avoid-breaking-exported-api = false` 是全局开关，
还会让 `box_collection`、`vec_box`、`redundant_allocation`、`enum_variant_names`、
`wrong_self_convention` 等默认 lint 开始检查 pub 项。
随第一个需要它的 lint 一起落地，并在同一提交中处理这些默认 lint 新增的告警。

## 辅助性 lint（不删原文）

这些 lint 只检查原文的某个侧面，不能替代审查，对应原文保留
（`api-design.md:8` 除外，它已随待迁入表中的 `clippy::too_many_lines` 删除）。

| lint | 组 | 1.95.0 默认 | 相关原文 | 来源 `文件:行` | 说明 |
|---|---|---|---|---|---|
| `static_mut_refs` | rustc | warn | "`static mut` for ordinary shared state;" | `unsafety.md:240` | 只拦对 `static mut` 取引用，不拦声明；已默认开启 |
| `improper_ctypes` / `improper_ctypes_definitions` | rustc | warn | "`repr(C)` structs and explicit ABI newtypes at the boundary;" | `unsafety.md:401` | 只查 FFI 签名中的类型是否 FFI 安全；已默认开启 |
| `clippy::wrong_self_convention` | style | warn | "use accurate names that reflect actual semantics and cost;" | `naming-and-structure.md:9` | 只查 `as_` / `to_` / `into_` 等前缀与 `self` 形式是否匹配；已默认开启 |
| `clippy::too_many_arguments` | complexity | warn | "keep functions small and focused;" | `api-design.md:8` | 只数参数个数（默认阈值 7）；已默认开启，现有约 31 处 `allow` |
| `unsafe_code` | rustc | allow | "when a crate or module is expected to remain safe-only, do not introduce `unsafe` casually;" | `unsafety.md:19-20` | 只能由各 crate 在根部声明 `#![forbid(unsafe_code)]`，不做 workspace 统一配置 |

## 已由默认 lint 或现有配置强制

这些 lint 已经生效（clippy 默认组、rustc 默认 lint，或已在 `[workspace.lints]` 中），
对应原文已从 skill 中删除。

| lint | 组 | 1.95.0 默认 | 当前生效方式 | 覆盖的规则原文（逐字） | 来源 `文件:行` | 覆盖程度 | 处理 |
|---|---|---|---|---|---|---|---|
| `clippy::undocumented_unsafe_blocks` | restriction | allow | `[workspace.lints]` deny | "every `unsafe` block must have a preceding `// SAFETY:` comment;"<br>"`unsafe impl` must have an explicit safety argument in nearby comments or docs;"<br>"missing `// SAFETY:` comments;"<br>"every `unsafe` block has a nearby `SAFETY:` comment that names the real invariants rather than using placeholders;" | `unsafety.md:8`<br>`unsafety.md:10`<br>`unsafety.md:520`<br>`SKILL.md:112-113` | full（`SKILL.md:112-113` 为 partial） | 整条删除；注释内容要求（写真实不变量、不用占位语）由 `SKILL.md` 审查清单 "every `unsafe` site has a concrete safety argument" 和 `unsafety.md` SAFETY Comment Standard 承担 |
| `non_camel_case_types` | rustc | warn | 默认 | "follow Rust CamelCase and acronym capitalization for type names;" | `naming-and-structure.md:18` | partial（CamelCase 一半；缩写大小写一半见待迁入表 `clippy::upper_case_acronyms`） | 与 `clippy::upper_case_acronyms` 一起删除 |
| `invalid_value` | rustc | warn | 默认 | "`mem::zeroed()` or `assume_init()` for arbitrary `T`;"<br>"`mem::zeroed()` or `assume_init()` for types whose valid bit patterns are not proven by the type contract;" | `unsafety.md:208`<br>`unsafety.md:508-509` | partial（只拦已知零值或未初始化无效的类型，如引用、`NonNull`、函数指针） | 与待迁入表 `clippy::disallowed_methods` 及下一行 `clippy::uninit_assumed_init` 合起来，两条原文整条删除 |
| `clippy::uninit_assumed_init` | correctness | deny | 默认 | 同上（`assume_init()` 一半） | `unsafety.md:208`<br>`unsafety.md:508-509` | partial（只拦 `MaybeUninit::uninit().assume_init()` 直接调用） | 整条删除；lint 拦不到的 `assume_init()` 只用于初始化已被证明的位置，由其 `// SAFETY:` 说明初始化如何被证明（`unsafety.md` Uninitialized Storage 保留 whole-array 一条） |
| `clippy::question_mark` | style | warn | 默认 | "use `?` for ordinary error propagation;"<br>"Check whether `?`, typed errors, and logging levels are used appropriately."<br>"error paths use `Result` and `?` rather than hidden panics;" | `error-handling-and-logging.md:8`<br>`SKILL.md:99`<br>`SKILL.md:126` | partial（只识别可直接改写为 `?` 的提前返回） | 第一条整条删除；`SKILL.md:99`、`:126` 只删 `?` 一半（`:126` 的 panic 一半见待迁入表 `clippy::panic`） |
