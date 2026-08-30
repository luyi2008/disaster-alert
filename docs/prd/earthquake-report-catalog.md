# 全国地震速报目录

运营要能在管理事件列表里看到接入过的地震速报，即使当时没有任何订阅命中。这不是新表，也不是新流水线：复用现有 `IncidentRecord`，只改「没人订阅就删档」的策略。

公开订阅、Bark 推送、匹配规则不在本文范围。查询口仍是未写入用户文档的 `GET /api/admin/events`。

## 背景

当前每条接入的速报都会先合成 incident（含 `latest_by_source` 和 timeline）。提交时若没有匹配任务、从未通知过订阅者、账本里也没有记录，存储会把这次 transition **整份丢掉**。管理列表扫的就是 `incidents`，所以没人订阅的地震从运营视角等于没发生过。

匹配、Bark、23 个 keyspace、event 与 incident 的分工都可以不动。缺的是保留策略。

## 目标

- 地震速报在接入后留下 incident 档案，无论有没有订阅命中。
- 过期速报仍**不通知**，但**建档**。过期窗口只决定要不要进入匹配，不再顺便决定能不能进目录。
- `has_matched_subscribers` 继续区分「接入了」和「有人收到」。
- 未命中不必长期保留 `events` 快照；管理列表读 incident 上的最新报文即可。

## 非目标

- 不向用户暴露全国目录 API。
- 不改订阅匹配（速报仍按震级、预警仍按预计烈度）。
- 不把预警、气象、海啸、台风的未命中记录做成目录（EEW 连报会把运营列表撑满）。
- 不把演练报写入目录（`IGNORE_TRAINING=true` 时训练不是真实测定）。
- 不扩展 ingest。Wolfx `cenc_eqlist` 仍只取 No1，FAN Studio CENC 仍是当前快照。目录等于「本进程运行期间见过的速报」，不是台网全量历史。

## 策略

| 情况 | 通知 | 留 incident |
| --- | --- | --- |
| 速报，有订阅命中 | 是 | 是，`has_matched_subscribers = true` |
| 速报，匹配无人命中 | 否 | 是，`has_matched_subscribers = false` |
| 速报，超出过期窗口 | 否 | 是 |
| 速报，演练且忽略演练 | 否 | 否 |
| 速报，取消且忽略取消 | 否 | 否 |
| 预警 / 其它灾种未命中或过期 | 否 | 否 |

体积按 CENC 每天几十条、保留 180 天估算，现有 `incidents` 全表排序足够，不加新索引。

后续若要「真·全国目录」，需要扩数据源（例如 eqlist No1–NoN 或定期拉历史）。那是另一份 PRD。

## 实现边界

改动集中在两处删除点，测试和 [storage.md](../storage.md) 跟着改。

- [`src/storage/fjall.rs`](../../src/storage/fjall.rs) `commit_incident`：无 match job 且从未通知时，地震速报改为写入；演练/取消跳过仍丢掉新建档案。已在库里的速报档案，后续被策略跳过时不得整份删掉。
- 同文件 `commit_match_batches`：空批次且从未通知时，速报 incident 留下，对应 `events` 快照仍删。
- [`src/events/coordinator.rs`](../../src/events/coordinator.rs)：过期窗口与 `should_match` 不变。
- 不新增公开 API、不改 Docker/部署。

## 验收

- 无订阅时接入一条 CENC 速报：`GET /api/admin/events` 能看到该 incident，`has_matched_subscribers` 为 false，Bark 未发送。
- 同一条速报晚于预警过期窗口到达：仍出现在列表中，且没有 match job。
- 演练速报在 `IGNORE_TRAINING=true` 时不建档。
- 过期地震预警仍不建档。
- 未命中速报的 `events` 快照在匹配结束后删除；incident、alias、correlation 保留。
- 已通知过的 incident 行为与现在一致。
