# Port 轨迹(Ch8 在线循环)

这个目录存 **不可变、append-only** 的 port 轨迹,每个 port 任务一个 `.jsonl`。

## 它是什么

`ldp trace add <port> --kind <k> --linux <sym> [--note ...] [--evidence ...]`
在 port 进行中追加一条观察事件。三种 kind:

- `lookup-miss`:`ldp lookup` 未命中(Linux API 在 `api-mapping.json` 没有)。
- `gap`:撞到缺失子系统的用法。
- `bitfield-pitfall`:位段/协议常量易错点。

每行一个 JSON:`{ts, kind, linux, note?, evidence?}`。`ts` 是 epoch 秒(单调,用于排序)。

## 为什么不可变

Ch8:轨迹是**进化证据**。它记录"这次 port 真撞到了什么"。事后改写历史会让离线聚合
(`ldp staging aggregate`)的支持计数失真。**没有 delete/edit 命令**——写错的当场观察就当
历史保留;真错的靠 `staging promote` 的门槛(≥2 支持)挡住,不靠改写历史。

## 下一步:离线进化

轨迹本身**不会**自动并入正式 `data/`(Ch8 候选/正式分离)。流程:

```bash
ldp staging aggregate   # 聚合所有 trace/*.jsonl → staging/candidates.json
ldp staging review      # 看哪些候选 ≥2 支持可发布
ldp staging promote --id <id>   # 标 promoted + 打印入库指引(不自动改 data/)
```

详见 `../schema.md` 与 `../../docs/design.md` 的「Ch8 双循环」一节。
