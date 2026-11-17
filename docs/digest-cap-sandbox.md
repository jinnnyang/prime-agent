# digest cap 沙盒：怎么做的、怎么用

本文档属于分支 `feat/harness-digest-coverage`。它描述该分支新增的设置项
`harness.digestEntriesPerKind`（digest 每种 kind 最多列几条）以及配套的本机沙盒
`C:\Users\jinnn\Desktop\pa-digest-sandbox`。功能合入 main 时本文档随分支一起进。

沙盒只解决一个问题：**多列记忆条目要花多少 token、覆盖到多少条**。它不测"记忆内容有没有用"。

---

## 1. 为什么需要沙盒

直接改 `~/.prime/agent/settings.json` 会影响所有真实会话；改坏之后也分不清是"代码有问题"还是
"真实记忆数据在作怪"。沙盒给的是：**干净的 agent dir + 可控的记忆规模 + 可重复的一次投递**。

---

## 2. 怎么做的（隔离原理）

沙盒的本质是**换掉 agent dir**，让 prime-agent 把 settings、harness state、sessions、daemon 全写到
沙盒里。做法参照本机已有的 `pa-onboarding-sandbox`。

| 环境变量 | 沙盒值 | 为什么 |
|---|---|---|
| `PRIME_AGENT_CODING_AGENT_DIR` | `<sandbox>\agent` | settings + `harness\harness_state.json` + auth 全在这里 |
| `PRIME_AGENT_SESSION_DIR` | `<sandbox>\agent\sessions` | 会话不写进真实会话库 |
| `PRIME_AGENT_DAEMON_SOCKET` | `\\.\pipe\prime-agent-digest-sandbox` | 不抢真实 daemon，避免串会话 |
| `PRIME_AGENT_KERNEL_PYTHON` | 真实 kernel venv 的 `python.exe` | 否则沙盒会重新安装 Python 环境（慢、需联网）；只用于交互入口 |
| `PRIME_AGENT_BASH_SHELL` | `C:\Symbols\Git\bin\bash.exe` | 让沙盒里的 `bash()` 可用；只用于交互入口 |
| `DO_NOT_TRACK` | `1` | 不产生遥测副作用 |

另外两点：

- **跑的是分支构建**：`Documents\prime-agent\target\debug\prime-agent.exe`，不是安装版 `~/.local/bin`。
  沙盒里的行为 = 分支代码；真实会话 = 安装版。两者互不影响。
- **测量入口带 `--no-session`**：每次都是一次冷边界启动，因此**每次都重新投递 digest**。这是稳定测量的前提；
  代价是不留会话文件。交互入口相反：会话会持久化，供事后回看。

`agent\auth.json` 是从真实 agent dir 复制的凭据副本，仅用于本机调用 provider。

---

## 3. 三个入口分别是什么

### 3.1 交互试用（TUI）

| 脚本 | 作用 |
|---|---|
| `try-sandbox.bat` | 用沙盒环境启动分支构建的 TUI，参数透传 |
| `try-sandbox-debug.bat` | 同上，但退出后停住并显示退出码（启动失败时用这个） |
| `set-cap.bat 12` | 只改沙盒的 cap，不启动会话 |
| `show-digest.bat` | 把沙盒最近一次会话**真正收到的** digest 打印出来 |

**关键事实：digest 在 TUI 里看不见。** 它是 `display: false` 的 custom 行，宿主不在界面渲染它。
所以"试用"必须配 `show-digest.bat` 才能看到模型拿到了什么。

### 3.2 测量（headless）

| 脚本 | 作用 | 参数 |
|---|---|---|
| `run-digest.bat` | 跑一次投递，打印 token 与覆盖率 | `--cap 12`、`--raw`、`--prompt "..."` |
| `sweep.bat` | 矩阵扫描（条数 × cap），累积写 `results\sweep.jsonl` | `--counts 12,60 --caps 3,12,64 --cjk` |
| `analyze.bat` | 读 `sweep.jsonl` 出对比表，写 `results\sweep.md` | 无 |

### 3.3 生命周期与内部实现

| 脚本 | 作用 |
|---|---|
| `reset-sandbox.bat [N]` | 停沙盒进程 → 清 `harness`/`sessions`/`results` → 重新种子 N 条（默认 12） |
| `seed.bat [N]` | 只重新种子，不清结果；`60 --cjk` 用全中文测最坏 token |
| `stop-sandbox-processes.ps1` | 按**精确可执行文件路径**杀进程；安装版与真实会话不动 |
| `seed-agent.py` | 写 `harness\harness_state.json`：先拷真实 state 前 N 条，不够用模板合成（`source: seed`） |
| `run-digest.py` | 设隔离环境 → `-p --mode json --no-session` → 抓 `harness_digest.details.digest` → 算 token/覆盖率 |
| `sweep.py` / `analyze.py` | 矩阵执行 / 汇总成表 |
| `latest-digest.py` | `show-digest.bat` 的实现：读沙盒会话 JSONL，抽 digest 原文 |

