# ktime_types 的 Kani 验证

当前验收范围、规格自审、自动验收入口及维护规则见
[README.md](README.md)。下面列出具体性质和复现方式。

## 目标与输入前提

验证对象是 `TimestampLimits`、`TimeSpan`、`SystemTime`、`SystemTimeError`
及四个公有时钟域 `Instant<C>` 的显式数值 API、构造器和常量，
以及下文列明的值 trait、全域 panic 契约和默认格式化性质。
独立验证 crate 通过 path dependency 编译真实生产库。证明入口位于本目录的
`harnesses/mod.rs` 及其子模块；业务 src 中不包含证明代码，
不使用实现副本、stub 或额外公有 API。

符号输入保留完整的 `i64` 秒数和 `u32` 纳秒、粒度宽度。
前提与构造器契约一致：

- `1 <= granularity_ns <= 1_000_000_000`。
- `minimum_seconds <= maximum_seconds`，包括只有一个端点的范围。
- `nanoseconds < 1_000_000_000`。
- 单调性入口额外要求两个输入有序；内部取整入口额外要求秒数严格位于范围内部。

没有对结果添加 `assume`，没有缩小到某几个常见粒度。

## TimestampLimits 性质

| 证明入口 | 性质 |
| --- | --- |
| `truncate_preserves_range_and_normalization` | 输出位于两个整秒端点之间，纳秒分量规范化 |
| `truncate_respects_granularity` | 输出纳秒分量是粒度的整数倍 |
| `truncate_is_idempotent` | 重复截断不改变结果 |
| `truncate_is_monotonic` | 固定配置下保持输入顺序 |
| `truncate_rounds_interior_down_by_less_than_one_granule` | 内部输入保持秒数，向下取整且损失小于一个粒度 |
| `truncate_input_classes_are_reachable` | 11 个 cover 条件均存在合法输入 |

cover 包括范围下方、最小端点、内部、最大端点、范围上方、单点范围、
`i64::MIN`、`i64::MAX`、最小及最大粒度，以及非零输出纳秒分量。
它们检查这些类别没有被前提排除，不代表所有输入组合的覆盖度指标。

正常返回的入口保留 Kani 默认检查，包括断言、溢出、除零及可达 panic。
这也检查了合法输入不会触发 `truncate` 内部的 `expect`。
这些路径没有循环或递归，不设置展开上限，也不关闭展开检查。

## TimeSpan 性质与 API 对照

令 `N = seconds * 1_000_000_000 + nanoseconds`，
`MAX_N = u64::MAX * 1_000_000_000 + 999_999_999`。
规格使用 `u128` 总纳秒，两个合法时长的和也不会使规格自身溢出。
单位因子直接写在规格中，不复用被测转换算法。
没有 stub 标准库 `Duration`；其被调用实现同样参与验证。

| API / 契约 | 证明入口（省略 `span_` 前缀） |
| --- | --- |
| `ZERO`、`MAX`、`Default` | `constants_and_default` |
| `from_core`、`into_core`、`as_secs`、`subsec_nanos`、`is_zero` | `core_roundtrip_and_components` |
| `from_secs`、`from_millis`、`from_micros`、`from_nanos` | `unit_constructors_are_exact` |
| `new`：所有可表示的 `u64` 秒 + `u32` 纳秒组合保持总值并规范化 | `new_normalizes_representable_components` |
| `new(u64::MAX, 1_000_000_000)` 触发 panic | `new_panics_at_overflow_boundary` |
| `try_from_nanos`：当且仅当 `N <= MAX_N` 成功，成功值精确 | `try_from_nanos_has_exact_domain` |
| `as_nanos`、`as_micros`、`as_millis`、`subsec_micros` 的精确值/向下取整 | `getters_round_down` |
| `try_from_nanos(as_nanos(span)) == Some(span)` | `nanos_roundtrip` |
| `try_as_nanos_u64`、`as_nanos_u64_saturating` 的成功/钳位边界 | `u64_conversions_have_exact_boundaries` |
| `checked_add`：当且仅当和可表示时成功，并返回精确和 | `checked_add_has_exact_domain_and_value` |
| `checked_sub`：当且仅当左值不小于右值时成功，并返回精确差 | `checked_sub_has_exact_domain_and_value` |
| `saturating_add`、`saturating_sub` 等于总纳秒钳位结果 | `saturating_arithmetic_matches_clamped_totals` |
| `<` 和 `==` 与总纳秒顺序/相等一致 | `order_matches_total_nanoseconds` |
| checked 加法交换律及成功加减的逆运算 | `checked_arithmetic_is_reversible` |

