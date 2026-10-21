settings.example.json — ~/.prime/agent/settings.json 的完整参考模板
（项目级覆盖文件：<项目>/.prime/agent/settings.json，结构与全局相同）

注意：settings.json 的解析器是严格的 serde JSON —— 不支持 // 注释和尾逗号。
本文件的注释仅供查阅，复制使用前必须删除所有注释。
未知键会被原样保留（向前兼容）；类型错误的键读取为 None，不报错。
来源：crates/pa-core/src/settings/types.rs

════════════════════════════════════════════
Onboarding
════════════════════════════════════════════
"onboardingShown": true,              // 首次启动引导是否已显示
"onboardingCompleted": null,          // 引导是否已完成（bool 或 null）

════════════════════════════════════════════
模型选择
════════════════════════════════════════════
"defaultProvider": "prime-inference", // 默认 provider id（如 prime-inference、anthropic、或 models.json 里的自定义 id）
"defaultModel": "provider/model-id",  // 默认模型，"provider/model-id" 或裸 model id
"subagentDefaultModel": null,         // 子代理默认模型；null 跟随 defaultModel
"recentModels": [                     // 最近使用模型（/model 选择器置顶），最多 20 条，程序自动维护
  "provider/model-id"
],
"auxiliaryModel": null,               // 辅助模型（摘要等后台任务用）；null = 用主模型
"defaultThinkingLevel": "low",        // 思考等级：off|minimal|low|medium|high|xhigh|max
"defaultServiceTier": null,           // 服务档位（如 "auto"、"priority"），透传给 provider
"providerBackupModel": null,          // 主模型不可用时的备选模型
"imageModel": null,                   // 会话模型不接受图片输入时代跑图片轮的模型；null = 报配置提示
"enabledModels": null,                // 联合模型允许列表（null = 不限制）
"allowedModels": null,                // Rust 独有 daemon 级允许列表：列表外模型直接报错，不回退；null = 不限制

════════════════════════════════════════════
更新
════════════════════════════════════════════
"updateChannel": "stable",            // 更新通道：stable|nightly（null = 默认 stable）

════════════════════════════════════════════
会话/队列行为
════════════════════════════════════════════
"steeringMode": "all",                // 插话排队模式：all|oneAtATime
"followUpMode": "all",                // 追加消息排队模式：all|oneAtATime
"rlmMaxDepth": 3,                     // RLM 子代理最大嵌套深度
"idleEvictionMinutes": 90,            // 空闲会话逐出分钟数；数字 | "off" | "none"
"sessionDir": null,                   // 会话文件目录覆盖（默认 ~/.prime/agent/sessions）
"chatDetail": "overview",             // Ctrl+O 循环的会话详情级别：overview|details|all
"treeFilterMode": "default",          // /tree 树视图初始过滤：default|no-tools|user-only|labeled-only|all

════════════════════════════════════════════
传输层
════════════════════════════════════════════
"transport": "auto",                  // provider 传输：auto|sse|websocket|websocket-cached

════════════════════════════════════════════
压缩（compaction）
════════════════════════════════════════════
"compaction": {
  "enabled": true,                    // 自动压缩开关
  "thresholdPercent": 95,             // 触发阈值（上下文百分比，1-100，可小数；非法值视为未设置）
  "reserveTokens": null,              // 为保留历史/摘要预留的 token 数
  "keepRecentTokens": null,           // 压缩后保留的最近消息 token 数
  "agentCallable": null               // 是否允许代理调用 compact
},

════════════════════════════════════════════
自动精炼（auto refine）
════════════════════════════════════════════
"autoRefine": {
  "enabled": null,                    // 自动 refine 开关
  "turnInterval": null,               // 每 N 轮触发一次
  "compact": null,                    // refine 时是否顺带压缩
  "cooldownMs": null                  // 两次 refine 之间的冷却（毫秒）
},

════════════════════════════════════════════
Harness / 代理轨迹 / 工厂 / 遥测
════════════════════════════════════════════
"harnessRefinements": true,           // harness 创建与上下文加载总开关；未设置 = 启用
"agentTraces": {
  "enabled": false                    // agent-traces 记录开关（还需 auth.json 里的 prime-agent-traces 凭据）
},
"factory": {
  "enabled": false                    // 编码代理工厂开关（/factory on 开启）；全局作用域独占读取
},
"telemetry": {
  "enabled": true,                    // 匿名遥测开关（DO_NOT_TRACK=1 环境变量也可禁用）
  "noticeShown": null,                // 遥测告知是否已展示（程序维护）
  "localMirror": true                 // 本地 JSONL 镜像 <agentDir>/telemetry.jsonl（默认开）
},
"branchSummary": {
  "reserveTokens": null,              // 分支摘要预留 token
  "skipPrompt": null                  // 跳过摘要前的确认提示
},

