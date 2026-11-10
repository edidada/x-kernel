# 候选区(Ch8 离线进化循环)

`candidates.json` 存从轨迹聚合出的**候选条目**,与正式 `data/{api-mapping,gaps,bitfield-defs}.json`
**物理分离**(Ch8 安全边界 2)。

## 字段

```json
{
  "id": "gap:clk_set_rate",      // kind:linux(稳定,跨 aggregate 不变)
  "kind": "gap",                 // lookup-miss / gap / bitfield-pitfall
  "linux": "clk_set_rate",
  "note": "...",                 // 聚合时取最长那条(信息最全)
  "support_count": 2,            // 被几个不同 port 轨迹支持
  "sources": ["dwmac-rk3588", "pcie-rk3588"],
  "status": "new",               // new / reviewed / promoted
  "first_ts": 1700000000,
  "last_ts": 1700001000
}
```

## 生命周期

```
trace add (在线)      → data/trace/*.jsonl(不可变)
staging aggregate     → 本文件(按 (kind,linux) 去重,support_count = 不同 port 数)
staging review        → 标 ★ 可发布(support≥2 且未 promoted)/ · 候选
staging promote --id  → 门槛检查 → 标 promoted + 打印入库指引
```

## 门槛(Ch8)

`support_count` 必须 **≥ 2**(PROMOTE_THRESHOLD,硬编码在 `ldp` 二进制,验证器不可自我修改)。
"至少两条非失败轨迹支持才写入正式文档。"

## promote 不自动改 data/

`promote` 只**标 promoted** + 打印"请把这条加进 data/<which>.json 的指引"。
为什么不自动?因为"加进哪个 section / 用什么 xkernel / 是新 mapping 还是新 gap"
都是**判断**,不是机械操作(Ch8 安全边界 2)。自动写 data/ 会让单次观察(可能错)
直接污染正式库。

promote 后人/agent 手动改 data/,再跑 `ldp check-data && check-bitfields && golden && gen-doc`
验证(Ch8 验证器门槛)。任一失败回滚;验证器脚本本身不会被生成物改写(安全边界 3)。
