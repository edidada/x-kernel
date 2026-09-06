# 核心时间契约验证验收

## 验收目标

本轮以核心时间数值 API 的功能正确性、边界处理和列明非法输入的错误语义为目标。
辅助格式化采用有限的形式化性质与回归测试。验收不是整个库全部行为、标准库或
最终内核机器码的完整正确性证明，也不以证明入口数量作为覆盖率百分比。

## 接口与规格核对

逐项检查当前六个生产 Rust 文件的公有声明、实现、输入生成器、断言与 cover。
具体 API 到入口的对照见 [contracts.md](contracts.md)，核对结果如下：

| 对象 | 公有数值接口 | 已核对的规格与前提 |
| --- | --- | --- |
| TimeSpan | 21 个方法、2 个常量、Default | `u128` 总纳秒；构造、单位换算、规范化、精确/饱和转换、checked/saturating 加减；输入域包括完整 u64/u128 和非规范化 u32 纳秒 |
| Instant | 7 个方法、ORIGIN、3 个运算符实例 | 四个公有时钟域的原点、构造、转换、加减、时间差；完整合法时长域；跨域误用另有编译失败测试 |
| SystemTime | 9 个方法、3 个常量、Default、2 个运算符 | `i128` 总纳秒；完整 i64 秒、u32 分量及 i128 构造输入；负值向下规范化、跨 epoch、上下溢、差值方向 |
| SystemTimeError | duration 方法 | duration_since 所有负差的精确正幅度；可产生范围为全部正的合法时长 |
| TimestampLimits | 5 个方法、2 个常量、Default | 构造保持字段；全部合法范围/粒度；内部向下截断、精确端点钳位、幂等与单调；三类非法构造分区覆盖同时发生的错误组合 |
| 单位常量 | 5 个常量 | 独立数字字面量核对 |
| 值 trait | Copy/Clone/clone_from、比较、min/max/clamp、单值 Hash | 以数值语义和无碰撞字段写入记录核对；Eq-only 类型只检查其提供的能力 |

四个公有 marker 是不可实例化的空枚举，四个 Instant 类型别名不引入新实现。
代表性自定义 marker 的数值/trait 实例与无 marker trait 约束的泛型编译示例分开验证。

审查要点：

- 前提限制输入合法域或被测分支，不假设返回结果正确。构造器接受/拒绝的检查直接
  使用完整宽度原始输入；合法生成器由单独构造器及常量证明支撑。
- 时长总量及两个最大时长之和均小于 `2^95`；墙钟时间与时长的和、差也能容纳于
  i128，规格计算不会溢出。SystemTime 只检查加法上界/减法下界，因为时长非负。
- TimestampLimits 输入按下端点及以下、严格内部、上端点及以上分区；单点范围
  有显式覆盖。截断采用实现约定的整秒端点语义。
- 正常性质保留安全检查；数值路径没有循环。格式化局部循环界保留展开断言。
- 12 个全域 panic 契约同时要求非法域可达、正常返回不可达以及预期失败位置；
  9 个具体 should_panic 入口仅作边界见证。
- cover 仅用于排除所列类别的空前提，不是所有输入组合的测试覆盖率。
- 规格使用被测 getter 的地方，getter 已有独立输入/总量对照；不因此宣称验证器、
  规格审查或共同依赖不可能出错。本轮是同一工作流程内的自审，不是第三方认证。

## 明确保留的缺口

| 保留项 | 当前保障及取舍 |
| --- | --- |
| Debug 精确字节串及任意宽度、精度、美化选项 | 默认非空、长度界和容量错误传播已证明，文本及选项保留回归测试；不继续投入高成本全域文本求解 |
| 具体 Hasher 与任意长度 hash_slice | 单值字段写入类型、顺序和值已证明；不保证任意算法、无碰撞或切片循环的全部长度 |
| 任意 Rust marker 类型 | 覆盖四个公有域与代表性实例，不能把单态化结果推广为所有类型的全称证明 |
| panic 精确消息 | 验证所列错误发生及位置，未承诺完整消息文本 |
| 编译器/求解器及机器码 | 依赖 Kani 0.67.0 配套模型、CBMC 与求解器正确性；AArch64 构建通过不等于该机器码已证明 |
| 内核集成和调用者 | 不验证时钟硬件、调用者的时钟域选择、ABI 接口或并发集成；这些不属于本纯值库的数值契约 |

## 目录与依赖

