---
title: "Page-backed memory"
---

分支 `feat/harness-digest-coverage` 上的记忆模型：**markdown 页是真相源，digest 里的索引是渲染产物**。本文档只描述已实现的代码，未实现的部分在 §7 单列。

## 1. 存储

```
<agent dir>/memory/
├── pages/<type>/<slug>.md     # 知识页（真相源）
├── pages/_entities/*.md       # 结构节点：真实页，但不进索引
├── archive/<slug>.md          # 归档的页（删除 = 移到这里 + 墓碑）
├── _archive.md                # 墓碑行：slug — title (archived date)
└── journal/dream.log.jsonl    # dream pass 的 journal
```

四种 type，索引按此顺序输出：`world`（与本机、仓库、版本无关的事实）、`model`（稳定偏好、规则、常规答案）、`observation`（≥2 次经验归纳，带证据）、`experience`（单次会话记录）。

页的 frontmatter：`slug` / `type` / `title` / `hook`（"什么时候该读我"）/ `status` / `updated`，可选 `evidence`（列表）、`weight`（宿主统计）、`relations`、`history`。

`status: archive` / `invalidated` 不进索引；`status: disputed` 单独成段，与已定事实分开列。

## 2. digest 里的索引

有页时，digest 是 **entries 段 + memory 段**：

- entries 段：prompt / skill / subagent / factory，逻辑与本分支之前完全一致。
- memory 段：页索引（`slug — title` + `· read when:`），正文要用文件工具读（`bash` / `python_repl`）。

索引预算 1700 字符，超过后按固定阶梯降级：缩短路由行 → 去掉路由行 → 每类只列最新 4 页 + `+N more`。实测 12 页中文语料（全量带 hook）= 索引 623 tok + 说明 223 tok；digest 合计 1480 tok（含 entries 段 633）。

## 3. 写路径

| 入口 | 行为 |
|---|---|
| `rlm.harness.create_memory(title, content, *, type=, hook=, id=)` | 写 `<type>/<slug>.md`，`type` 默认 `experience`，`status: draft` |
| `rlm.harness.update_memory(id, ...)` | 重写正文，**保留调用方没给的 frontmatter 字段**（`evidence` 块、`weight` 等） |
| `rlm.harness.delete_memory(id)` | 归档 + 墓碑（不 unlink）。语义上删除属于 dream |
| `rlm.harness.search(query)` | 搜索包含页；短查询（ASCII < 3 字符）**报错**而不是静默返回空 |
| `rlm.harness.overview()` | memory 段显示为 `page-backed` 计数与 hook |

refine 的 `memory` 编辑在**全局作用域**下同样写页（`planner::apply_refinement_proposal` 带 `pages_dir`），local 作用域不写，避免会话内改动跨会话存储。

## 4. 提示词

- `core.md`：记忆是页，digest 是索引，正文要读；写页时保留 frontmatter，结论放正文第一句。
- `REFINEMENT_SYSTEM_PROMPT`：refine 对记忆做**优化**（提炼 / 关联 / 抽象）；压缩、合并、晋升、删除属于定时的 dream pass；证据要写可解析的引用。

## 5. dream pass

`pa-core::memory::dream` 提供可复现的那部分：

| 函数 | 作用 |
|---|---|
| `dream_due(agent_dir, now)` | journal + 页库决定是否该跑；返回种子（draft 优先、其次无证据、再次最久未更新） |
| `clamp_next_at(now, requested)` | 下一次运行夹紧在 6 小时 ~ 14 天 |
| `heartbeat_interval(next_at, now)` | 把选择的时间转成 `every Nh` |
| `acquire_lock` / `release_lock` | 单 pass 锁，30 分钟视为陈旧 |
| `append_journal` / `read_journal` | JSONL：visited / compressed / merged / promoted / removed / next_at / why |
| `dream_prompt(decision)` | 子代理的任务书：允许的动作按优先级排列、抽取式压缩、删除的四条前置、同 pass 证据不能支持删除、预算 24 节点 / 3 轮 |

## 6. 怎么打开 dream（当前默认关闭）

本分支不自动排程。要在某个会话里启用：

```python
await rlm_heartbeat.create(
    "<paste memory::dream::dream_prompt() 的输出>",
    interval="every 12h",
    label="memory dream",
    delivery_mode="follow_up",
)
```

pass 结束后由它自己用 `heartbeat_interval` 的结果重建下一次的 heartbeat。**未做的部分**：由宿主在会话启动时自动判定 `dream_due` 并创建 heartbeat（会改动真实行为，需要你先确认）。

## 7. 未实现 / 已知缺口

| 项 | 状态 |
|---|---|
| dream 的自动触发（daemon 或专用记忆会话） | 未做，需先定"会话如何保持 active" |
| 压缩 / 合并 / 晋升的实际执行 | 只有提示词与归档原语；`compress`/`merge`/`promote` 作为结构化动作还没落代码 |
| `get_harness_state` 的页投影 | 未做：`search` / `overview` 已读页，`get_harness_state` 仍读条目 |
| `weight` 的宿主统计 | 字段已解析；读取计数未接 |
| 归因：`evidence` 的存在性校验 | 未做（只解析条数） |
