models.example.json — ~/.prime/agent/models.json 的完整参考模板

注意：models.json 解析器支持 // 行注释和尾逗号（strip_json_comments 会先剥离），
所以本文件可以直接 `cp models.example.json models.json` 使用。
用途：定义自定义 provider/模型、覆盖内置 provider 与模型的字段。
内置模型来自编译进二进制的 fallback + 缓存（<agentDir>/models/），不经过本文件。
来源：crates/pa-core/src/models/custom.rs（ModelsConfig / validate_config）

校验规则（validate_config）：
- 自定义 provider 定义了 models 时：baseUrl 和 apiKey 必填。
- 空 provider（无 models）必须至少有 baseUrl、headers、compat、modelOverrides 之一。
- 每个模型必须有 id，且 contextWindow/maxTokens 不为 0。
{
  "providers": {
    // ════════════════════════════════════════════
    // 自定义 provider（OpenAI 兼容示例）
    // ════════════════════════════════════════════
    "aptapi": {
      "name": "AptAPI",                       // 显示名（可选）
      "baseUrl": "https://aptapi.dev/v1",     // API base URL（自定义 provider 定义 models 时必填）
      "apiKey": "sk-...",                     // API key（同上必填；也可放 auth.json，二选一）
      "api": "openai-responses",              // API 协议：openai-completions | openai-responses | anthropic-messages 等
      "authHeader": true,                     // 可选：是否用标准 Authorization 头携带 key
      "headers": {                            // 额外请求头（可选）
        "X-Title": "prime-agent"
      },
      "compat": {                             // provider 级兼容块（可选，见下方 compat 说明）
        "supportsStore": false
      },
      "models": [                             // 自定义模型列表
        {
          "id": "light",                      // 模型 id（必填）
          "name": "Light",                    // 显示名（可选）
          "api": null,                        // 可选：覆盖 provider 级 api
          "baseUrl": null,                    // 可选：覆盖 provider 级 baseUrl
          "reasoning": true,                  // 是否推理模型
          "thinkingLevelMap": {               // 思考等级 -> provider 词汇的映射（键：off|minimal|low|medium|high|xhigh|max；值 null = off）
            "off": null,
            "low": "low",
            "high": "high"
          },
          "input": ["text", "image"],         // 支持的输入模态：text|image|audio|video
          "cost": {                           // 每百万 token 成本（可选，用于成本显示）
            "input": 0.5,
            "output": 1.5,
            "cacheRead": 0.1,
            "cacheWrite": 0.2
          },
          "contextWindow": 1000000,           // 上下文窗口（必填非 0）
          "maxTokens": 65536,                 // 最大输出 token（必填非 0）
          "headers": { "X-Model-Header": "v" }, // 模型级额外请求头（可选）
          "compat": {                         // 模型级兼容块（可选）
            "thinkingFormat": "deepseek",
            "requiresReasoningContentOnAssistantMessages": true
          }
        }
      ],
      "modelOverrides": {                     // 覆盖已有模型（内置或自定义），按 id 匹配
        "big": {                              // 例：给内置/同 id 模型改上下文窗口（README.md:160 的用法）
          "contextWindow": 524288,
          "name": "Big (overridden)",
          "reasoning": true,
          "thinkingLevelMap": null,
          "input": ["text"],
          "cost": { "input": 1.0, "output": 2.0, "cacheRead": null, "cacheWrite": null },
          "maxTokens": 32768,
          "headers": { "X-Override": "1" },
          "compat": { "supportsStore": false }
        }
      }
    },

    // ════════════════════════════════════════════
    // 仅覆盖内置 provider（不定义 models）：改 baseUrl / 头 / compat
    // ════════════════════════════════════════════
    "prime-inference": {
      "headers": { "X-Custom-Header": "value" }
    }
  }
}

════════════════════════════════════════════
compat 块可用的键（按 API 协议分三种；JSON 上无标签，
按特征键嗅探：supportsEagerToolInputStreaming -> anthropic，
sendSessionIdHeader -> responses，其余 -> completions）
════════════════════════════════════════════
openai-completions（OpenAI 兼容补全）:
  supportsStore                                        // 是否支持 store 参数
  supportsDeveloperRole                                // 是否支持 developer 角色
  supportsReasoningEffort                              // 是否支持 reasoning_effort
  supportsUsageInStreaming                             // 流式是否带 usage
  supportsSessionAffinityHeaders                       // 会话亲和头
  maxTokensField: max_completion_tokens | max_tokens   // max token 字段名
  requiresToolResultName                               // 工具结果是否需带名字
  requiresAssistantAfterToolResult                     // 工具结果后是否需 assistant 消息
  requiresThinkingAsText                               // 思考是否需以文本发送
  requiresReasoningContentOnAssistantMessages          // assistant 消息是否需带 reasoning_content
  thinkingFormat: openai|openrouter|deepseek|zai|qwen|qwen-chat-template
  cacheControlFormat: anthropic                        // 缓存控制格式
  openRouterRouting                                    // OpenRouter 路由块
  vercelGatewayRouting                                 // Vercel 网关路由块
  supportsLongCacheRetention                           // 长缓存保留
openai-responses:
  sendSessionIdHeader                                  // 是否发送会话 id 头
  supportsLongCacheRetention
anthropic-messages:
  supportsEagerToolInputStreaming                      // 工具入参提前流式
  supportsLongCacheRetention
