auth.example.json — ~/.prime/agent/auth.json 的完整参考模板

注意：auth.json 是锁定的凭据文件（0600，原子写），存真实密钥——不是注释友好的
JSON（严格 serde 解析），注释仅供查阅。真实文件由登录流程写入：
`prime-agent` 的 provider login（API key）、OAuth 登录、/mcp api-key 录入等。
顶层结构：{ "<provider-id>": <凭据> }。未知 provider 键原样保留（往返兼容）。
来源：crates/pa-core/src/auth/types.rs（AuthCredential）

顶层键（常见）：
  "prime-inference"    Prime Inference API key 或 OAuth（含 primeTeam 团队信息）
  "serper"             Serper 网络搜索 API key（websearch 技能运行时读取）
  "codex"              Codex 订阅 OAuth（带 accountId）
  "copilot"            Copilot OAuth（可带 enterpriseUrl）
  "mcp:<connectionId>" MCP 服务器的 OAuth / 静态 token 凭据
  任意自定义 id        models.json 里定义的自定义 provider 的 API key

────────────────────────────────────────────
凭据形态 1：api_key（最常见）
────────────────────────────────────────────
{
  "my-provider": {
    "type": "api_key",
    "key": "sk-..."
    // "primeTeam": {                    // 仅 prime-inference：key 绑定的团队
    //   "teamId": "team_abc",           // 团队 id（必填）
    //   "name": "My Team",              // 显示名（必填）
    //   "slug": "my-team",              // 可选
    //   "role": "member",               // 可选
    //   "createdAt": "2026-01-01T00:00:00Z" // 可选
    // }
  }
}

────────────────────────────────────────────
凭据形态 2：oauth
────────────────────────────────────────────
{
  "codex": {
    "type": "oauth",
    "access": "<access token>",         // 必填
    "refresh": "<refresh token>",       // 可选：无则过期后需重新登录
    "expires": 1735689600,              // 必填：过期时间（epoch 秒）
    "accountId": "acct_...",            // 可选：Codex 的 ChatGPT 账户 id（请求路径从 token 重新提取）
    "enterpriseUrl": "https://ghe.example.com", // 可选：Copilot 登录的 GHE 域；刷新与 base URL 经它解析
    "endpoint": "https://mcp.example.com/mcp",  // 可选：MCP 登录的端点绑定（mcp:<id> 凭据）；消费者拒绝发到别处
    "tokenEndpoint": "https://auth.example.com/token", // 可选：token 交换端点（刷新复用）
    "clientId": "client_123",           // 可选：登录时注册/预配置的 OAuth client
    "resource": "https://mcp.example.com", // 可选：RFC 9728 资源指示符（存在即 protected-resource-metadata 登录）
    "issuer": "https://auth.example.com",  // 可选：RFC 8414/OIDC issuer
    "audienceMode": "exact"             // 可选：resource 与 endpoint 的关联方式：exact|origin；老凭据无此键
  }
}

────────────────────────────────────────────
凭据形态 3：mcp_static_token（粘贴的静态 MCP token）
────────────────────────────────────────────
{
  "mcp:my-server-1": {
    "type": "mcp_static_token",
    "bearer": "<literal bearer token>", // 必填
    "endpoint": "https://mcp.example.com/mcp" // 可选：绑定的端点；无过期时间
  }
}

────────────────────────────────────────────
完整最小示例（多 provider 共存）
────────────────────────────────────────────
{
  "prime-inference": { "type": "api_key", "key": "sk-...", "primeTeam": { "teamId": "t1", "name": "Team One" } },
  "serper": { "type": "api_key", "key": "serper-key" },
  "my-provider": { "type": "api_key", "key": "sk-..." }
}
