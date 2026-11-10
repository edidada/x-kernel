# 演示手册:PL031 删除重写 —— linux-driver-port harness 在 QEMU virt 上的全自动转写

**一句话**:PL031 RTC 驱动及其全部接线已被删除(本仓库 HEAD 即删除态),用 harness 从
钉死的 Linux 参考(linux-7.1.8 `drivers/rtc/rtc-pl031.c`,**已 vendor 进本仓库**)
重新转写整条链路,在 QEMU aarch64 virt 上恢复 hctosys 墙钟。全程可重放、无真机、
无网络(参考源码全部在仓,只有构建工具链本身需要本机已装好)。

> **本仓库即验证包,且只有单一初始提交**——PL031 旧驱动的实现不在仓库里,也**不在
> 任何 git 历史里**(初始提交即删除态,没有可恢复的旧代码;验证必须走 harness 从
> vendored 参考重写)。第 1 幕是起点确认,第 2、3 幕就是要重放的 harness 流程。

## 为什么选 PL031

- QEMU virt **默认设备**(0x9010000,DT `arm,pl031`),零额外 `-device` 参数;
- 参考驱动 470 行、寄存器模型简单(MMIO + 1 个使能位),但**位段/序列/AMBA 匹配**
  一应俱全,能完整展示 scan→gate→sequence→check-layout→生成→带机全流程;
- 删除态与重写后行为可逐行 diff(同一条 hctosys 日志),演示闭环干净;
- 重写建议不依赖外部寄存器 crate:布局写在驱动内、由 `ldp check-layout`
  对 vendored 参考机械校验;
- 参考驱动 `#include <linux/amba/bus.h>` 的总线语义也已 vendor
  (`data/references/linux/drivers/amba/bus.c` + `include/linux/amba/bus.h`,
  amba_id 表/periphid 匹配的真相源);`ldp lookup amba_id` 直接命中仓内映射。
  **本环境没有任何仓库外的源码树,一切参考以 `data/references/` 为准。**

## 前置(一次性)

环境要求:Rust 工具链(`rust-toolchain.toml` 钉定版本,`rustup` 会自动装)、
`qemu-system-aarch64`(Debian 系 `apt install qemu-system-arm`)、`python3`。

```bash
cd <本仓库根>
cp platforms/kplat-aarch64/qemu_defconfig .config && make defconfig
truncate -s 64M disk.img          # virtio-blk 占位盘(演示只看内核启动日志)
```

## 第 1 幕:删除态确认(HEAD,重写起点)

```bash
make build && make run LOG=trace | grep -E "RTC|Boot at"
#  driver-core: no RTC device supplied the boot wall clock   ← 缺口可见
#  (Boot at 1970-01-01:墙钟从未被播种)
```

> 启动末尾出现 `panicked at fs/boot` / `Failed to mount any block device`
> 是**预期行为**：演示用空盘（无根文件系统），而 RTC 缺口证据在 initcall 阶段、
> panic 之前就已打出。跑通根文件系统不是本演示的关注点。

## 第 2 幕:harness 流水线(重放命令)

