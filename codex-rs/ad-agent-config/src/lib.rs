// [ad-agent] Fetches model configuration from the server so it can be inserted as dedicated
// `ConfigLayerSource::ServerConfig` (enforced) and `ConfigLayerSource::ServerDefaults` layers
// (see `apply_server_config_layer` and core/src/config/mod.rs `build_inner`). Every caller that builds a `ConfigLayerStack` from
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

pub use fetch::AdAgentConfigError;
pub use fetch::ServerConfigOverrides;

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
) -> Result<ServerConfigOverrides, AdAgentConfigError> {
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
) -> Result<ServerConfigOverrides, AdAgentConfigError> {
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

/// Fetches server-driven model config and returns `stack` with up to two layers inserted:
/// `ConfigLayerSource::ServerConfig` for the enforced entries (outranking every local layer,
/// including `SessionFlags`) and `ConfigLayerSource::ServerDefaults` for the `[defaults]`
/// entries (ranking just below the user's own `config.toml`, so a choice the user made
/// survives restarts). Empty parts add no layer. On fetch failure, returns `stack` unchanged;
/// the fetch `Result` is returned alongside so the caller can still surface a fetch error
/// where that caller is expected to (e.g. `Config::ad_agent_config_status`).
///
/// Every place that assembles a `ConfigLayerStack` from scratch must call this — a
/// `ConfigLayerStack` built without it silently reflects only local/session config, even
/// if a fetch elsewhere in the process succeeded.
pub async fn apply_server_config_layer(
    codex_home: &Path,
    stack: ConfigLayerStack,
) -> (ConfigLayerStack, Result<ServerConfigOverrides, AdAgentConfigError>) {
    let overrides_result = fetch_model_overrides(codex_home).await;
    let stack = match &overrides_result {
        Ok(overrides) => with_server_config_layers(stack, overrides),
        Err(_) => stack,
    };
    (stack, overrides_result)
}

fn with_server_config_layers(
    stack: ConfigLayerStack,
    overrides: &ServerConfigOverrides,
) -> ConfigLayerStack {
    [
        (ConfigLayerSource::ServerDefaults, &overrides.defaults),
        (ConfigLayerSource::ServerConfig, &overrides.enforced),
    ]
    .into_iter()
    .filter(|(_, entries)| !entries.is_empty())
    .fold(stack, |stack, (source, entries)| {
        stack.with_layer_inserted_by_precedence(ConfigLayerEntry::new(
            source,
            build_cli_overrides_layer(entries),
        ))
    })
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

    use super::ServerConfigOverrides;
    use super::fetch_model_overrides_with_auth_manager;
    use super::with_server_config_layers;
    use codex_config::ConfigLayerEntry;
    use codex_config::ConfigLayerSource;
    use codex_config::ConfigLayerStack;
    use codex_utils_absolute_path::AbsolutePathBuf;
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
            ServerConfigOverrides {
                enforced: vec![(
                    "model".to_string(),
                    TomlValue::String("qwen3.8-max".to_string())
                )],
                defaults: Vec::new(),
            }
        );
    }

    #[test]
    fn user_choice_beats_server_defaults_but_not_enforced_entries() {
        let codex_home = tempfile::tempdir().expect("create codex home");
        let user_file = AbsolutePathBuf::from_absolute_path(codex_home.path().join("config.toml"))
            .expect("absolute user config path");
        let user_config: TomlValue = toml::from_str(
            r#"
            model = "user-picked-model"
            model_provider = "local-provider"
            "#,
        )
        .expect("user config should parse");
        let stack = ConfigLayerStack::default().with_layer_inserted_by_precedence(
            ConfigLayerEntry::new(
                ConfigLayerSource::User {
                    file: user_file,
                    profile: None,
                },
                user_config,
            ),
        );
        let overrides = ServerConfigOverrides {
            enforced: vec![(
                "model_provider".to_string(),
                TomlValue::String("cowork".to_string()),
            )],
            defaults: vec![
                (
                    "model".to_string(),
                    TomlValue::String("qwen3.8-max".to_string()),
                ),
                (
                    "model_reasoning_effort".to_string(),
                    TomlValue::String("high".to_string()),
                ),
            ],
        };

        let effective = with_server_config_layers(stack, &overrides).effective_config();

        assert_eq!(
            (
                effective.get("model"),
                effective.get("model_provider"),
                effective.get("model_reasoning_effort"),
            ),
            (
                Some(&TomlValue::String("user-picked-model".to_string())),
                Some(&TomlValue::String("cowork".to_string())),
                Some(&TomlValue::String("high".to_string())),
            )
        );
    }
}
