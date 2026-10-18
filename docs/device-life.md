# 设备的一生（跨 crate 的单条叙事）

> 规范性说明：本文件是"一个设备从被枚举到被释放"的唯一叙事。
> 其它文档只描述自己那一站，不再重复整条链路。

## 一句话

**枚举 → 匹配 → probe → 发布（资源 / 类 / provider）→ quiesce → 释放**。
每一步只有一个所有者，释放只有一条路径（`Data` 的字段序）。

## 谁在哪一站

| 阶段 | 代码 | 所有者 | 失败怎么办 |
|---|---|---|---|
| 枚举 | `drivers/init`（DT/静态/PCI）+ `drivers/firmware`、`drivers/bus/pci` | 组合根 | 跳过该设备 + `log::warn` |
| 描述符 | `DeviceDescriptor`（`ident` + `resources` + `fw` + `dma`） | 组合根 | — |
| 入域 | `DeviceDomain::add_device` | 域 | `AddDeviceError`，跳过 |
| 匹配/绑定 | `domain/{registry,probe}.rs` | 域（一把锁、锁外 probe） | `Deferred(dep)` → park；`Fatal` → 换候选 |
| probe | 驱动 `DriverOps::probe(&mut Device)` | 驱动 | 返回 `Err` → 局部 guard 按声明逆序 drop |
| 类发布 | `dev.publish(&CLASS, runtime, name)` | `Publication`（驱动 `Data` 第一字段；stdout 归 console） | 发布前失败 → probe 失败 |
| provider 发布 | `dev.env().provide::<C>(key, lookup)` | host 的 class registry（boot-static） | 唯一允许的**最后一步**；之后不得再失败 |
| 激活 | 域把 `Data` 收进 binding，状态 `Active` | 域 | — |
| quiesce | `DriverOps::remove(&mut Device, &mut Data)` | 驱动 | 不可回滚；只记日志 |
| 释放 | `drop(Data)`：字段序 = 逆序获取序 | 驱动 `Data` + class/provider | 每个 guard 释放恰好一次 |
| 撤发布 | `Publication::drop` → 关闸 → removal 回调 → offline → 撤 devnode → 类 teardown | `Publication` | 在途 `InUse` 归零后执行 teardown |
| 回收 entry | `domain/lifecycle.rs`：`Removing → Removed` | 域 | 级联失败可续做 |

## 状态机（唯一权威：`DeviceDomain`）

```text
 add_device
      │
      ▼
   Pending ──select_batch──────────────┐
      │                                │
      ├─ 无候选 ──► Unclaimed ──(late driver/provider)──┐
      │                                │               │
      └─► Probing ──Ok(Data)──► Active │               │
              │                        │               │
              └─Deferred──► Deferred ──wake(dep)────────┘
              │
              └─Fatal──► 记入 attempted，换下一个候选

 Active ──remove_device──► Removing ──(级联成功)──► Removed
                              └─(子设备 Busy/BadState)──► 停在 Removing，可续做
```

## 三条不变量

1. **一个设备一个身份**：`DeviceCore`（id / driver name / path / online）是唯一
   权威；类发布、devnode、console 都读它，不复制值。
2. **一条释放路径**：资源 guard、`Publication`、provider 都挂在 `Data` 上；
   `remove()` 只做 quiesce，不做释放。字段序就是释放序。
3. **一个移除权威**：域状态机决定"移除开始/完成"；类侧 teardown 由
   `Publication` 的 drain 协议延后到最后一个在途 `InUse`，不存在第二条通知链。

## 在哪读什么

- 想加一个驱动：`docs/linux-porting-map.md` + 最近的 golden 驱动。
- 想看模型内部：`drivers/base/docs/design.md`（域/引擎）+ 本文件的链路表。
- 想看边界规则：`docs/ai/driver-subsystem-boundaries.md`。
