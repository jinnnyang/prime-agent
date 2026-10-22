# 角色 / 工作区隔离（agent dir 与项目级资源）

Prime Agent 没有 `profile`、`workspace`、`role` 这类开关。系统提示词里那个叫
"session role" 的分段只描述递归深度和 daemon 状态（`session_role_section`，
`crates/pa-core/src/prompts/system_prompt.rs`），不是人格或角色设定。

隔离完全靠目录：**换一个 agent dir = 换一套记忆、技能、MCP 连接、会话和 daemon**。

## 三层状态

| 层 | 路径 | 作用域 |
| --- | --- | --- |
| agent dir | `$PRIME_AGENT_CODING_AGENT_DIR`，默认 `~/.prime/agent` | 机器级，一套角色一个 |
| 项目配置 | `<cwd>/.prime/agent/` | 项目级，同 cwd 的所有 agent dir 共享 |
| 祖先技能扫描 | `<祖先目录>/.agents/skills`、`~/.agents/skills` | 全局，所有 agent dir 共享 |

`CONFIG_DIR_NAME` = `.prime/agent`（`crates/pa-cli/src/config.rs`、
`crates/pa-core/src/settings/storage.rs`）。agent dir 决定隔离，`--cwd` 只换项目层。

## 各能力落在哪

| 能力 | 位置 | 换 agent dir 后 |
| --- | --- | --- |
| 记忆（continual harness） | `<agent_dir>/harness/harness_state.json` 的 `entries.memory` | 隔离 |
| 会话内局部 harness | `<session_artifact_dir>/harness/` | 隔离 |
| 用户技能 | `<agent_dir>/skills/` | 隔离 |
| 项目技能 | `<cwd>/.prime/agent/skills/` | 不隔离（同 cwd 共享） |
| 用户 MCP 服务器 | `<agent_dir>/settings.json` 的 `mcpServers` | 隔离 |
| MCP 连接记录 | `<agent_dir>/mcp-connections.json` | 隔离 |
| 凭证 | `<agent_dir>/auth.json` | 隔离（文件而非目录） |
| 模型表 | `<agent_dir>/models.json` | 隔离 |
| 会话历史 | `<agent_dir>/sessions/`（`PRIME_AGENT_SESSION_DIR` 可覆盖） | 隔离 |
| daemon 端点 | 见下节「daemon」 | **不隔离**，默认全机共用一个 |

记忆没有独立文件，它就是 harness 状态的一部分，所以隔离记忆不需要额外配置——换
agent dir 即可。`scope: global` 是"相对 agent dir 的全局"，不是"相对机器的全局"。

## daemon：默认不按 agent dir 隔离

daemon 端点路径与 agent dir 无关（`crates/pa-daemon/src/platform/paths.rs`）：

- Windows：固定命名管道 `\\.\pipe\prime-agent-daemon`
- Unix：`<tmpdir>/prime-agent-<uid>/daemon.sock`

只有 worker 描述符目录跟着 agent dir（`<agent_dir>/daemon-workers/`），
`prime-agent status` 也用它来找 tracked workers（`find_all_tracked_workers(&root.agent_dir)`，
`crates/pa-cli/src/daemon_discovery/mod.rs`）。

后果：三套 profile 默认共用**一个** daemon 进程和一条管道。要让 daemon 也按角色独立，
给每个角色显式钉住 socket（优先级：`--daemon-socket` 标志 > `PRIME_AGENT_DAEMON_SOCKET`
> 平台默认，`crates/pa-cli/src/config.rs`）：

```bash
export PRIME_AGENT_DAEMON_SOCKET='\\.\pipe\prime-agent-cad'   # Windows
# export PRIME_AGENT_DAEMON_SOCKET=/tmp/prime-agent-cad.sock # Unix
```

注意：一旦钉住，`prime-agent status` 里那条 socket 就不再算"产品默认端点"，
socket 目录的孤儿清扫也不再覆盖它（`current_state_root` 里那个
`default_socket_path == default_daemon_socket_path()` 判断）。

## 建一套角色

```bash
# 一个角色一个目录；skills/ 必须存在才会被扫描
mkdir -p ~/.prime/profiles/cad/skills

# 凭证和模型表要复制，不要软链：写入走 atomic rename（原子改名），会替换掉链接本身
cp ~/.prime/agent/auth.json   ~/.prime/profiles/cad/
cp ~/.prime/agent/models.json ~/.prime/profiles/cad/
```

启动：