```text
ktime_types/
├── Cargo.toml
├── src/                       # 生产实现，无 Kani 模块或配置
├── docs/                      # 正常模块文档
└── verification/
    ├── Cargo.toml / Cargo.lock # 独立 workspace，path 依赖真实生产 crate
    ├── lib.rs                 # 仅在此声明 Kani harness 模块
    ├── harnesses/             # 100 个证明入口
    ├── tests/                 # 格式化回归、时钟域编译测试
    ├── config.json            # 工具配置、源码范围、逐入口验收规则
    ├── contracts.md           # 规格与前提
    └── README.md
```

验证 crate 通过 `ktime_types = { path = ".." }` 引用生产库，不复制生产实现，
不使用 stub，也不新增业务公有 API。所有证明只依赖生产库公有接口。
业务 Cargo.toml 和 src/lib.rs 不再登记 cfg(kani) 或声明验证模块。

## 本地自动验收

在仓库根目录执行：

```bash
python3 scripts/verification/run.py util/ktime_types -j 4
# 局部诊断，不能替代完整验收：
python3 scripts/verification/run.py util/ktime_types --harness traits_clamp_span
```

安装方式见 [contracts.md](contracts.md#安装与复现)。当前固定 Linux x86_64、
Kani 0.67.0 / 其配套 Rust nightly-2025-11-21 / CBMC 6.8.0、
cvc5 1.3.4 官方二进制（f3b21c4），宿主测试使用仓库的 Rust 1.95.0。
通用入口只接受库路径、并行数与可选入口过滤；工具参数和局部循环界在库配置中审查。

仓库共用 `scripts/verification/run.py`、`reports.py` 和 `test_reports.py`。
增加其他库时，提供自己的 verification crate、证明/测试及 config.json，
不复制专用执行或审计脚本。本仓库没有为此接入 CI。

完整命令检查生产源码范围，运行 17 个通用审计器测试、4 个时钟域 doctest、
6 组格式化回归，随后以 cargo-kani 验证 100 个入口并审计 12 个全域 panic 契约。
局部诊断同样审计所选结果，但不会标为完整验收。漏跑、重复、未知入口、未满足的
普通 cover、未知状态以及非预期失败均拒绝通过。

执行器从仓库外的临时工作目录调用独立 Cargo manifest，避免继承内核 Kconfig
目标及 .cargo 配置；不修改用户 .config。先用 `cargo metadata --locked` 确认依赖图，
再离线运行 Kani。Cargo.lock、所有本地依赖的 Rust 源码/manifest、验证配置和
通用工具都有运行前后哈希核对；依赖或输入变化会使本次验收失败。

每次运行保存在 `target/kani/ktime_types/run.*/`：环境/参数 `environment.json`、
输入清单 `inputs.json`、测试日志、`verification.log`、逐入口报告，以及成功后生成的
`acceptance.json`。有过滤器的结果会明确写入 `full_acceptance: false`。
历史迁移前的结果不能作为新布局的验收证据。

## 独立布局验收（2026-09-06）

通过通用入口实际运行，退出码 0；100 个入口和 12 个全域 panic 契约通过，
`full_acceptance` 为 true。记录位于 `target/kani/ktime_types/run.ly9leosl/`。
17 个共享工具测试、4 个时钟域编译测试、6 组格式化回归均通过。
AArch64 defconfig 下的 `make build`、完整 `make clippy` 和 Rust 格式检查通过；
原 .config 及无关锁文件已恢复。Kani 与输入快照确认实际依赖的是生产 crate，
没有源码副本；src 中不再包含 Kani 入口或这批验证测试。

## 维护规则

config.json 固定经审查的生产指纹、入口及预期 panic 位置。这是变更提醒和验收清单，
不是自动判断规格正确的工具；同名入口的断言或 assume 弱化仍需人工审查。

1. 生产实现或接口改变时，先核对 API 清单、输入域、正常/错误规格及保留缺口。
2. 审查后更新 config.json 的相应源码 SHA-256、入口和预期行为，再完整验收。
3. 工具/依赖升级需重新审查报告格式、标准库行为、循环界及所有结果，不能只改版本。
4. 单独验证 crate 不属于内核 workspace；格式化它时使用仓库固定 rustfmt，
   内核代码变动仍须经过 defconfig/Makefile 构建和相关测试。

按这些边界维护时，核心数值契约验证可收尾；不承诺整个库全部行为已证明。
