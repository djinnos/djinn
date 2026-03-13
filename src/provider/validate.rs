use std::time::Duration;

use serde::Serialize;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Classification of a validation failure for normalized UI messaging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    None,
    MissingKey,
    BadRequest,
    InvalidKey,
    RateLimit,
    ServerError,
    Timeout,
    Unreachable,
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ErrorKind::None => "",
            ErrorKind::MissingKey => "missing_key",
            ErrorKind::BadRequest => "bad_request",
            ErrorKind::InvalidKey => "invalid_key",
            ErrorKind::RateLimit => "rate_limit",
            ErrorKind::ServerError => "server_error",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Unreachable => "unreachable",
        };
        f.write_str(s)
    }
}

pub struct ValidationRequest {
    pub base_url: String,
    pub api_key: String,
    /// Optional provider ID for logging/diagnostics only.
    pub provider_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ValidationResult {
    pub ok: bool,
    pub error_kind: ErrorKind,
    pub error: String,
    pub models: Vec<String>,
    pub http_status: u16,
}

/// Probe a provider's `GET /models` endpoint with the supplied API key.
/// Returns a normalised `ValidationResult` suitable for direct MCP tool consumption.
///
/// The probe targets OpenAI-compatible endpoints (the majority of providers in
/// the models.dev catalog).  A single attempt is made; UI-level retry is the
/// desktop's responsibility.
pub async fn validate(req: ValidationRequest) -> ValidationResult {
    if req.api_key.is_empty() {
        return ValidationResult {
            ok: false,
            error_kind: ErrorKind::MissingKey,
            error: "API key is required".into(),
            models: vec![],
            http_status: 0,
        };
    }

    if req.base_url.is_empty() {
        return ValidationResult {
            ok: false,
            error_kind: ErrorKind::BadRequest,
            error: "base URL is required".into(),
            models: vec![],
            http_status: 0,
        };
    }

    let url = format!("{}/models", req.base_url.trim_end_matches('/'));
    tracing::debug!(
        provider_id = req.provider_id.as_deref().unwrap_or("unknown"),
        url = %url,
        "validating provider credentials"
    );

    let client = match reqwest::Client::builder().timeout(DEFAULT_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return ValidationResult {
                ok: false,
                error_kind: ErrorKind::BadRequest,
                error: format!("failed to build HTTP client: {e}"),
                models: vec![],
                http_status: 0,
            };
        }
    };

    let is_anthropic = req
        .provider_id
        .as_deref()
        .map(|id| id == "anthropic")
        .unwrap_or(false)
        || req.base_url.contains("anthropic.com");

    let mut request = client.get(&url).header("Accept", "application/json");
    if is_anthropic {
        request = request
            .header("x-api-key", &req.api_key)
            .header("anthropic-version", "2023-06-01");
    } else {
        request = request.header("Authorization", format!("Bearer {}", req.api_key));
    }

    let resp = match request.send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return ValidationResult {
                ok: false,
                error_kind: ErrorKind::Timeout,
                error: "connection timed out".into(),
                models: vec![],
                http_status: 0,
            };
        }
        Err(e) => {
            return ValidationResult {
                ok: false,
                error_kind: ErrorKind::Unreachable,
                error: format!("could not reach provider: {e}"),
                models: vec![],
                http_status: 0,
            };
        }
    };

    let status = resp.status().as_u16();

    match status {
        401 | 403 => ValidationResult {
            ok: false,
            error_kind: ErrorKind::InvalidKey,
            error: "API key was rejected by the provider (check key and permissions)".into(),
            models: vec![],
            http_status: status,
        },
        429 => ValidationResult {
            ok: true,
            error_kind: ErrorKind::RateLimit,
            error: "provider is rate-limiting (key accepted)".into(),
            models: vec![],
            http_status: status,
        },
        s if s >= 500 => ValidationResult {
            ok: false,
            error_kind: ErrorKind::ServerError,
            error: format!("provider returned server error ({s})"),
            models: vec![],
            http_status: status,
        },
        200 => {
            let models = parse_models(resp).await;
            ValidationResult {
                ok: true,
                error_kind: ErrorKind::None,
                error: String::new(),
                models,
                http_status: status,
            }
        }
        s => ValidationResult {
            ok: false,
            error_kind: ErrorKind::ServerError,
            error: format!("unexpected HTTP status {s} from provider"),
            models: vec![],
            http_status: status,
        },
    }
}

#[derive(serde::Deserialize)]
struct ModelsListResponse {
    data: Vec<ModelEntry>,
}

