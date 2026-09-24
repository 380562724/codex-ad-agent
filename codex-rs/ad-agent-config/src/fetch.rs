use toml::Value as TomlValue;

use crate::flatten::flatten_toml_table;

/// Errors from fetching the server-driven model config. Neither variant is retried:
/// the caller (`build_inner`) fails the whole config load, matching the product
/// requirement that model config must come from the server or not at all.
#[derive(Debug, thiserror::Error)]
pub enum AdAgentConfigError {
    #[error("not logged in; run `codex login`")]
    NotLoggedIn,
    #[error("failed to fetch model config from server: {0}")]
    Unavailable(String),
}

impl From<AdAgentConfigError> for std::io::Error {
    fn from(err: AdAgentConfigError) -> Self {
        std::io::Error::other(err.to_string())
    }
}

/// Name of the top-level table in the server config whose keys are defaults rather than
/// enforced values (see [`ServerConfigOverrides`]).
const DEFAULTS_TABLE: &str = "defaults";

/// The server config, flattened into `cli_overrides`-style dotted entries and split by how
/// strongly each entry applies.
#[derive(Debug, Default, PartialEq)]
pub struct ServerConfigOverrides {
    /// Everything outside `[defaults]`. Enforced: outranks every local config layer.
    pub enforced: Vec<(String, TomlValue)>,
    /// Keys under `[defaults]`, with the table prefix stripped. Used only when the user
    /// hasn't set the key locally (e.g. which model to use until the user picks one).
    pub defaults: Vec<(String, TomlValue)>,
}

/// Fetches the `/agent/config` TOML document and flattens it into `cli_overrides` entries.
pub(crate) async fn fetch_model_overrides_from(
    url: &str,
    access_token: &str,
    http_client: &reqwest::Client,
) -> Result<ServerConfigOverrides, AdAgentConfigError> {
    let response = http_client
        .get(url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|err| AdAgentConfigError::Unavailable(err.to_string()))?;

    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(AdAgentConfigError::NotLoggedIn);
    }
    let response = response
        .error_for_status()
        .map_err(|err| AdAgentConfigError::Unavailable(err.to_string()))?;

    let body = response
        .text()
        .await
        .map_err(|err| AdAgentConfigError::Unavailable(err.to_string()))?;
    let document: toml::Table =
        toml::from_str(&body).map_err(|err| AdAgentConfigError::Unavailable(err.to_string()))?;

    split_server_config(document)
}

fn split_server_config(
    mut document: toml::Table,
) -> Result<ServerConfigOverrides, AdAgentConfigError> {
    let defaults = match document.remove(DEFAULTS_TABLE) {
        None => Vec::new(),
        Some(defaults @ TomlValue::Table(_)) => flatten_toml_table(&defaults),
        Some(_) => {
            return Err(AdAgentConfigError::Unavailable(format!(
                "`{DEFAULTS_TABLE}` in server config must be a table"
            )));
        }
    };
    Ok(ServerConfigOverrides {
        enforced: flatten_toml_table(&TomlValue::Table(document)),
        defaults,
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use toml::Value as TomlValue;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::AdAgentConfigError;
    use super::ServerConfigOverrides;
    use super::fetch_model_overrides_from;

    #[tokio::test]
    async fn parses_toml_response_into_dotted_overrides() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/agent/config"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("model = \"qwen3.8-max\"\n")
                    .insert_header("content-type", "application/toml"),
            )
            .mount(&server)
            .await;

        let overrides = fetch_model_overrides_from(
            &format!("{}/agent/config", server.uri()),
            "test-token",
            &reqwest::Client::new(),
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

    #[tokio::test]
    async fn splits_defaults_table_from_enforced_entries() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/agent/config"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"
                model_provider = "cowork"

                [model_providers.cowork]
                base_url = "https://example.invalid/llm/v1"

                [defaults]
                model = "qwen3.8-max"
                model_reasoning_effort = "high"
                "#,
            ))
            .mount(&server)
            .await;

        let mut overrides = fetch_model_overrides_from(
            &format!("{}/agent/config", server.uri()),
            "test-token",
            &reqwest::Client::new(),
        )
        .await
        .expect("request should succeed");
        overrides.enforced.sort_by(|a, b| a.0.cmp(&b.0));
        overrides.defaults.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(
            overrides,
            ServerConfigOverrides {
                enforced: vec![
                    (
                        "model_provider".to_string(),
                        TomlValue::String("cowork".to_string())
                    ),
                    (
                        "model_providers.cowork.base_url".to_string(),
                        TomlValue::String("https://example.invalid/llm/v1".to_string())
                    ),
                ],
                defaults: vec![
                    (
                        "model".to_string(),
                        TomlValue::String("qwen3.8-max".to_string())
                    ),
                    (
                        "model_reasoning_effort".to_string(),
                        TomlValue::String("high".to_string())
                    ),
                ],
            }
        );
    }

    #[tokio::test]
    async fn rejects_non_table_defaults() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/agent/config"))
            .respond_with(ResponseTemplate::new(200).set_body_string("defaults = \"qwen3.8-max\"\n"))
            .mount(&server)
            .await;

        let err = fetch_model_overrides_from(
            &format!("{}/agent/config", server.uri()),
            "test-token",
            &reqwest::Client::new(),
        )
        .await
        .expect_err("non-table defaults should be an error");

        assert!(matches!(err, AdAgentConfigError::Unavailable(_)));
    }

    #[tokio::test]
    async fn maps_401_to_not_logged_in() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/agent/config"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let err = fetch_model_overrides_from(
            &format!("{}/agent/config", server.uri()),
            "expired-token",
            &reqwest::Client::new(),
        )
        .await
        .expect_err("401 should be an error");

        assert!(matches!(err, AdAgentConfigError::NotLoggedIn));
    }

    #[tokio::test]
    async fn maps_5xx_to_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/agent/config"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let err = fetch_model_overrides_from(
            &format!("{}/agent/config", server.uri()),
            "test-token",
            &reqwest::Client::new(),
        )
        .await
        .expect_err("503 should be an error");

        assert!(matches!(err, AdAgentConfigError::Unavailable(_)));
    }

    #[test]
    fn converts_into_an_io_error_for_build_inner_propagation() {
        let io_err: std::io::Error = AdAgentConfigError::NotLoggedIn.into();
        assert!(io_err.to_string().contains("not logged in"));
    }

    #[tokio::test]
    async fn maps_malformed_toml_body_to_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/agent/config"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not = [valid toml"))
            .mount(&server)
            .await;

        let err = fetch_model_overrides_from(
            &format!("{}/agent/config", server.uri()),
            "test-token",
            &reqwest::Client::new(),
        )
        .await
        .expect_err("malformed TOML should be an error");

        assert!(matches!(err, AdAgentConfigError::Unavailable(_)));
    }
}