════════════════════════════════════════════
重试与故障转移
════════════════════════════════════════════
"retry": {
  "enabled": null,                    // 重试总开关
  "maxRetries": null,                 // 最大重试次数
  "baseDelayMs": null,                // 首次退避延迟
  "failover": {                       // provider 故障转移：同一模型有多个 provider 时切换
    "enabled": null,
    "maxRetries": 5,                  // 每个 provider 切换前重试次数（默认 5）
    "baseDelayMs": 1000,              // 首次退避，逐次翻倍（默认 1000ms）
    "maxDelayMs": 30000               // 单次退避上限（默认 30000ms）
  },
  "provider": {                       // 单 provider 请求级重试
    "timeoutMs": null,                // 请求超时
    "maxRetryDelayMs": null,          // 重试退避上限
    "waitForUsage": {                 // 配额/用量等待
      "enabled": null,
      "baseDelayMs": null,            // 首次等待
      "maxDelayMs": null,             // 等待上限
      "maxAttempts": null,
      "maxWaitMs": null,              // 单次配额事件总等待上限
      "pauseUntilReset": true,        // provider 报告的重置时间超过有界等待时挂起会话（默认 true）
      "maxPauseMs": 86400000,         // 单次挂起上限（默认 24h，钳到 7d）
      "maxParks": 8                   // 每个配额事件最多挂起次数（默认 8）
    }
  }
},

════════════════════════════════════════════
自主运行上限（每项：数字 | "unlimited"）
════════════════════════════════════════════
"autonomous": {
  "maxContinuations": null,           // 最大续跑次数
  "maxTurns": null,                   // 最大轮数
  "maxTokens": null,                  // 最大 token
  "timeoutMs": null                   // 超时毫秒
},

════════════════════════════════════════════
Shell / 内核
════════════════════════════════════════════
"shellPath": null,                    // 包裹所有 agent shell 命令的自定义 shell；项目级设置会覆盖全局（打开不可信仓库前先审查其项目设置！）
"shellCommandPrefix": null,           // 每条 shell 命令前注入的前缀（如 "shopt -s expand_aliases"）
"npmCommand": null,                   // npm 命令及参数，如 ["pnpm"]（安装 git/npm 包时用）

════════════════════════════════════════════
MCP 服务器
════════════════════════════════════════════
"mcpServers": {
  "my-http-server": {
    "type": "http",                   // http 型
    "url": "https://mcp.example.com/mcp",
    "headers": { "X-Custom": "value" },
    "bearerTokenEnvVar": "MY_MCP_TOKEN", // 存静态 bearer 的环境变量名（设置后跳过 OAuth）
    "oauth": true,                    // 对该服务器走通用 OAuth 登录流程
    "enabled": true,                  // false = 即使有凭据也强制禁用
    "enabledTools": null,             // 工具白名单
    "disabledTools": null,            // 工具黑名单
    "startupTimeoutMs": null,
    "callTimeoutMs": null,
    "clientId": null,                 // 预注册的 OAuth client
    "callbackPort": null              // OAuth 回调端口
  },
  "my-stdio-server": {
    "type": "stdio",                  // stdio 型（本地子进程）
    "command": "python3",
    "args": ["-m", "my_mcp_server"],
    "cwd": null,                      // 工作目录
    "env": { "API_KEY": { "env": "MY_API_KEY" } }, // 值为 {"env": "<变量名>"}，从内核环境解析
    "enabled": true,
    "enabledTools": null,
    "disabledTools": null,
    "startupTimeoutMs": null,
    "callTimeoutMs": null
  }
},
"mcpCatalogSources": null,            // 额外本地 MCP 服务目录文件（可 ~ 相对路径）；先到先得，不能覆盖内置 id

════════════════════════════════════════════
包 / 技能 / 提示词 / 主题
════════════════════════════════════════════
"packages": [                         // 已安装包来源字符串（本地路径相对 settings 基准目录；git/npm 原样保存）
  "github.com/org/some-package"
],
"skills": null,                       // 技能源列表
"prompts": null,                      // 提示词源列表
"themes": null,                       // 主题源列表
"enableSkillCommands": null,          // 是否把技能暴露为斜杠命令
"enableBuiltinSkills": null,          // 内置技能开关
"bundledSkills": {
  "websearch": true                   // 内置 websearch 技能开关
},

════════════════════════════════════════════
终端 UI
════════════════════════════════════════════
"theme": "dark",                      // 主题名
"terminal": {
  "showImages": true,                 // 终端内显示图片
  "clearOnShrink": true,              // 终端缩小时清屏
  "showTerminalProgress": true,       // 显示终端进度
  "fullscreenMouse": null             // 全屏模式鼠标支持
},
"images": {
  "autoResize": true,                 // 自动缩放图片
  "blockImages": false                // 完全屏蔽图片
},
"editorPaddingX": null,               // 输入框水平内边距
"autocompleteMaxVisible": null,       // 自动补全最大可见条数
"showHardwareCursor": null,           // 显示硬件光标
"markdown": {
  "codeBlockIndent": null,            // 代码块缩进
  "mermaid": "streaming"              // Mermaid 渲染：off|final|streaming
},
"warnings": {
  "anthropicExtraUsage": true         // Anthropic 额外用量警告
},

════════════════════════════════════════════
诊断 / 其他
════════════════════════════════════════════
"requestTiming": false,               // 把每请求的 provider 计时阶段写入诊断日志（默认关）
"thinkingBudgets": {                  // 各思考等级的 token 预算
  "minimal": null,
  "low": null,
  "medium": null,
  "high": null
}
