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

    let resp = match request.send().await
    {
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
