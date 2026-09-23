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

/// Fetches the `/agent/config` TOML document and flattens it into `cli_overrides` entries.
pub(crate) async fn fetch_model_overrides_from(
    url: &str,
    access_token: &str,
    http_client: &reqwest::Client,
) -> Result<Vec<(String, TomlValue)>, AdAgentConfigError> {
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
    let document: TomlValue =
        toml::from_str(&body).map_err(|err| AdAgentConfigError::Unavailable(err.to_string()))?;

    Ok(flatten_toml_table(&document))
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
            vec![(
                "model".to_string(),
                TomlValue::String("qwen3.8-max".to_string())
            )]
        );
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