#[derive(serde::Deserialize)]
struct ModelEntry {
    id: String,
}

async fn parse_models(resp: reqwest::Response) -> Vec<String> {
    match resp.json::<ModelsListResponse>().await {
        Ok(parsed) => parsed
            .data
            .into_iter()
            .filter(|m| !m.id.is_empty())
            .map(|m| m.id)
            .collect(),
        Err(_) => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, extract::Request, routing::get};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug, Default)]
    struct SeenHeaders {
        authorization: Option<String>,
        x_api_key: Option<String>,
        anthropic_version: Option<String>,
    }

    fn spawn_server(
        status: u16,
        body: &'static str,
        seen: Arc<Mutex<Option<SeenHeaders>>>,
    ) -> String {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind local tcp listener");
        let addr = listener.local_addr().expect("local addr");
        listener.set_nonblocking(true).expect("set nonblocking");

        let app = Router::new().route(
            "/models",
            get(move |req: Request| async move {
                let headers = req.headers();
                *seen.lock().expect("lock seen headers") = Some(SeenHeaders {
                    authorization: headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from),
                    x_api_key: headers
                        .get("x-api-key")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from),
                    anthropic_version: headers
                        .get("anthropic-version")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from),
                });
                (
                    axum::http::StatusCode::from_u16(status).expect("status"),
                    body,
                )
            }),
        );

        let tokio_listener =
            tokio::net::TcpListener::from_std(listener).expect("convert to tokio listener");
        tokio::spawn(async move {
            axum::serve(tokio_listener, app).await.ok();
        });

        format!("http://{}:{}", addr.ip(), addr.port())
    }

    #[tokio::test]
    async fn rate_limit_429_returns_ok_true_and_rate_limit_kind() {
        let seen = Arc::new(Mutex::new(None));
        let base_url = spawn_server(429, "too many requests", seen);

        let result = validate(ValidationRequest {
            base_url,
            api_key: "k123".into(),
            provider_id: Some("openai".into()),
        })
        .await;

        assert!(result.ok);
        assert_eq!(result.error_kind, ErrorKind::RateLimit);
        assert_eq!(result.http_status, 429);
        assert!(result.models.is_empty());
    }

    #[tokio::test]
    async fn anthropic_provider_id_uses_anthropic_headers() {
        let seen = Arc::new(Mutex::new(None));
        let base_url = spawn_server(200, r#"{"data":[]}"#, seen.clone());

        let _ = validate(ValidationRequest {
            base_url,
            api_key: "anthropic-key".into(),
            provider_id: Some("anthropic".into()),
        })
        .await;

        let headers = seen
            .lock()
            .expect("seen lock")
            .clone()
            .expect("captured headers");
        assert_eq!(headers.x_api_key.as_deref(), Some("anthropic-key"));
        assert_eq!(headers.anthropic_version.as_deref(), Some("2023-06-01"));
        assert!(headers.authorization.is_none());
    }

    #[tokio::test]
    async fn malformed_json_on_200_keeps_ok_true_with_empty_models() {
        let seen = Arc::new(Mutex::new(None));
        let base_url = spawn_server(200, "{not-json", seen);

        let result = validate(ValidationRequest {
            base_url,
            api_key: "k123".into(),
            provider_id: Some("openai".into()),
        })
        .await;

        assert!(result.ok);
        assert_eq!(result.error_kind, ErrorKind::None);
        assert!(result.models.is_empty());
        assert_eq!(result.http_status, 200);
    }

    #[tokio::test]
    async fn bare_model_names_are_returned_as_is() {
        let seen = Arc::new(Mutex::new(None));
        let base_url = spawn_server(
            200,
            r#"{"data":[{"id":"gpt-4o"},{"id":"anthropic/claude-opus-4-6"},{"id":""}]}"#,
            seen,
        );

        let result = validate(ValidationRequest {
            base_url,
            api_key: "k123".into(),
            provider_id: Some("openai".into()),
        })
        .await;

        assert_eq!(
            result.models,
            vec![
                "gpt-4o".to_string(),
                "anthropic/claude-opus-4-6".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn trailing_slash_base_url_forms_single_models_path() {
        let seen = Arc::new(Mutex::new(None));
        let mut base_url = spawn_server(200, r#"{"data":[]}"#, seen);
        base_url.push('/');

        let result = validate(ValidationRequest {
            base_url,
            api_key: "k123".into(),
            provider_id: Some("openai".into()),
        })
        .await;

        assert!(result.ok);
        assert_eq!(result.http_status, 200);
        assert!(result.models.is_empty());
    }
}
