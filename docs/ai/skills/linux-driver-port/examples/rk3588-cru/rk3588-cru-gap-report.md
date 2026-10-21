<!-- 本文件由 `ldp scan --report` 自动生成(草稿)。
人工补全:compatible / DeviceKind / 落点 / 逐条处置 / 可编译性结论 / 集成状态。
模板见 templates/gap-report.md。 -->

# 移植缺口报告(草稿)— ../docs/ai/skills/linux-driver-port/data/references/linux/drivers/clk/rockchip/clk-rk3588.c

| 字段 | 值 |
|------|----|
| Linux 源 | `drivers/clk/rockchip/clk-rk3588.c`(linux-7.1.8,已 vendor;配套 clk-pll/clk-cpu/clk-half-divider/gate-link/clk.c/clk.h)|
| 目标 compatible | `rockchip,rk3588-cru`(板 DT `clock-controller@fd7c0000`,phandle 0x02)|
| 目标 bus / 发布 | PlatformBus;基础设施 provider(无设备类发布,走 ClkProvider 门)|
| x-kernel 落点 | `drivers/drv/clk/src/rk3588_cru/`(mod.rs 绑定+probe,data.rs 648 分支生成表,ops.rs PLL/armclk/frac/halfdiv 自定义 ops)|
| 移植结论 | **受限可用**:时钟树完整(9 PLL+3 armclk+648 分支+31 frac+3 halfdiv+22 gate-link),软复位域与 SCMI 相位钟除外(见下)|

## 依赖的 Linux 子系统清单(scan 自动命中)

- [x] `clk framework` [partial→provider 落地]: core/kclk + 本次框架增量(HIWORD_MASK/READ_ONLY/NO_REPARENT/same-rate 短路/父 notifier/跨 provider 父名);提供者 `rockchip,rk3588-cru` 已注册

## 逐条缺口(scan 自动命中;处置待填)

| 子系统 | 严重度模型 | 现状 | 处置建议 | 需求 issue |
|--------|-----------|------|---------|-----------|
| **clk framework** | … | 本次移植后:provider 已注册(drv/clk/rk3588_cru),消费端 `dev.res().clk(name)` 可解析全树 | 已落地;框架增量以 5 个 kclk 单测守护 | req-clk-framework(landed)|
| **reset controller**(CRU 软复位域)| blocker(USB/PCI probe 需要) | gaps.json `reset` [missing] | 本次未移植(rst-rk3588.c;无 reset framework)| 随 USB/PCI 切片立项 |
| **SCMI clk provider**(MMC 相位钟 706/707 等)| conditional | 无 SCMI 固件通道 | MMC 相位分支未移植(父钟 scmi_cclk/sclk_sd 以 rate-0 占位)| 不阻塞 UART/USB2/PCIe |
| **restart notifier**(GLB_SRST_FST)| optional | 无重启钩子 | 未移植 | 低优先级 |

## 阻塞项汇总(决策门,生成代码之前必须过)

- ⚠ `clk-framework`: 严重度模型含 conditional/blocker,尚无已决策需求 issue(无)。
  - 下一步:`ldp requirements gen --from-scan <路径>` 生成 issue,人工 `confirm`
    后 `ldp scan <路径> --gate` 通过,才能进入代码生成。

## 可编译性结论

- `make build`(rk3588_defconfig,aarch64):**通过**(2026-07,v2 树)。
- `make unittest`:3634 passed,0 failed(含本次 5 个 kclk 框架增量测试)。
- `ldp check-layout --port rk3588-cru`:rk3588-pll-con0/1/2/6 位段与参考一致。

## 集成状态

- [x] 驱动 crate:`drv-clk/rk3588-cru` feature + `drivers/drv/clk/src/rk3588_cru/`
- [x] 组合根:`drivers/init` level3 `drv_clk::rk3588_cru::register()`;Kconfig `KFEAT_DRIVER_CLK_RK3588_CRU`
- [x] defconfig:`platforms/kplat-aarch64/rk3588_defconfig` 已启用
- [ ] 真机 P6(bring-up 手册见 BRINGUP.md;暂无真机,编译验证为主)
> 在草稿上继续补全(处置、编号参考序列对照表、集成记录),完成后即正式缺口报告。
