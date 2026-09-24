use base64::Engine;
use serde_json::Value as JsonValue;
use url::Url;

pub const CODEX_OPEN_APP_URL: &str = "https://chatgpt.com/codex/open-app";

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub enum LoginSuccessPage {
    #[default]
    Local,
    Hosted {
        url: Url,
        app_brand: LoginSuccessPageBrand,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LoginSuccessPageBrand {
    Codex,
    Chatgpt,
}

impl LoginSuccessPageBrand {
    fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Chatgpt => "chatgpt",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum LoginSuccessRedirect {
    Local(String),
    Hosted(String),
}

/// [ad-agent] 上游会从 id_token 里读 organization_id / project_id /
/// completed_platform_onboarding / is_org_owner，据此把用户重定向到
/// platform.openai.com 的 /org-setup 去补付款方式。我们的服务端不签发这些 claim，
/// 这条路永远走不到；而且那是上游的计费页面，不该出现在本产品里，整段删除。
/// 顺带把 id_token 从成功页的 query 里去掉 —— 凭证不该落进浏览器历史。
pub(crate) fn compose_success_url(
    port: u16,
    _issuer: &str,
    _id_token: &str,
    _access_token: &str,
    codex_streamlined_login: bool,
    login_success_page: &LoginSuccessPage,
) -> LoginSuccessRedirect {
    if let LoginSuccessPage::Hosted { url, app_brand } = login_success_page {
        let mut success_url = url.clone();
        success_url.set_query(None);
        success_url
            .query_pairs_mut()
            .append_pair("source", "login")
            .append_pair("app_brand", app_brand.as_str());
        return LoginSuccessRedirect::Hosted(success_url.into());
    }

    let mut params: Vec<(&str, String)> = Vec::new();
    if codex_streamlined_login {
        params.push(("codex_streamlined_login", "true".to_string()));
    }
    let query = params
        .into_iter()
        .map(|(key, value)| format!("{key}={}", urlencoding::encode(&value)))
        .collect::<Vec<_>>()
        .join("&");
    LoginSuccessRedirect::Local(format!("http://localhost:{port}/success?{query}"))
}

pub(crate) fn jwt_auth_claims(jwt: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut parts = jwt.split('.');
    let (_header, payload, _signature) = match (parts.next(), parts.next(), parts.next()) {
        (Some(header), Some(payload), Some(signature))
            if !header.is_empty() && !payload.is_empty() && !signature.is_empty() =>
        {
            (header, payload, signature)
        }
        _ => {
            eprintln!("Invalid JWT format while extracting claims");
            return serde_json::Map::new();
        }
    };
    match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(mut value) => {
                if let Some(claims) = value
                    .get_mut("https://api.openai.com/auth")
                    .and_then(JsonValue::as_object_mut)
                {
                    return claims.clone();
                }
                // [ad-agent] 我们的 id_token 是扁平 claim，没有这层命名空间对象。
                // 退回整个 payload，并把 account_id 映射成下游取用的 chatgpt_account_id
                // （persist_tokens_async 和 ensure_workspace_allowed 都按后者取值）。
                if let Some(claims) = value.as_object_mut() {
                    if !claims.contains_key("chatgpt_account_id")
                        && let Some(account_id) = claims.get("account_id").cloned()
                    {
                        claims.insert("chatgpt_account_id".to_string(), account_id);
                    }
                    return claims.clone();
                }
            }
            Err(error) => {
                eprintln!("Failed to parse JWT JSON payload: {error}");
            }
        },
        Err(error) => {
            eprintln!("Failed to base64url-decode JWT payload: {error}");
        }
    }
    serde_json::Map::new()
}

#[cfg(test)]
#[path = "success_page_tests.rs"]
mod tests;
