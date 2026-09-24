use base64::Engine;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::auth::PlanType;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Default)]
pub struct TokenData {
    /// Flat info parsed from the JWT in auth.json.
    #[serde(
        deserialize_with = "deserialize_id_token",
        serialize_with = "serialize_id_token"
    )]
    pub id_token: IdTokenInfo,

    /// This is a JWT.
    pub access_token: String,

    pub refresh_token: String,

    pub account_id: Option<String>,
}

/// Flat subset of useful claims in id_token from auth.json.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IdTokenInfo {
    pub email: Option<String>,
    /// The ChatGPT subscription plan type
    /// (e.g., "free", "plus", "pro", "business", "enterprise", "edu").
    /// (Note: values may vary by backend.)
    pub chatgpt_plan_type: Option<PlanType>,
    /// ChatGPT user identifier associated with the token, if present.
    pub chatgpt_user_id: Option<String>,
    /// Organization/workspace identifier associated with the token, if present.
    pub chatgpt_account_id: Option<String>,
    /// Whether the selected ChatGPT workspace must route through the FedRAMP edge.
    pub chatgpt_account_is_fedramp: bool,
    pub raw_jwt: String,
}

impl IdTokenInfo {
    pub fn get_chatgpt_plan_type(&self) -> Option<String> {
        self.chatgpt_plan_type.as_ref().map(|t| match t {
            PlanType::Known(plan) => plan.display_name().to_string(),
            PlanType::Unknown(s) => s.clone(),
        })
    }

    pub fn get_chatgpt_plan_type_raw(&self) -> Option<String> {
        self.chatgpt_plan_type.as_ref().map(|t| match t {
            PlanType::Known(plan) => plan.raw_value().to_string(),
            PlanType::Unknown(s) => s.clone(),
        })
    }

    pub fn is_workspace_account(&self) -> bool {
        matches!(
            self.chatgpt_plan_type,
            Some(PlanType::Known(plan)) if plan.is_workspace_account()
        )
    }

    pub fn is_fedramp_account(&self) -> bool {
        self.chatgpt_account_is_fedramp
    }
}

#[derive(Deserialize)]
struct IdClaims {
    #[serde(default)]
    email: Option<String>,
    #[serde(rename = "https://api.openai.com/profile", default)]
    profile: Option<ProfileClaims>,
    #[serde(rename = "https://api.openai.com/auth", default)]
    auth: Option<AuthClaims>,
    // [ad-agent] 我们的服务端签发扁平 claim，没有上游那层命名空间对象。
    // account_id 一旦为空，reload_if_account_id_matches 会直接 Skipped，
    // 刷新链路会判定为永久失败（manager.rs:2462），所以这个字段是必需的。
    #[serde(default)]
    account_id: Option<String>,
    // [ad-agent] 扁平 claim 里用户 id 就是标准的 sub。它必须能解析出来：same_owner 靠
    // (chatgpt_user_id, account_id) 判断刷新前后是不是同一个人，user id 为空会让每次刷新
    // token 都被当成换号，进而清空应用网络策略，之后所有请求都报
    // "application network policy is unavailable"。
    #[serde(default)]
    sub: Option<String>,
}

#[derive(Deserialize)]
struct ProfileClaims {
    #[serde(default)]
    email: Option<String>,
}

#[derive(Deserialize)]
struct AuthClaims {
    #[serde(default)]
    chatgpt_plan_type: Option<PlanType>,
    #[serde(default)]
    chatgpt_user_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_account_is_fedramp: bool,
}

#[derive(Deserialize)]
struct StandardJwtClaims {
    #[serde(default)]
    exp: Option<i64>,
}

#[derive(Deserialize)]
struct AccountUserClaims {
    #[serde(rename = "https://api.openai.com/auth")]
    auth: Option<AccountUserAuthClaims>,
}

#[derive(Deserialize)]
struct AccountUserAuthClaims {
    chatgpt_account_user_id: Option<String>,
    chatgpt_account_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum IdTokenInfoError {
    #[error("invalid ID token format")]
    InvalidFormat,
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

fn decode_jwt_payload<T: DeserializeOwned>(jwt: &str) -> Result<T, IdTokenInfoError> {
    // JWT format: header.payload.signature
    let mut parts = jwt.split('.');
    let (_header_b64, payload_b64, _sig_b64) = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s)) if !h.is_empty() && !p.is_empty() && !s.is_empty() => (h, p, s),
        _ => return Err(IdTokenInfoError::InvalidFormat),
    };

    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64)?;
    let claims = serde_json::from_slice(&payload_bytes)?;
    Ok(claims)
}

pub fn parse_jwt_expiration(jwt: &str) -> Result<Option<DateTime<Utc>>, IdTokenInfoError> {
    let claims: StandardJwtClaims = decode_jwt_payload(jwt)?;
    Ok(claims
        .exp
        .and_then(|exp| DateTime::<Utc>::from_timestamp(exp, 0)))
}

// Keep membership parsing separate from ordinary auth claims: a malformed optional
// membership must disable local verification without breaking an otherwise valid login.
// Both paths share decode_jwt_payload for the JWT envelope.
pub(crate) fn parse_chatgpt_account_user_id(
    jwt: &str,
    account_id: &str,
) -> Result<Option<String>, IdTokenInfoError> {
    if account_id.is_empty() || account_id.trim() != account_id {
        return Ok(None);
    }
    if jwt.split('.').count() != 3 {
        return Err(IdTokenInfoError::InvalidFormat);
    }
    let claims: AccountUserClaims = decode_jwt_payload(jwt)?;
    let Some(auth) = claims.auth else {
        return Ok(None);
    };
    if auth.chatgpt_account_id.as_deref() != Some(account_id) {
        return Ok(None);
    }
    Ok(auth
        .chatgpt_account_user_id
        .filter(|id| !id.is_empty() && id.trim() == id.as_str()))
}

pub fn parse_chatgpt_jwt_claims(jwt: &str) -> Result<IdTokenInfo, IdTokenInfoError> {
    let IdClaims {
        email,
        profile,
        auth,
        account_id,
        sub,
    } = decode_jwt_payload(jwt)?;
    let email = email.or_else(|| profile.and_then(|profile| profile.email));

    // [ad-agent] 命名空间 claim 优先（兼容上游签发的 token），缺失时回落到扁平 claim。
    match auth {
        Some(auth) => Ok(IdTokenInfo {
            email,
            raw_jwt: jwt.to_string(),
            chatgpt_plan_type: auth.chatgpt_plan_type,
            chatgpt_user_id: auth.chatgpt_user_id.or(auth.user_id),
            chatgpt_account_id: auth.chatgpt_account_id.or(account_id),
            chatgpt_account_is_fedramp: auth.chatgpt_account_is_fedramp,
        }),
        None => Ok(IdTokenInfo {
            email,
            raw_jwt: jwt.to_string(),
            chatgpt_plan_type: None,
            // [ad-agent] Only Cowork-issued tokens (identified by the flat account_id claim)
            // carry the user id in `sub`; other namespace-less tokens keep upstream behavior.
            chatgpt_user_id: account_id.as_ref().and(sub),
            chatgpt_account_id: account_id,
            chatgpt_account_is_fedramp: false,
        }),
    }
}

fn deserialize_id_token<'de, D>(deserializer: D) -> Result<IdTokenInfo, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    parse_chatgpt_jwt_claims(&s).map_err(serde::de::Error::custom)
}

fn serialize_id_token<S>(id_token: &IdTokenInfo, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&id_token.raw_jwt)
}

#[cfg(test)]
#[path = "token_data_tests.rs"]
mod tests;
