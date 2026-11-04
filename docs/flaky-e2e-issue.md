# 一次 main 构建里 3 个 e2e 同时红：都是 deadline 轮询超时，前后 run 全绿

## 摘要

合并 `b836675fa`（PR #5）触发的 main CI（run `41766881698`）有 3 个 shard 红，共 4 个 e2e 断言失败。**同一个 commit 前后两次 run 全绿**，本仓库没有 CI 重试机制，所以不是代码缺陷 —— 是 daemon/PTY 类 e2e 的时序抖动。

## 证据

| run | commit | 结果 |
|---|---|---|
| `41764381041` | 之前 | ✅ success |
| **`41766881698`** | `b836675fa` | ❌ 3 shard 红 |
| `41769960065` | 之后（PR #7） | ✅ success |
| `41775137080` | 之后（main HEAD） | ✅ success |

同一个 `b836675fa` 的改动只有 `.github/workflows/ci.yml`（TS 参考双源下载），不碰任何 crate 代码，也没有任何理由影响下面这些测试。

## 四个失败断言（原文）

### 1. `crates/pa-daemon/tests/agent_family_e2e.rs:536` — `parent_child_agent_message_round_trip_end_to_end`

```
the parent never rendered the child's reply to the kid send: {"messages":[]}
```

`card_deadline = Instant::now() + Duration::from_secs(20)`，20 秒轮询 `client.messages("gm-parent", ...)` 等子代理回复卡片。超时后 `{"messages":[]}` 说明父会话消息列表**完全为空** —— 不是"卡片内容不对"，是消息根本没到。

### 2. `crates/pa-daemon/tests/agent_family_e2e.rs:310` — `family_edges_never_cross_families_end_to_end`

```
record parent-broadcast.json never appeared or never parsed in /tmp/.tmpIrOtjw/receipts:
  existing: ;
  kernel error record: <none>;
  daemon log tail: pa-daemon client connection error: Broken pipe (os error 32)
```

`deadline = 30s`。关键是 `Broken pipe (os error 32)` —— **daemon 侧连接被对端关闭**。receipts 目录为空说明 kernel 根本没写出任何记录就断了。

### 3. `crates/pa-cli/tests/interactive_daemon_e2e.rs:5814` — `tui_accepted_then_killed_turn_renders_closed_error`

```
killed turn had 5 frames without the TS error row; old_info=false;
```

`HeadlessStep` 的 `timeout_ms: 30_000`。5 帧之后 TUI 停在欢迎屏（日志里能看到 `prime agent v0.0.0 / cwd /tmp/.tmp3XRrw9` 横幅），**turn 根本没被 accepted**，所以"杀掉后显示错误行"的前提没成立。`old_info=false` 说明不是"显示了旧信息"那种退化。

### 4. `crates/pa-daemon/tests/registration_refusal_heal_e2e.rs:507` — `an_owned_stop_kills_a_worker_that_missed_the_shutdown`

```
the owned stop never persisted its tombstone (the stop must be durable before the shutdown routes): null
```

`deadline = 10s`。`descriptor` 是 `null` —— 不是字段不对，是**整个 descriptor 没写出来**。

## 共同特征

四个失败全是同一个形状：

```
loop { if <条件成立> { break; } assert!(Instant::now() < deadline, "..."); }
```

即"轮询到一个 timeout，然后断言"。这类测试在满负载 runner 上必然有假红风险：

1. **deadline 是绝对墙钟**，不随 CPU 竞争／磁盘 IO 延长。GitHub 共享 runner 上一个 shard 并行编译 + 其它 shard 抢 IO 时，10~30 秒的窗口不宽裕。
2. **失败信息里的空值指向更早的一步没完成**（`messages: []`、`existing: `、`descriptor: null`、只有欢迎屏），不是断言条件太严 —— 说明 daemon 起不来／连接断了／kernel 没跑起来。这些前置步骤自己**没有独立超时断言**，只能由最终断言兜底，所以报错指向的是症状而非根因。
3. **CI 没有任何重试**：`.github/workflows/ci.yml` 里搜 `rerun` / `retry` / `--test-threads` 均为 0 命中。

## 另一个环境因素：runner 磁盘

同一时期 PR #7 的 run（`41769960065`）也有 3 个 shard 红，但那是另一个原因：

```
error: failed to build archive at .../libpa_daemon-*.rlib: No space left on device (os error 28)
##[warning]You have run out of disk space. Free space left: 6 MB
```

重跑即绿。所以这台 runner 池本身在被压榨：**磁盘和时序两种抖动都在发生**。这两件事相关但不同：磁盘耗尽会直接失败编译，时序抖动则是测试跑到了但等不到结果。

## 建议方向（未实施）

1. **先分离"慢"和"错"**：把断言里的空值信息（`messages: []` / `descriptor: null`）改成在**前置步骤**就断言 —— 比如 daemon 起来后先断言能 `client.messages()` 拿到会话列表，不等 20 秒才发现全是空。这样红的那一刻就能指向真正的失败点。
2. **deadline 改成可配置**：`PA_E2E_DEADLINE_MULTIPLIER`（默认 1.0），CI 满负载时设 2.0 或 3.0。比无条件调大常量的好处是本地快速反馈不变。
3. **加 CI 重试**：`gh run rerun --failed` 是手动的。可以考虑在 shard 级加一次自动重试，但要**先记录 flake 频率**再决定 —— 无脑重试会掩盖真实缺陷。
4. **flaky 跟踪**：目前没有任何 flake 统计。至少可以周期性地统计各 run 的失败测试名，出现频率 >N% 的单独排查。

## 为什么值得记录

- 这个仓库的合并门禁是"绿才合"。flaky 会让门禁不可信 —— 开发者学会"红了就重跑"，真红也会被当抖动忽略。
- 本次 3 个 e2e 都涉及 **daemon 生命周期 + 子代理家族通信**，是这个产品的核心可靠性面（AGENTS.md 的 Reliability model 一节）。抖动在这里出现，值得知道是环境还是真有竞态。
- 四个断言都是"等不到就报错"，没有一个是"值算错了" —— 这个分布本身说明环境压力是主因，但不能排除真实的窄窗口竞态。

## 相关

- 触发 run：https://github.com/jinnnyang/prime-agent/actions/runs/41766881698
- 磁盘耗尽那次：https://github.com/jinnnyang/prime-agent/actions/runs/41769960065
- 相邻的 CI 可靠性问题：#4（工具链未锁）、#6（parity 测试静默跳过）
