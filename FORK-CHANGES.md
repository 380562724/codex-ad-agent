# ad-agent fork 相对 openai/codex 的改动

分叉基点：`openai/codex@5a9eb145`（2026-09-10）
分支：`feat/cowork-auth`

所有改动都用 `// [ad-agent]` 注释标记，便于日后 merge 上游时定位。
原则：能改常量就不改函数体，能让服务端适配就不改客户端。

---

## 1. 认证端点指向自己的服务器

| 文件 | 原值 | 现值 |
|---|---|---|
| `login/src/server.rs` `DEFAULT_ISSUER` | `https://auth.openai.com` | `https://quchenyang.com` |
| `login/src/auth/manager.rs` `REFRESH_TOKEN_URL` | `https://auth.openai.com/oauth/token` | `https://quchenyang.com/oauth/token` |
| `login/src/auth/manager.rs` `REVOKE_TOKEN_URL` | `https://auth.openai.com/oauth/revoke` | `https://quchenyang.com/oauth/revoke` |
| `login/src/auth/manager.rs` `CLIENT_ID` | `app_EMoamEEZ73f0CkXaXp7hrann` | `codex-cli` |

**这四处必须同时改。** 刷新/吊销端点是独立常量，不跟随 issuer；只改 issuer 会造成
"登录成功、几十分钟后刷新时打到 auth.openai.com"的延迟故障。

`cli/src/login.rs` 和 `app-server/.../account_processor.rs` 是两条独立的登录路径，
都通过 `ServerOptions::new()` + `oauth_client_id()` 取上述常量，因此无需分别修改。

**已知缺口**：服务端尚未实现 `/oauth/revoke`，logout 时该请求会 404。
调用方只 `tracing::warn!` 不中断（`manager.rs:984`），本地凭证照常清除，
但 refresh token 在服务端会一直有效到自然过期。

## 2. id_token claim 从命名空间改为扁平

上游的 id_token 把信息塞在 `https://api.openai.com/auth` 这个命名空间对象里，
我们的服务端签发扁平 claim（`account_id`、`sub`）。

- `login/src/token_data.rs`：`IdClaims` 增加 `account_id`，解析时命名空间优先、
  缺失时回落到扁平字段。（注意：只兜底 `account_id`。曾经顺手用 `sub` 兜底
  `chatgpt_user_id`，会让上游 20 个 auth_refresh 用例的期望值变化，而这个字段
  在登录链路里没有消费方，已去掉。）
- `login/src/success_page.rs` `jwt_auth_claims()`：命名空间对象不存在时退回整个 payload，
  并把 `account_id` 映射成下游取用的 `chatgpt_account_id`。

**为什么必须做**：`account_id` 为空会让 `reload_if_account_id_matches` 直接返回
`Skipped`（`manager.rs:2462`），刷新链路判定为**永久失败**，用户被迫重新登录。

## 3. 删除注定失败的 API key token-exchange

`login/src/server.rs`：删除 `obtain_api_key()` 及其调用。上游在换码成功后会再用
RFC 8693 token-exchange 换一把 OpenAI API key，我们的服务端不提供这种 grant，
请求必然 400（上游本来也只是 `.ok()` 丢掉结果）。

`login/tests/suite/login_server_e2e.rs` 里断言 `OPENAI_API_KEY == "access-123"` 的用例
改为断言该字段不存在 —— 上游注释里本来就写了"这个机制删除后测试应当这样改"。

## 4. 授权 URL 只留标准 OAuth 参数

`login/src/server.rs` `build_authorize_url()` 移除：
`id_token_add_organizations`、`codex_cli_simplified_flow`、`originator`，
scope 里的 `api.connectors.read` / `api.connectors.invoke`。

这些是上游私有参数，服务端不认，而且会出现在**用户看得见的授权链接**里。

## 5. 去品牌化

- `login/src/assets/success_legacy.html`（默认成功页）：标题 `Signed in to Codex`
  → `Signed in to ad-agent`；删除跳转 `platform.openai.com/org-setup` 补付款方式的整段逻辑。
- `login/src/assets/success.html`（streamlined 成功页）：`ChatGPT` 字标 → `ad-agent`；
  同样删除上游计费重定向。
- `login/src/success_page.rs` `compose_success_url()`：删除
  `platform_url` / `org_id` / `project_id` / `plan_type` / `needs_setup` 参数。
  **顺带把 `id_token` 从成功页 query 里去掉** —— 凭证不该落进浏览器历史。
- `login/src/success_page_tests.rs`：删除 `compose_success_url_keeps_setup_on_local_page`
  用例（它测的就是上面删掉的功能）。
- `app-server/.../account_processor.rs`：不再使用上游托管成功页（chatgpt.com），
  一律用本地成功页。

**尚未处理**（不在登录链路上，属于其他子系统）：
`rmcp-client/src/oauth_client_registration.rs`、
`app-server-transport/.../remote_control/protocol.rs`、
`thread-manager-sample/src/main.rs` 里的 chatgpt.com 引用；
`login/src/assets/error.html` 的 "Codex login" 字样（是工具自身名称，二进制仍叫 codex）；
success.html 里的 "Open Codex" 按钮与 `codex://threads/new` 深链。

---

## 待办

- [ ] **模型请求仍然打到上游**：`core/src/config/mod.rs:4313` 的
      `chatgpt_base_url` 默认值是 `https://chatgpt.com/backend-api/`。
      "所有请求从自己服务器发"这个目标还没做，取决于服务端是否提供兼容的模型转发端点。
- [ ] 登录闸门：见下方分析，现有 `forced_login_method` 机制拦不住
      `CODEX_ACCESS_TOKEN` / workload identity 等非交互式注入路径。
