# 嵌入方接入指南：凭证直传（Cookie / Bearer）

plugin-hub 嵌入其他项目使用时，嵌入方与 hub **共用平台登录态**（由所配身份插件
对接的 SSO 决定）：用户在嵌入方已登录，不必再向 hub 报一遍账号密码。本文说明
三种接入形态、两条凭证通道与相关配置。

## 两条凭证通道

| 通道 | 形态 | 验证链路 | 适用 |
|---|---|---|---|
| Cookie | 请求带浏览器 Cookie 原文 | 由身份插件的 Cookie 验证链路验证 | 同域反代、同站跨域嵌入 |
| Bearer | `Authorization: Bearer <平台token>` | 只走平台直验（token 即 SSO 签发的登录态） | 跨域 fetch、MCP 客户端 headers、服务端转发 |

两条通道**同现时 Bearer 优先**（显式传 token 意图明确；Cookie 是存量通道）。
两条路给出的调用主体是同一个人：`userCode` 一律取平台 username。

## 形态一：同域反代（零配置）

嵌入方的 nginx 把 hub 反代到同域路径下（`deploy/nginx/` 是现成样例）。浏览器
自动携带 Cookie，hub 侧无需任何配置。

## 形态二：跨域嵌入（CORS 白名单）

嵌入方前端与 hub **跨 origin 但同站**（不同端口/子域，如 `*.example.com` 体系内）时，
hub 需要应答 CORS 头。在部署目录的 `.env` 配置：

```bash
# 逗号分隔的 Origin 白名单（协议+域名+端口精确匹配）。留空 = 不挂 CORS 层
HUB_CORS_ALLOWED_ORIGINS=https://console.example.com,https://app.example.com:8443
```

行为要点：

- 白名单内：预检 OPTIONS 短路 204，实际请求应答 `Access-Control-Allow-Origin`
  （精确回显）+ `Allow-Credentials: true`。嵌入方前端 fetch 需带
  `credentials: 'include'`。
- **刻意不支持 `*`**：带凭证的 CORS 规范禁止通配 Origin，而嵌入要带的正是 Cookie。
- 白名单外：不加 CORS 头（浏览器自行拦截）；服务端转发、curl 等非浏览器调用方
  不受影响。
- 同站 Cookie 不受浏览器第三方 Cookie 封锁影响。**完全跨站**（不同 eTLD+1）嵌入
  时 Cookie 通道不可靠，请改用 Bearer 通道。

## 形态三：MCP 客户端 headers 直传

MCP 客户端（ZCode/Cursor 等）在 mcp 配置的 `headers` 里带凭证，HTTP 层鉴权中间件
统一处理：

```jsonc
// ZCode 的 mcp 配置示例（Bearer 通道，推荐）
{
  "mcpServers": {
    "plugin-hub": {
      "url": "https://<hub-host>/mcp",
      "headers": {
        "Authorization": "Bearer <平台登录态 token>"
      }
    }
  }
}

// Cookie 通道同样可用
{
  "mcpServers": {
    "plugin-hub": {
      "url": "https://<hub-host>/mcp",
      "headers": { "Cookie": "<完整 Cookie 串>" }
    }
  }
}
```

### 登录闸门仍然保留

`HUB_MCP_LOGIN_GATE=true` 时，**无凭证或凭证失效**的 MCP 请求会回退到账号密码
闸门（elicitation 弹窗 / 指引卡）；带有效凭证的请求**最优先**使用请求自带的身份。
闸门关闭时无凭证请求维持 401。

## 身份优先级（实现语义）

同一请求可用的身份来源按此排序：

1. **请求自带凭证**（Cookie/Bearer，经鉴权中间件验过）——「这次调用是谁」的答案
2. **login 缓存**（进程级单槽，MCP `login` 工具建立）——仅对无凭证请求兜底
3. **账号密码闸门**（elicitation / 指引卡）——前两者皆无时

嵌入场景多用户共用 hub 进程时，第 1 级保证 B 用户不会被 A 的缓存身份顶替。

## 登录态透传给下游插件

验过的 **Bearer token** 会随信封 meta（`hub.mas_token`）透传给下游插件——需要
平台登录态调平台接口的插件在嵌入场景端到端可用。**Cookie 形态不透传**：那是
SSO 会话，对下游要平台登录态的插件说不通。优先级同样是请求凭证 > login 缓存。

## token 验证缓存（性能与撤权延迟）

身份插件配了缓存密钥后，Bearer token 的验证结果默认缓存 5 分钟（可配）：

```bash
# 身份插件侧（示例命名，以插件实际配置面为准）
AUTH_CACHE_SECRET=<openssl rand -hex 32>
AUTH_TOKEN_CACHE_TTL=5m   # 缺省 5m；Go duration
```

- 缓存省掉「每请求两趟平台外呼」；缓存的是**验证结果**（他是谁、是不是超管），
  **不是权限**——scopes 每次现算。
- 代价：token 被撤销/过期后，最长一个 TTL 内仍表现为有效。5 分钟是保守点。
- fail-open：没配密钥、中台不可达、解不开、过期——一律当未命中走真验。

## 配置清单（新增项）

| 环境变量 | 归属 | 缺省 | 说明 |
|---|---|---|---|
| `HUB_CORS_ALLOWED_ORIGINS` | hub | 空 = 不挂 CORS 层 | 逗号分隔 Origin 白名单 |
| `AUTH_TOKEN_CACHE_TTL` | 身份插件 | `5m` | token 验证结果缓存 TTL |
| `AUTH_CACHE_SECRET` | 身份插件 | 空 = 缓存禁用 | 两个缓存共用（已存在，语义扩展） |

## 故障对照

| 症状 | 原因 | 处置 |
|---|---|---|
| 跨域 fetch 全部失败，控制台报 CORS | 未配 `HUB_CORS_ALLOWED_ORIGINS` 或 Origin 不在白名单 | 补白名单（精确协议+域名+端口） |
| 预检通过但实际请求 401 | 前端 fetch 没带 `credentials: 'include'` | 前端补 credentials |
| MCP 调用拿指引卡要求 login | headers 没配 / token 已失效 | 客户端 headers 补 Bearer，或按指引卡走 login |
| 撤权后最长 5 分钟仍有效 | token 验证缓存 TTL | 预期内；调小 `AUTH_TOKEN_CACHE_TTL` 可收紧 |
