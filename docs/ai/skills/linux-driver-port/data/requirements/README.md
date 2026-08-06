# 需求 issue(Requirements)

这个目录存 `ldp requirements gen` 生成的**需求 issue**(每个缺口一个 md),
加 `index.json` 做状态索引。它是"这个缺口要不要实现、怎么实现"的可决策文档。

## 生成

```bash
ldp requirements gen --from-scan <Linux源码路径>   # 扫一份驱动,发现的缺口各生成一个 issue
ldp requirements gen --from-trace <port>           # 从一个 port 的轨迹生成
ldp requirements list                               # 看(📝 proposed 待确认的会高亮)
ldp requirements show <id>                          # 看某个 issue 详情
ldp requirements confirm <id> --status confirmed   # 人工确认
```

## 分类(infra_type)

每个 issue 按 `data/gaps.json` 里对应 subsystem 的 `infra_type` 分类,决定「建议实现路径」:

| infra_type | 代表 | 实现路径 |
|-----------|------|---------|
| `multi-provider` | clk, gpio, phy, reset, regulator, iommu, pinctrl | Provider trait + phandle 键注册表 + consumer API + 总线层 DT 解析(参考 clk) |
| `single-provider` | dma 模式 | Provider trait + 单全局 `Once<&dyn>` + consumer API(参考 device-res) |
| `framework` | regmap, workqueue, timer, completion | 具体框架设计 |
| `subsystem-class` | i2c, spi, watchdog, rtc | 新 DeviceKind + publish_* + class trait |
| `driver-missing` | (有 DeviceKind,缺驱动) | 直接按 driver-skeleton 写 |

新发现的缺口(gaps.json 未列)默认归 `framework`,标 `📝 proposed` 待人工归类。

## Provider 判据 + 实现质量清单(防 provider 泛滥 + 半成品 OS-agnostic)

provider 类 issue(multi-provider/single-provider)额外渲染两节:

- **分类依据(provider 判据,Stage 1)**:五判据(物理基础设施 / 多生产者 / 消费者泛化 /
  契约跨内核稳定 / 收益>成本)。五条全 pass → 值得抽。未记录留 `?`,`ldp check-data` WARN。
- **实现质量清单(cross-kernel 坑,Stage 2)**:provider 已确认该抽后,避免「名义 OS-agnostic
  实际被绑死」。三坑(从 clk 实证归纳):注册键用了固件描述特定物 / trait 只定当前 Tier /
  注册表全局静态不可注入。其中「Cargo.toml 不得依赖 k*/of」是机械可查红线:

```bash
ldp check-provider-crate drivers/clk/Cargo.toml   # 机械查 provider crate 的 OS-agnostic 红线
```

判据评估 + impl 坑定义在 [`provider-boundary.json`](../provider-boundary.json),渐进填充。

## 状态机

```
proposed → confirmed → in-progress → landed
                                    ↘ rejected(终态)
```

`landed`/`rejected` 是终态,`confirm` 拒绝重复变更(避免反复)。落地后记得把
`data/gaps.json` 对应 subsystem 的 `status` 改 `landed`,跑 `ldp check-data && gen-doc`。

## 与其它环节的关系

- **vs staging(Ch8 知识侧)**:staging 回答"这个经验值不值得沉淀进知识库";
  requirements 回答"这个缺口要不要实现"。一个缺口可同时走两条路。
- **vs gap-report**:gap-report 是某次 port 的完整缺口快照;requirements 是
  跨 port 的、可追踪的需求 backlog(带状态机)。

## 幂等

`gen` 幂等:同 gap_id+symbol 多次生成 issue id 不变,保留 status/confirmed_ts。
只更新内容字段。分类不对就改 gaps.json 的 `infra_type` 重新 gen。