---

## 4. 什么时机用什么

| 你想知道的 | 用这个 | 例子 |
|---|---|---|
| 交互体感：多列记忆后 agent 行为有变化吗 | `set-cap.bat` + `try-sandbox.bat` + `show-digest.bat` | 见 §5 |
| 当前默认值长什么样 | `run-digest.bat --cap 3` | 869 tok / 3 条 |
| 调大后长什么样（含原文） | `run-digest.bat --cap 12 --raw` | 原文落 `results\digest-cap12.md` |
| 记忆变多后成本怎么涨 | `sweep.bat --counts 12,60,120 --caps 3,12,64` | 出矩阵 |
| 中文记忆的成本上限 | `sweep.bat --counts 30 --caps 3,64 --cjk` | 最坏情况 |
| 沙盒状态脏了 / 换规模 | `reset-sandbox.bat 30` | 清干净重来 |

**改完代码后的典型节奏**：`cargo build -p pa-cli` → `reset-sandbox.bat` → `sweep.bat` → `analyze.bat`。

---

## 5. 交互试用的标准流程

```bat
cd C:\Users\jinnn\Desktop\pa-digest-sandbox

set-cap.bat 3
try-sandbox.bat                 :: 聊两句，然后 /exit
show-digest.bat                 :: 看这次真正喂了什么：3 条 + "+9 more"

set-cap.bat 12
try-sandbox.bat                 :: 再聊两句，/exit
show-digest.bat                 :: 对比：12 条全列
```

想看**行为**差别而不只是文本差别，就在会话里直接问：

> 逐条列出你这次被提供的记忆标题

cap=3 时它会答得残缺（或说"还有 N 条未显示"），cap=12 时能列全。这比看 token 数字更接近"这个功能有没有用"。

---

## 6. 怎么读结果

token 口径：`est_tokens = CJK 字数 + 其余字符 / 4`。**不是真实 tokenizer**，数量级可信、个位数不可信。
需要真值就看 provider 返回的 usage。

已实测（2026-11-17，`deepseek-v4.1-flash`，每次一次投递）：

| 记忆条数 | cap | digest tok | 固定部分 | 展示 | 隐藏 |
|---|---|---|---|---|---|
| 12 | 3 | 869 | 634 | 3/12 | 9 |
| 12 | 6 | 1075 | 634 | 6/12 | 6 |
| 12 | 12 | 1611 | 628 | 12/12 | 0 |
| 30 | 3 | 869 | 635 | 3/30 | 27 |
| 30 | 12 | 1617 | 635 | 12/30 | 18 |
| 30 | 64 | 2798 | 628 | 30/30 | 0 |

三个稳定结论：

1. **固定说明文字约 630 tok，不随 cap 变化**（call contract 约 249 tok，refine 触发说明约 146 tok）。
2. **每条目约 66–72 tok**（title/path/version + 正文截断 140 字）。
3. **成本线性**：30 条全列是默认的 3.2 倍。cap 拉满只适合小规模记忆（约 ≤15 条）；更大规模应靠检索按需读。

---

## 7. 限制与坑

- 每次 `run-digest` / `sweep` 的每一格都是**一次真实模型调用**（回一个词）。单次成本可忽略，但矩阵会累加。
- 测量入口的 `--no-session` 让沙盒不留会话文件，`agent\sessions` 常为空，**属预期**；交互入口才会留。
- 沙盒 `agent\auth.json` 是**凭据副本**，只放本机沙盒目录，不要提交或外发。
- 只测 digest **投递**，不测检索质量、不测记忆内容好坏。
- 沙盒假设分支构建在 `Documents\prime-agent\target\debug`；换路径要同时改 `run-digest.py` 的 `EXE`
  与 `stop-sandbox-processes.ps1` 的 `-ExePath`。
- cap 会被 `1..=64` 夹取：`0` 当作 1，超大值当作 64。

---

## 8. 把这个模式复用到别的功能

1. 建目录：`<name>-sandbox\{agent,work,results}`。
2. `agent\settings.json` 只放被测键；`agent\auth.json` 从真实 agent dir 复制。
3. 一个 runner：设 §2 的环境变量 + 指向**分支构建**的 exe，用 `-p --mode json --no-session` 跑，
   从 JSON 事件流里抓你关心的事件。