```bash
PRIME_AGENT_CODING_AGENT_DIR=~/.prime/profiles/cad prime-agent --cwd ~/iot/cad
```

`harness/`、`sessions/`、`session-artifacts/`、`settings.json` 首次会话自动生成。

## 隔离边界

**固定共享，无法按 agent dir 关闭：**

- `<cwd>/.prime/agent/skills/` —— 项目级技能。这是有意的设计：同一项目下的所有角色
  应该看到同一套项目知识。要按角色区分能力，就放到各自的 `<agent_dir>/skills/`。
- `~/.agents/skills` 和祖先目录扫描 —— 见下节排除方法。

**需要显式排除时才隔离：** 在角色自己的 `settings.json` 写 `skills` 排除模式：

```json
{
  "skills": ["!**/gitnexus/**"]
}
```

模式前缀语义（`crates/pa-core/src/packages/resolve/patterns.rs`）：`!` 普通排除、
`+` 强制包含（精确）、`-` 强制排除（精确、最后应用）。匹配基准是 **agent dir**，
所以 `!../../.agents/skills/gitnexus/**` 这种相对父目录的写法不生效。

## MCP 隔离

`mcpServers` 是 settings 里的普通键，全局 `settings.json` 与项目
`.prime/agent/settings.json` **深合并**（`deep_merge`，`crates/pa-core/src/settings/merge.rs`），
项目覆盖全局同一 id，`null` 不覆盖任何值。

因此同一 agent dir 下的所有项目共享全局 `mcpServers`；要按项目区分，写进项目的
`.prime/agent/settings.json`；要按角色区分，写进各自的 `<agent_dir>/settings.json`。

`enabled: false` 可以强制关闭单个服务器，`enabledTools` / `disabledTools` 按名字裁剪工具。

## 技能来源优先级

加载顺序决定重名冲突谁赢（先到先赢，`crates/pa-core/src/skills/loader.rs`）。实际优先级由
`resource_precedence_rank`（`crates/pa-core/src/packages/resolve/mod.rs`）排序，数字小者先：

| rank | 来源 |
| --- | --- |
| 0 | 项目 `.prime/agent/settings.json` 的 `skills` 显式指定（local） |
| 1 | 项目 auto 发现：`<cwd>/.prime/agent/skills/`、祖先 `.agents/skills` |
| 2 | 用户 `<agent_dir>/settings.json` 的 `skills` 显式指定（local） |
| 3 | 用户 auto 发现：`<agent_dir>/skills/`、`~/.agents/skills` |
| 4 | package 来源 |
| 5 | 内置 bundled skills |

注意 auto 发现的目录是**并行**的：`<agent_dir>/skills/` 与 `~/.agents/skills` 同级，
所以这两个目录里同名技能谁先被扫到取决于遍历顺序，而显式写在 `settings.skills` 里的
条目永远优先于 auto 发现的。

CLI 追加与关闭：`--skill <path>` 追加单个，`-ns/--no-skills` 关闭全部发现，
`--no-context-files` 关闭 AGENTS.md / CLAUDE.md 读取。

## 验证隔离

```bash
# 技能：两个 agent dir 看同一个 cwd，各自只应看到自己的角色技能
PRIME_AGENT_CODING_AGENT_DIR=~/.prime/profiles/cad prime-agent prompt --cwd ~/iot/shared | grep '<name>'
PRIME_AGENT_CODING_AGENT_DIR=~/.prime/profiles/rust prime-agent prompt --cwd ~/iot/shared | grep '<name>'
```

`prime-agent prompt` 打印按层拆解的完整系统提示词，是用来确认"这个会话到底加载了什么"的
唯一权威手段（分段含 `packages` / `project-context` / `skills-inventory` / `mcp-servers` /
`environment` / `session-role`）。

## 已知陷阱

- **`HOME` 必须用原生路径**。在 Git-Bash 里设 `HOME=/c/Users/...` 会让
  `pa_types::platform::home_dir()` 解析出不可用路径，`~/.prime/agent/skills` 直接不加载。
  用 `HOME=C:/Users/...`。
- **凭证是复制不是共享**。三个角色三份 `auth.json`，在其中一个里重新登录 MCP 不会同步到
  另外两个。不重新登录就一直有效。
- **多套 profile 默认共用 daemon**。daemon 端点不随 agent dir 变，要独立就显式设
  `PRIME_AGENT_DAEMON_SOCKET`（见上）。`prime-agent status` 只报告当前 agent dir 的
  tracked workers，看不到别的 profile 启动的 daemon。