正常返回入口覆盖所有合法时长，不限制秒数大小。
单位构造器的输入覆盖完整 `u64`；`try_from_nanos` 覆盖完整 `u128`，
包括不可表示输入，而非只从有效时长反向生成输入。
`new` 的正常返回入口仅假设总时长可表示，允许纳秒大于等于一秒。
14 个新增 cover 条件检查规范化进位、最大纳秒分量、可表示上限、
首个拒绝值、最大 `u128` 输入、`u64` 转换边界、加法进位、减法借位和下溢边界。

`#[kani::should_panic]` 仅检查存在 panic，不能证明全体非法输入必然 panic，
也不能限定具体 panic 消息。因此该入口使用固定的边界输入，
没有把它计作全域 panic 正确性证明。其他 13 个 TimeSpan 入口要求正常验证通过。
这一组数值入口不检查格式化和其他值 trait；后续扩展见下文对应章节。

## Instant 性质与时钟域

`harnesses/instant.rs` 对 `Monotonic`、`Boottime`、`ProcessCpu`、
`ThreadCpu` 四个公有 marker 分别实例化性质。
时刻的规格值为从原点起的 `u128` 总纳秒，范围与 `TimeSpan` 相同。
输入覆盖全部合法时长，没有缩小到硬件常用的 `u64` 纳秒范围。

| API / 契约 | 证明入口 |
| --- | --- |
| `ORIGIN`、`from_span_since_origin`、`span_since_origin`、`as_nanos_u64_saturating` | `instant_constructors_all_domains` |
| `checked_add` 的成功范围和精确和、成功 `+`、加后减可逆 | `instant_addition_{monotonic,boottime,process_cpu,thread_cpu}` |
| `checked_sub` 的成功范围和精确差、成功 `- TimeSpan`、减后加可逆 | `instant_subtraction_{monotonic,boottime,process_cpu,thread_cpu}` |
| `checked_duration_since`、`saturating_duration_since`、成功 `- Instant`，以及 `<`/`==` | `instant_differences_{monotonic,boottime,process_cpu,thread_cpu}` |
| `+` 上溢、`- TimeSpan` 越过原点、`- Instant` 逆序的具体 panic 边界 | `instant_add_panics_at_boundary`、`instant_sub_panics_before_origin`、`instant_difference_panics_for_reversed_order` |

各时钟域的加、减、时间差入口分别求解，避免将多个独立符号问题合并导致超时。
每个入口保留完整输入范围、精确数值关系和默认安全检查。
panic 入口仅使用 Monotonic 的具体边界输入，不声称证明所有 marker 的非法输入。

跨时钟域运算不属于运行时数值性质。在 `tests/domains.rs` 中提供一个
同域运算的成功示例、两个 Monotonic/Boottime 跨域比较及相减的编译失败示例。
运行通用验收入口，用仓库指定 Rust 工具链编译
原 crate 并执行这些 doctest；它们不依赖 Kani 或内核 Kconfig。
这些示例检查所列类型组合，不代表枚举了所有自定义 marker 或所有类型组合。

## SystemTime 性质与错误结果

令 `MIN_T = i64::MIN * 1_000_000_000`，
`MAX_T = i64::MAX * 1_000_000_000 + 999_999_999`。
规格使用 `i128` 总纳秒，所有合法时间/时长的和差均在规格可表示范围内。