4. 一个 seeder：写 `harness\harness_state.json`（或该功能的其他输入），规模参数化。
5. 一个 sweep + analyze：矩阵跑、存 jsonl、出 md 表。
6. 一个交互入口 + 回看脚本：TUI 启动器，以及把隐藏行（`display: false`）读出来的工具。
7. 一个 stop 脚本（精确路径匹配）+ 一个 reset（清状态并重新种子）。

---

## 9. 相关

- 分支：`feat/harness-digest-coverage`，提交 `3c1accf8b`（`harness.digestEntriesPerKind` + 指纹版本升到 2）
- 设置示例：`docs/examples/settings.example.md`
- 变更片段：`.changes/harness-digest-coverage.md`
- 设计背景与投递基线：本地 `Technology-Lectotype\pa-memory-基线测量-20261117.md`

---

## 10. 页索引模式（本分支新增的第二件事）

分支还换了 digest 的**数据源**：agent 目录下有 `memory/pages/**/*.md` 时，digest 渲染的是这些页的**索引**，不再是 `harness_state.json` 的条目列表。没有页的 agent 完全走旧渲染。

```
<agent dir>/memory/pages/<type>/*.md      ← 真相源（markdown + frontmatter）
        ↓ load_pages()                    解析 slug / type / title / hook / status / updated
        ↓ render_index(pages, 1700 chars) 按类型分节；每条出 slug — title + `read when`
   digest 的记忆段 = 这个索引（不是页正文）
```

正文怎么读：模型用已有的 `bash` / `python_repl` 读文件，**不新增工具**。索引只负责"让人知道它存在、什么时候该读"。

### 10.1 截断阶梯（按实测成本排序）

12 页中文语料实测（CJK 记 1 token、ASCII 记 1/4）：

| 阶梯 | 渲染字符 | token | 说明 |
|---|---|---|---|
| ① 全量带 hook | 1729 | 644 | 默认档 |
| ② hook 截 40 字 | 1718 | 636 | 这批 hook 本来就短，几乎无收益；长 hook 语料才有用 |
| ③ **去掉 hook** | 1109 | 383 | 大幅下降 |
| ④ 每类只留最新 4 页（`+N more`） | 846 | 279 | 唯一会隐藏页的一档，按 `updated` 取最新 |

预算 `MEMORY_INDEX_BUDGET_CHARS = 1700`：12 页全量带 hook 正好装下（1690 chars），超过约 15 页才开始降级。

### 10.2 与旧机制的成本对照（修复后，2026-11-17 复测）

| digest 组成 | token | 说明 |
|---|---|---|
| entries 段（legacy 固定说明 + 各 kind 计数） | 633 | 沙盒里 prompt/skill/subagent/factory 都为 0，几乎全是固定开销 |
| memory 段：索引说明文字 | 223 | `memory_index_preamble`，可压缩 |
| memory 段：页索引正文（12 页，全量带 hook） | 623 | 1690 字符，未触发阶梯降级 |
| **合计** | **1480** | 沙盒实测，`digest-capNone.md` |

对照：

| 机制 | digest 总 token | 覆盖条目 |
|---|---|---|
| 旧机制（entries 段 + 3 条记忆，cap=3，12 条记忆库） | 869（早先实测） | 3 / 12 |
| 页索引（entries 段 + 12 页索引） | **1480** | **12 / 12** |

**更正**：此前写的「847 tok，与旧机制持平」是错的。那次测的是修复前的二进制，它整段替换 digest，丢掉了 legacy 固定说明和其他 kind，所以数字偏低。修复后，页索引模式每轮比旧机制多约 610 tok（1480 − 869），换来覆盖率从 25% 提到 100%。

可以压缩的部分（待决定）：

- 索引说明文字 223 tok，可以改写得更短，预计省 100 tok 左右。
- 把预算从 1700 字符降到 1100，阶梯会去掉 hook（623 → 约 383 tok），但路由行也一起没了，不推荐。

### 10.3 沙盒里怎么验证这一档

```bat
cd C:\Users\jinnn\Desktop\pa-digest-sandbox
try-sandbox.bat
```

页目录是 `agent\memory\pages\` 的一份快照（12 个可索引页 + 5 个 `_entities`，后者不参与索引）。进 TUI 后问一句 **「你的记忆是怎么组织的？」**：

- 旧机制：会答 5 种 kind、`harness_state`、`rlm.harness.*`
- 页索引模式：会答 markdown 页、四种 type、索引只做路由、正文要用文件工具读

`_entities` 目录被显式跳过（结构节点不是知识单元）；`status: archive` / `invalidated` 的页不进索引。

### 10.4 已知未做的部分

- **写路径没换**：`rlm.harness.create_memory` 仍然写 `harness_state.json`；页模式下的条目不可见。写入映射是下一步。
- 页索引模式的指纹版本升到 4，所有存量会话会重新投递一次 digest。
