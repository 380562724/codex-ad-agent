// [ad-agent] Fetches model configuration from the server so it can be inserted as a
// dedicated `ConfigLayerSource::ServerConfig` layer (see `apply_server_config_layer` and
// core/src/config/mod.rs `build_inner`). Every caller that builds a `ConfigLayerStack` from
// scratch (core's `build_inner`, and app-server's `ConfigManager::load_config_layers`) must
// route through `apply_server_config_layer` — otherwise that caller's effective config silently
// falls back to whatever local/session model is on disk, even though the server fetch itself
// succeeds elsewhere in the process. See SERVER-DRIVEN-CONFIG history.
mod fetch;
mod flatten;

use std::path::Path;
use std::sync::Arc;

use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_config::build_cli_overrides_layer;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::AuthRouteConfig;
use toml::Value as TomlValue;

pub use fetch::AdAgentConfigError;

const DEFAULT_CONFIG_ENDPOINT: &str = "https://quchenyang.com/agent/config";
const OVERRIDE_ENV: &str = "AD_AGENT_CONFIG_URL";

fn config_endpoint() -> String {
    std::env::var(OVERRIDE_ENV).unwrap_or_else(|_| DEFAULT_CONFIG_ENDPOINT.to_string())
}

/// Fetches server-driven model config overrides for the given `CODEX_HOME`.
///
/// Builds its own minimal [`AuthManager`] (not shared with the caller) purely to read
/// `auth.json` and get a fresh, auto-refreshed access token — `build_inner` runs before
/// any application-level `AuthManager` exists, see FORK-CHANGES / SERVER-DRIVEN-CONFIG history.
pub async fn fetch_model_overrides(
    codex_home: &Path,
) -> Result<Vec<(String, TomlValue)>, AdAgentConfigError> {
    let auth_manager = AuthManager::new(
        codex_home.to_path_buf(),
        /* enable_codex_api_key_env */ false,
        AuthCredentialsStoreMode::default(),
        /* forced_chatgpt_workspace_id */ None,
        /* chatgpt_base_url */ None,
        AuthKeyringBackendKind::default(),
        AuthRouteConfig::from_http_client_factory(HttpClientFactory::new(
            OutboundProxyPolicy::ReqwestDefault,
        )),
    )
    .await;
    fetch_model_overrides_with_auth_manager(&Arc::new(auth_manager), &config_endpoint()).await
}

async fn fetch_model_overrides_with_auth_manager(
    auth_manager: &Arc<AuthManager>,
    url: &str,
) -> Result<Vec<(String, TomlValue)>, AdAgentConfigError> {
    let auth = auth_manager
        .auth()
        .await
        .ok_or(AdAgentConfigError::NotLoggedIn)?;
    let access_token = auth
        .get_token()
        .map_err(|_| AdAgentConfigError::NotLoggedIn)?;
    let http_client = reqwest::Client::new();
    fetch::fetch_model_overrides_from(url, &access_token, &http_client).await
}

/// Fetches server-driven model config and, on success with a non-empty result, returns
/// `stack` with a `ConfigLayerSource::ServerConfig` layer inserted (outranking `SessionFlags`,
/// see the module doc comment). On fetch failure or an empty response, returns `stack`
/// unchanged; the fetch `Result` is returned alongside so the caller can still surface a
/// fetch error where that caller is expected to (e.g. `Config::ad_agent_config_status`).
///
/// Every place that assembles a `ConfigLayerStack` from scratch must call this — a
/// `ConfigLayerStack` built without it silently reflects only local/session config, even
/// if a fetch elsewhere in the process succeeded.
pub async fn apply_server_config_layer(
    codex_home: &Path,
    stack: ConfigLayerStack,
) -> (ConfigLayerStack, Result<Vec<(String, TomlValue)>, AdAgentConfigError>) {
    let overrides_result = fetch_model_overrides(codex_home).await;
    let stack = match &overrides_result {
        Ok(overrides) if !overrides.is_empty() => {
            let server_config_layer = ConfigLayerEntry::new(
                ConfigLayerSource::ServerConfig,
                build_cli_overrides_layer(overrides),
            );
            stack.with_layer_inserted_by_precedence(server_config_layer)
        }
        _ => stack,
    };
    (stack, overrides_result)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::fetch_model_overrides_with_auth_manager;
    use codex_login::AuthManager;
    use codex_login::CodexAuth;
    use codex_login::test_support::auth_manager_from_optional_auth;
    use toml::Value as TomlValue;

    #[tokio::test]
    async fn returns_not_logged_in_when_no_auth_present() {
        let auth_manager: std::sync::Arc<AuthManager> = auth_manager_from_optional_auth(None);

        let err = fetch_model_overrides_with_auth_manager(&auth_manager, "http://unused.invalid")
            .await
            .expect_err("should fail without auth");

        assert!(matches!(err, super::AdAgentConfigError::NotLoggedIn));
    }

    #[tokio::test]
    async fn fetches_and_flattens_overrides_using_the_cached_auth_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/agent/config"))
            .and(header("authorization", "Bearer cached-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string("model = \"qwen3.8-max\"\n"))
            .mount(&server)
            .await;
        let auth_manager = auth_manager_from_optional_auth(Some(CodexAuth::from_api_key(
            "cached-api-key",
        )));

        let overrides = fetch_model_overrides_with_auth_manager(
            &auth_manager,
            &format!("{}/agent/config", server.uri()),
        )
        .await
        .expect("request should succeed");

        assert_eq!(
            overrides,
            vec![(
                "model".to_string(),
                TomlValue::String("qwen3.8-max".to_string())
            )]
        );
    }
}
