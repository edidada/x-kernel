# 参考树(References)

本目录存放 `ldp check-layout` 机械导出布局真值用的**本地参考源**。

## 为什么必须是本地目录

`bitfield-defs.json` 里每个布局的 `ref.ref_symbol`(如 `MSG_ID_MASK`)锚到参考树里的一个
符号。`ldp check-layout` 从这里**机械提取**该符号的位段(GENMASK/BIT),diff 移植方声明
`claim`,以及(可选)驱动代码实际写法。

关键约束:**参考必须是本地目录,不能是 web search 链接。** 原因见 SKILL 铁律 2 与
位段检查的起源——SCMI `message_id`/`token` 那次 bug 正是凭记忆抄错,而 harness 必须能
重新从参考导出真值才能抓到。web 链接无法满足"机器可重新导出 + 钉死版本"。

缺参考 → `ldp ref check` 与 `ldp check-layout` 都 **fail-closed**(退出 1)。这把
"太容易忘了拉参考"变成结构性强制:必须在场才能过。

## 目录布局

```
data/references/
  README.md            ← 本文件
  linux/               ← Linux 参考树(vendored 子集 或 submodule)
    VENDORED.txt       ← vendored 时:首行记 pin(tag/commit),证明来源可复现
    ...                <子树,如 drivers/firmware/arm_scmi/common.h>
  test-fixtures/       ← 合成 fixture(非真实 Linux,仅供提取器单元测试)
```

## 如何 vendor Linux 子集(推荐:sparse-checkout 最小子集)

不需要整棵 Linux 树,只要被 `bitfield-defs.json` 引用的文件。例:

```bash
cd docs/ai/skills/linux-driver-port/data/references
git clone --filter=blob:none --no-checkout --depth 1 \
    --branch v6.12 https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git linux
cd linux
git sparse-checkout init --cone
git sparse-checkout set \
    drivers/firmware/arm_scmi \
    drivers/mmc/host
git checkout
echo "v6.12 (kernel.org, sparse: drivers/firmware/arm_scmi drivers/mmc/host)" > VENDORED.txt
```

- `VENDORED.txt` 首行是 pin,`ldp ref check` 读它确认来源可复现。
- submodule 形式也接受(目录下有 `.git`,`ref` 据此识别 pin 由 `.gitmodules` 钉)。
- 不要手抄单个 header 进来又删 gitlink —— 那会让 pin 不可追溯,失去复现性。

## 当前状态

`linux/` 尚未 vendored(首次接入)。预期行为:

```
$ ldp ref check
ERROR [ref] 必需参考树 `linux` 缺失:.../references/linux。vendor 方法见 .../README.md。
✗ ref check FAIL
$ ldp check-layout
══ scmi-msg-header (...) ══
  ✗ 必需参考树 `linux` 缺失:check-layout 无法导出真值 → fail-closed。
```

这正是设计要的"长牙":没 vendor 真参考,就过不了。vendor 后,SCMI header 的位段
会从 `common.h` 的 `MSG_ID_MASK GENMASK(7,0)` 等机械导出,diff claim 与驱动
`pack_header` 实际写法。