| API / 契约 | 证明入口 |
| --- | --- |
| `MIN`、`MAX`、`UNIX_EPOCH`、`Default`、`from_unix_seconds` | `system_constants_and_seconds` |
| `from_unix_parts` 接受且仅接受规范化纳秒，分量 getter 及 `unix_nanos` 精确 | `system_parts_have_exact_domain` |
| `from_unix_nanos` 接受且仅接受 `[MIN_T, MAX_T]`，负数按秒向下规范化 | `system_nanos_have_exact_domain_and_floor_normalization` |
| `unix_nanos` 精确、转换往返、`<`/`==` 与总纳秒一致 | `system_nanos_roundtrip_and_order` |
| `checked_add` 的成功范围、精确值和规范化，成功 `+`、加后减可逆 | `system_checked_add_matches_signed_total` |
| `checked_sub` 的成功范围、精确值和规范化，成功 `-`、减后加可逆 | `system_checked_sub_matches_signed_total` |
| `duration_since` 的 Ok/Err 方向及精确幅度，包括 `SystemTimeError::duration` | `system_duration_since_reports_exact_magnitude_and_sign` |
| 两个运算符的具体越界 panic | `system_add_panics_at_boundary`、`system_sub_panics_at_boundary` |

分量构造入口保留完整 `i64` 和 `u32` 输入，整数构造入口保留完整 `i128` 输入，
既验证可表示值，也验证非法值返回 `None`。
21 个 cover 条件覆盖纳秒分量边界、`-1 ns`、时间范围两端及首个拒绝值、
`i128` 极值、进位借位、跨 epoch、相等时间和两个方向的最大时间差。
`duration_since` 不添加顺序前提，成功和错误结果都参与证明；内部两个
`expect` 在所有合法时间对上均不得 panic。

## TimestampLimits 与单位常量的补充覆盖

- `timestamp_limits_construction_preserves_components` 检查所有合法构造输入
  和三个 getter 的一致性。
- `timestamp_limits_constants_and_units` 检查 NANOSECOND、SECOND、Default
  及五个导出的单位常量。
- `truncate_clamps_to_exact_endpoints` 检查范围外和端点上的输入返回精确端点。
- 三个 `timestamp_limits_panics_for_*` 入口提供零粒度、超过一秒的粒度、
  逆序范围的具体 panic 见证。

## 值 trait、Hash 与自定义时钟实例

`traits.rs` 的 9 个值比较/复制入口验证 Clone、clone_from、Copy、cmp、partial_cmp、
六种比较运算符及 min/max 与数值语义的一致性。
`TimeSpan`、`SystemTime`、四个公有 Instant 域及一个 `CustomClock` 实例
覆盖有序值的这些性质；`TimestampLimits`、`SystemTimeError` 则覆盖它们提供的
相等/不等和复制行为。

另有 7 个 `traits_clamp_*` 入口覆盖 TimeSpan、SystemTime、四个公有 Instant 域和
一个自定义有序 marker。值、下界、上界独立符号化，仅假设下界不大于上界。
用可容纳所有时间值的 `i128` 总纳秒及独立条件分支计算预期结果，
检查区间外精确钳位、区间内保持不变；规格不调用整数或时间类型的 clamp。
每个入口的 6 个 cover 检查下方、上方、严格内部、两端点及单点区间均可达。
这些检查没有循环或展开界，也不等于对标准库所有默认 trait 方法的证明。

三个 Hash 入口覆盖库中实现 Hash 的 TimeSpan、SystemTime、Instant：
验证器记录恰好两个字段写入事件，区分 `write_u64`、`write_u32`、`write_i64`
以及其参数和顺序。任何其他写入或调用 finish 都会失败。
TimeSpan 与 core::time::Duration 的单值 Hash 输入序列相同；
Instant 与其底层时长相同，不混入 marker；SystemTime 写入有符号秒和纳秒。
这证明了单值 Hash 的观测行为和相等值的一致性，没有依赖哈希摘要无碰撞，
也没有证明具体 Hasher 算法或任意长度 `hash_slice`。