```bash
L="cargo run --manifest-path xtask/Cargo.toml -p ldp --"

# 参考就位(fail-closed;拒绝 web search)
$L ref check

# P1/P2:逐条 API 映射(命中带 source 依据;未命中=候选缺口)
$L lookup devm_ioremap
$L lookup request_irq
$L lookup amba_id                     # AMBA 匹配语义:映射 + vendored 总线源码
$L lookup devm_pm_set_wake_irq        # 未命中 → 缺口/裁剪候选

# P2:机械扫缺口 + 报告草稿落盘 + blocker 决策门(退出 0 才允许写代码)
$L scan docs/ai/skills/linux-driver-port/data/references/linux/drivers/rtc/rtc-pl031.c \
    --report docs/ai/skills/linux-driver-port/examples/pl031-qemu-gap-report.md --gate

# 铁律 6:序列即数据(从参考抽有序步骤;人工归并 → data/sequences/pl031-qemu.toml)
$L sequence extract docs/ai/skills/linux-driver-port/data/references/linux/drivers/rtc/rtc-pl031.c \
    --function pl031_probe --path init

# P3 生成前:读参考并钉锚(pre-commit 钩子强制:含驱动变更的提交必须带新锚点)
bash scripts/ref-anchor.sh \
  docs/ai/skills/linux-driver-port/data/references/linux/drivers/rtc/rtc-pl031.c \
  pl031_probe "锚定 probe 序列"

# 生成后:三道验证门
make ldp-check                                             # ref/data/check-layout 全仓
$L check-layout --port pl031-qemu                          # 位段三方校验(位段声明写进 data/bitfield-defs.json)
$L sequence check --port pl031-qemu --strict               # 覆盖/可达/顺序
#  check-layout 期望:  ✓ <位> [l:l] = reference(行 N) / ✓ 驱动实现匹配 claim
#  sequence 期望:      步骤 N:anchored a / waived w / unported 0
```

## 第 3 幕:集成 + 带机验收

```bash
# 注册三连:drv-rtc feature + driver-init feature + api/kfeat 特性行 + drivers/Kconfig 块
make build && make clippy && make run LOG=trace | grep -E "pl031|wall clock"
```

**验收预期**(重写成功时启动日志应出现,与 Linux 参考行为一致):

```
driver-core: registered driver `rtc-pl031` for bus `platform`
rtc-pl031: published at 0x9010000 (4096 bytes)
rtc: wall clock seeded at unix <host-utc> s by `rtc-pl031`
```

第 1 幕的 `no RTC device supplied the boot wall clock` 消失、墙钟被 `rtc-pl031`
播种 → 删除重写闭环。

## 验证者应产出的产物

| 产物 | 位置 |
|------|------|
| 驱动(绑定+寄存器层,推荐不依赖外部寄存器 crate) | `drivers/drv/rtc/src/pl031.rs` |
| 序列数据(铁律 6) | `data/sequences/pl031-qemu.toml` |
| 位段声明(check-layout 真相源) | `data/bitfield-defs.json` 新增 def(scope=port, port=pl031-qemu) |
| 缺口报告(裁剪逐条带理由) | `examples/pl031-qemu-gap-report.md`(由 scan --report 草稿补全) |
| 参考锚(读参考再改驱动的证据) | `reference-anchors.md` 新增条目(pre-commit 钩子强制) |
| P7 轨迹(撞墙点沉淀) | `ldp trace add pl031-qemu --kind <lookup-miss\|gap\|bitfield-pitfall> ...` |

> 注:上一轮演示中发现并已修复的两处 harness 缺陷已包含在本包
> (`ldp sequence extract` 的通用 `writel/readl` 词汇、`ldp check-layout`
> 接受私有 `const` 锚),验证时直接可用,不需要重新发现。

## 已知边界(验证时可能被问)

- **为什么可以没有 alarm/IRQ?** 类契约 `subsys_rtc::RtcDevice` 今日只读(设计如此,
  出现消费者再扩);PL031 的 alarm 寄存器布局可进 bitfield-defs,后续支持是"接上"
  不是"重找"。裁剪必须写进缺口报告,不允许静默删除。
- **偏移校验为什么是 ℹ 不是 ✓?** A 档 C 提取器对标量(裸 `0xNN`)只做偏移值比对;
  位段(`1 << N`/`GENMASK`)才是硬断言。写进 def note 即可。
- **非 PL031 的 PrimeCell 节点弹回是什么?** 泛型 `arm,primecell` 节点被 periphid
  校验拒绝(`Fatal` → 域把节点交给下一个候选),PL011 等不受影响——这正是参考
  `amba_id` 表语义(见 vendored `drivers/amba/bus.c`)。