`custom_clock_*` 的另外 4 个入口复用构造、加、减和时间差规格，
实例化到一个非内核定义的时钟 marker。时钟域编译测试另有一个无 marker trait 的
`ApplicationClock` 示例和 `advance<C>` 泛型函数，检查数字 API 不额外要求
marker traits。单态化结果与泛型编译检查分开报告，不声称量化了所有 Rust 类型。

## 全域 panic 契约与报告审计

`panic_contracts.rs` 的 12 个入口覆盖 TimeSpan::new 规范化溢出、
TimestampLimits::new 的三类非法输入、Instant 三个运算符、SystemTime 两个运算符，
以及 TimeSpan、SystemTime、Instant 的逆序 clamp 上下界。
Instant 运算符入口用符号分支覆盖四个公有时钟域和一个没有 trait 派生的自定义 marker；
clamp 入口使用另一个满足 Copy/Ord 约束的 marker。逆序 clamp 的值仍取完整合法域。
clamp 报告只接受已登记单态类型的 `Ord::clamp` 中 `min <= max` 断言失败，
其他断言位置、类型或描述均拒绝通过。
TimestampLimits 将非法输入分为：有序范围 + 零粒度、有序范围 + 超大粒度、
逆序范围 + 任意粒度，因此也覆盖多种错误同时出现的输入。

这些入口仍使用 `should_panic`，但脚本额外要求：

1. `panic-domain` 的状态为 SATISFIED，排除空前提。
2. `panic-return` 的状态为 UNREACHABLE 或 UNSATISFIABLE，证明不存在正常返回的非法输入。
3. 存在预期 panic；其余检查不得失败或未决。只允许列明函数的 assertion 失败。
4. 全量运行必须执行完整 12 个入口，并读取本次新生成的逐入口报告。

没有 stub 实现，没有使用 `--prove-safety-only`，没有关闭默认检查。
这些被测路径没有循环或递归；结合已验证的非 panic 安全检查和返回不可达性，
可以对所列非法域给出必然 panic 的结论。
这不是适用于任意非终止程序的通用算法。

通用 `scripts/verification/reports.py` 结合 config.json 的逐入口规则审计 Kani 0.67.0 报告，
缺失或未知格式会失败，运行脚本也拒绝未经审查的 Kani 版本升级。
单独运行原始 Kani 命令的 should_panic 成功标记仍不足以得到全域结论。
17 个通用工具测试检查可达正常返回、空输入域、未知状态、缺失结果、非 panic 失败，
以及 clamp 的错误类型、断言位置和描述等情况。

## 格式化：已证明、已测试与尚未证明

`formatting.rs` 的 4 个入口分别对 SystemTime、TimestampLimits、
Monotonic Instant、SystemTimeError 的默认 Debug 格式，证明完整合法数值域上
向立即拒绝写入的 sink 传播 `fmt::Error`。
这些入口使用 CaDiCaL 和展开上限 16；展开检查保持开启且通过。
该结论仅覆盖所述 sink 和默认格式。

另有 5 个 `formatting_*_default_length` 入口，直接执行成功的默认渲染，
检查完整合法数值域上输出非空，且字节数分别不超过：

| 类型 | 默认输出字节数上界 |
| --- | ---: |
| TimeSpan | 32 |
| SystemTime | 68 |
| TimestampLimits | 124 |
| Monotonic Instant | 41 |
| SystemTimeError | 62 |

这些是保守界，不承诺最紧上界。计数器接收真实格式化字符串的长度，
未替换生产或标准库实现。全程安全和展开检查同时验证。

5 个 `formatting_*_budget_error` 入口对上述类型和任意 `usize` 字节容量，
检查真实格式化实现遵守 sink 错误：写入片段超过剩余容量时返回 `fmt::Error`，
格式化必须立即停止写入，并且返回错误当且仅当 sink 发生拒绝。
覆盖标记要求成功和拒绝两条路径都可达。
该 sink 按整个 `write_str` 片段接收或拒绝，不模拟部分写入、I/O、副作用或任意
用户定义的 `fmt::Write` 实现。SystemTimeError 由 MIN 与任意更晚时间的错误差构造，
涵盖全部合法正误差幅度；Instant 此处实例化为 Monotonic。

新增入口使用 CaDiCaL、展开上限 32。原来的主要瓶颈是字符计数内部不可行嵌套
循环的展开。脚本为 Kani 0.67.0 配套 core 中 `str::count::do_count_chars`
的四个循环分别指定 1、1、2、3 的展开界，且保留每个展开断言。
这会减少符号执行工作量，不缩小合法输入域；如果界不足，验证必须失败。
循环符号与工具版本绑定，升级时需要重新检查，不能复用旧结论。
调整界时应修改入口属性或 config.json 中的局部界；Kani 不允许将命令行 `--unwind` /
`--default-unwind` 与这里使用的 CBMC `--unwindset` 同时传入。

`tests/formatting.rs` 的 6 组宿主回归测试覆盖五类格式化类型，
检查默认/美化输出、TimeSpan 的若干精度/宽度选项、极值与单位边界，
以及代表值每个输出容量下的写入错误传播。它们是确定性测试，不是符号全域证明。

完整 Debug 文本及任意格式选项的全域证明未完成。长度与错误传播性质不能
代替文本正确性；确切文本继续由回归测试保障，验收范围见
[README.md](README.md)。

## 安装与复现

已验证的组合：Linux x86_64、Kani 0.67.0、其配套
`nightly-2025-11-21` / CBMC 6.8.0、CVC5 1.3.4。

安装 Kani（会安装其独立工具链）：

```bash
cargo install --locked kani-verifier --version 0.67.0
cargo kani setup
```

从 [CVC5 1.3.4 官方发布页](https://github.com/cvc5/cvc5/releases/tag/cvc5-1.3.4)
安装对应平台的命令行程序，将 `cvc5` 加入 `PATH`。
本次使用 `cvc5-Linux-x86_64-static.zip`。

在仓库根目录运行：

```bash
python3 scripts/verification/run.py util/ktime_types -j 4
python3 scripts/verification/run.py util/ktime_types --harness truncate_is_monotonic
```

完整目录、工具约束、报告审计和维护流程见 [README.md](README.md)。
数值/trait/panic 入口使用 CVC5，格式化入口使用 CaDiCaL。
CVC5 通过 `--solve-bv-as-int=sum` 转换原有位向量公式，保留固定宽度语义。
格式化循环界保存在 config.json，保留展开断言，不缩小合法数值输入域。

## 结论边界

结果仅覆盖上述函数、前提和配置，依赖 Rust 到验证模型的翻译以及求解器正确性。
它不证明整个 `ktime_types`、内核集成、时间源或最终机器码的正确性。
非法配置的预期 panic 不属于这些入口的无 panic 保证。
显式数值 API、上述库内值 trait 及列明非法输入域的 panic 已有对应检查。
仍未证明完整 Debug 文本、具体 Hasher 算法、任意长度 hash_slice、
标准库所有默认 trait 方法、所有 Rust marker 类型以及最终机器码。
9 个旧的具体 should_panic 入口保留为边界见证；12 个 panic_contract
入口只有在额外报告审计通过时才产生全域结论。
超时、求解器错误或失败不能视为证明通过；代码或工具版本变化后应重新运行。

工具使用依据：[Kani 安装](https://model-checking.github.io/kani/install-guide.html)、
[Kani 单 crate 和 cfg 用法](https://model-checking.github.io/kani/usage.html)、
[CVC5 选项](https://cvc5.github.io/docs/latest/options.html)。
