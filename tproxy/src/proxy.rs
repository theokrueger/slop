use crate::{config::Config, usage::Usage};
use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

const MAX_ERROR_BODY_BYTES: usize = 1 << 20;
const MAX_RATE_KEYS: usize = 100_000;
const DASHBOARD: &str = include_str!("dashboard.html");

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub usage: Usage,
    client: reqwest::Client,
    keys: Arc<Vec<(String, [u8; 32])>>,
    models: Bytes,
    secrets: Arc<Vec<Vec<u8>>>,
    user_limiter: Arc<RateLimiter>,
    ip_limiter: Arc<RateLimiter>,
    ip_total_limiter: Arc<RateLimiter>,
    concurrency: Arc<Semaphore>,
}

impl AppState {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        Self::with_usage(config, Usage::in_memory())
    }

    pub fn with_usage(config: Config, usage: Usage) -> anyhow::Result<Self> {
        config.validate()?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(
                config.server.upstream_read_timeout_secs,
            ))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("tproxy/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let keys = config
            .users
            .iter()
            .map(|(name, key)| (name.clone(), digest(key)))
            .collect();

        let data: Vec<_> = config
            .models
            .iter()
            .map(|(alias, model)| {
                json!({
                    "id": alias,
                    "object": "model",
                    "created": 0,
                    "owned_by": "tproxy",
                    "name": model.display_name(alias),
                    "reasoning": model.reasoning,
                    "input": &model.input,
                    "cost": {
                        "input": model.cost.input,
                        "output": model.cost.output,
                        "cacheRead": model.cost.cache_read,
                        "cacheWrite": model.cost.cache_write,
                    },
                    "contextWindow": model.context_window,
                    "maxTokens": model.max_tokens,
                })
            })
            .collect();
        let models = Bytes::from(serde_json::to_vec(
            &json!({"object": "list", "data": data}),
        )?);

        let mut secrets: Vec<Vec<u8>> = config
            .models
            .values()
            .filter_map(|model| model.api_key.as_deref())
            .filter(|key| !key.is_empty())
            .map(|key| key.as_bytes().to_vec())
            .collect();
        // Deduplicate identical keys, then redact the longest first so a
        // shorter key cannot partially mask a longer one.
        secrets.sort_unstable();
        secrets.dedup();
        secrets.sort_by_key(|key| std::cmp::Reverse(key.len()));
        let rate_limit = config.server.rate_limit;
        let ip_rate_limit = config.server.ip_rate_limit;
        let max_concurrency = config.server.max_concurrency;

        Ok(Self {
            config: Arc::new(config),
            usage,
            client,
            keys: Arc::new(keys),
            models,
            secrets: Arc::new(secrets),
            user_limiter: Arc::new(RateLimiter::new(rate_limit)),
            ip_limiter: Arc::new(RateLimiter::new(rate_limit)),
            ip_total_limiter: Arc::new(RateLimiter::new(ip_rate_limit)),
            concurrency: Arc::new(Semaphore::new(max_concurrency)),
        })
    }

    fn authorize(&self, headers: &HeaderMap, ip: IpAddr) -> Result<&str, ApiError> {
        let ip = ip.to_string();
        self.ip_total_limiter
            .check(&ip)
            .map_err(|limit| ApiError::rate_limited(limit.retry_after))?;
        match self.authenticate(headers) {
            Some(user) => {
                self.user_limiter
                    .check(user)
                    .map_err(|limit| ApiError::rate_limited(limit.retry_after))?;
                Ok(user)
            }
            None => {
                self.ip_limiter
                    .check(&ip)
                    .map_err(|limit| ApiError::rate_limited(limit.retry_after))?;
                Err(ApiError::unauthorized())
            }
        }
    }

    fn authenticate(&self, headers: &HeaderMap) -> Option<&str> {
        let token = bearer(headers)?;
        let token = digest(token);
        self.keys
            .iter()
            .find(|(_, key)| bool::from(key.as_slice().ct_eq(token.as_slice())))
            .map(|(user, _)| user.as_str())
    }
}

struct RateLimiter {
    capacity: f64,
    refill_per_sec: f64,
    window: Duration,
    state: Mutex<RateState>,
}

struct RateState {
    buckets: HashMap<String, Bucket>,
    last_cleanup: Instant,
}

struct Bucket {
    tokens: f64,
    updated: Instant,
}

struct RateLimit {
    retry_after: u64,
}

impl RateLimiter {
    fn new(limit: u32) -> Self {
        let window = Duration::from_secs(60);
        Self {
            capacity: f64::from(limit),
            refill_per_sec: f64::from(limit) / window.as_secs_f64(),
            window,
            state: Mutex::new(RateState {
                buckets: HashMap::new(),
                last_cleanup: Instant::now(),
            }),
        }
    }

    fn check(&self, key: &str) -> Result<(), RateLimit> {
        self.check_at(key, Instant::now())
    }

    fn check_at(&self, key: &str, now: Instant) -> Result<(), RateLimit> {
        if self.capacity == 0.0 {
            return Ok(());
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if now.saturating_duration_since(state.last_cleanup) >= self.window {
            state.buckets.retain(|_, bucket| {
                let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
                bucket.tokens + elapsed * self.refill_per_sec < self.capacity
            });
            state.last_cleanup = now;
        }
        if state.buckets.len() >= MAX_RATE_KEYS && !state.buckets.contains_key(key) {
            return Err(RateLimit { retry_after: 1 });
        }
        let bucket = state.buckets.entry(key.into()).or_insert(Bucket {
            tokens: self.capacity,
            updated: now,
        });
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.updated = now;
        if bucket.tokens < 1.0 {
            let retry_after = ((1.0 - bucket.tokens) / self.refill_per_sec)
                .ceil()
                .max(1.0) as u64;
            return Err(RateLimit { retry_after });
        }
        bucket.tokens -= 1.0;
        Ok(())
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/usage.json", get(usage_json))
        .route("/health", get(|| async { "ok" }))
        .route("/v1/models", get(models))
        .route("/v1/{*path}", post(forward))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    response
}

async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD)
}

async fn usage_json(State(state): State<AppState>) -> Result<Response, ApiError> {
    let body = serde_json::to_vec(&state.usage.snapshot_now())
        .map_err(|e| ApiError::internal(format!("serialize usage: {e}")))?;
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(response)
}

async fn models(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.authorize(&headers, addr.ip())?;
    let mut response = Response::new(Body::from(state.models.clone()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(response)
}

async fn forward(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
) -> Result<Response, ApiError> {
    let user = state.authorize(request.headers(), addr.ip())?;
    let uri = request.uri().clone();
    let method = request.method().clone();
    let headers = request.headers().clone();
    if !state
        .config
        .server
        .allowed_paths
        .iter()
        .any(|path| path == uri.path())
        || !safe_upstream_path(uri.path())
    {
        return Err(ApiError::not_found("endpoint not allowed"));
    }
    let permit = state
        .concurrency
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::overloaded())?;
    let body = tokio::time::timeout(
        Duration::from_secs(state.config.server.body_timeout_secs),
        to_bytes(request.into_body(), state.config.server.max_body_bytes),
    )
    .await
    .map_err(|_| ApiError::request_timeout())?
    .map_err(|e| ApiError::bad_request(format!("read request body: {e}")))?;

    let mut payload: Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))?;
    let alias = payload
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("missing model field"))?
        .to_string();
    let model = state
        .config
        .models
        .get(&alias)
        .ok_or_else(|| ApiError::not_found(format!("unknown model: {alias}")))?;
    payload["model"] = Value::String(model.upstream_name(&alias).into());
    let body = serde_json::to_vec(&payload)
        .map_err(|e| ApiError::internal(format!("serialize request: {e}")))?;

    let url = upstream_url(&model.endpoint, uri.path(), uri.query());
    let mut builder = state
        .client
        .request(method, url)
        .headers(request_headers(&headers))
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )
        .body(body);
    if let Some(key) = &model.api_key {
        builder = builder.bearer_auth(key);
    }

    let mut upstream = builder.send().await.map_err(|e| {
        tracing::warn!(user, model = %alias, error = %e.without_url(), "upstream request failed");
        ApiError::bad_gateway("upstream request failed")
    })?;
    state.usage.record(user, &alias);
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    tracing::info!(
        user,
        model = %alias,
        status = status.as_u16(),
        "proxied"
    );

    if status.is_redirection() {
        tracing::warn!(user, model = %alias, "refusing upstream redirect");
        return Err(ApiError::bad_gateway("upstream redirect refused"));
    }

    if !status.is_success() {
        if upstream_headers
            .get(header::CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| !value.is_empty() && !value.eq_ignore_ascii_case("identity"))
        {
            tracing::warn!(user, model = %alias, "refusing encoded upstream error body");
            return Err(ApiError::bad_gateway("upstream request failed"));
        }
        let body = read_error_body(&mut upstream).await?;
        let body = redact_bytes(&body, &state.secrets);
        let response = copy_response_headers(
            Response::builder().status(status),
            &upstream_headers,
            &state.secrets,
            true,
        )
        .header(header::CONTENT_LENGTH, body.len().to_string());
        return response
            .body(Body::from(body))
            .map_err(|e| ApiError::internal(format!("build response: {e}")));
    }

    let response = copy_response_headers(
        Response::builder().status(status),
        &upstream_headers,
        &state.secrets,
        false,
    );
    let stream = futures_util::stream::unfold(
        (upstream.bytes_stream(), permit),
        |(mut stream, permit)| async move { stream.next().await.map(|item| (item, (stream, permit))) },
    );
    response
        .body(Body::from_stream(stream))
        .map_err(|e| ApiError::internal(format!("build response: {e}")))
}

fn safe_upstream_path(path: &str) -> bool {
    let suffix = path.strip_prefix("/v1").unwrap_or(path);
    suffix.split('/').all(|segment| {
        !segment.contains("..")
            && segment != "."
            && !segment.contains('\\')
            && !segment.to_ascii_lowercase().contains("%2e")
    })
}

fn upstream_url(endpoint: &str, path: &str, query: Option<&str>) -> String {
    let suffix = path.strip_prefix("/v1").unwrap_or(path);
    let suffix = suffix.trim_start_matches('/');
    let endpoint = endpoint.trim_end_matches('/');
    let mut url = if suffix.is_empty() {
        endpoint.to_string()
    } else {
        format!("{endpoint}/{suffix}")
    };
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    url
}

fn request_headers(headers: &HeaderMap) -> HeaderMap {
    const FORWARD: [&str; 3] = ["accept", "idempotency-key", "openai-beta"];
    headers
        .iter()
        .filter(|(name, _)| FORWARD.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn copy_response_headers(
    mut builder: axum::http::response::Builder,
    headers: &HeaderMap,
    secrets: &[Vec<u8>],
    drop_content_length: bool,
) -> axum::http::response::Builder {
    for (name, value) in headers {
        if is_hop_by_hop(name.as_str())
            || is_sensitive_header(name.as_str())
            || (drop_content_length && name.as_str() == "content-length")
        {
            continue;
        }
        builder = builder.header(name, redact_header(value, secrets));
    }
    builder
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name,
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "cookie2"
            | "set-cookie"
            | "api-key"
            | "apikey"
            | "x-api-key"
            | "x-api-token"
            | "x-auth-key"
            | "x-auth-token"
            | "x-access-token"
            | "x-goog-api-key"
    )
}

async fn read_error_body(response: &mut reqwest::Response) -> Result<Vec<u8>, ApiError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        tracing::warn!(error = %e.without_url(), "read upstream error body failed");
        ApiError::bad_gateway("upstream request failed")
    })? {
        if body.len() + chunk.len() > MAX_ERROR_BODY_BYTES {
            return Err(ApiError::bad_gateway("upstream error response too large"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn redact_header(value: &HeaderValue, secrets: &[Vec<u8>]) -> HeaderValue {
    let bytes = redact_bytes(value.as_bytes(), secrets);
    HeaderValue::from_bytes(&bytes).unwrap_or_else(|_| HeaderValue::from_static("[redacted]"))
}

fn redact_bytes(bytes: &[u8], secrets: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = bytes.to_vec();
    for secret in secrets {
        bytes = replace_bytes(&bytes, secret, b"[redacted]");
    }
    bytes
}

fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut start = 0;
    while let Some(pos) = haystack[start..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let pos = start + pos;
        out.extend_from_slice(&haystack[start..pos]);
        out.extend_from_slice(replacement);
        start = pos + needle.len();
    }
    out.extend_from_slice(&haystack[start..]);
    out
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let mut parts = value.split_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    (scheme.eq_ignore_ascii_case("bearer") && parts.next().is_none()).then_some(token)
}

fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

struct ApiError {
    status: StatusCode,
    message: String,
    retry_after: Option<u64>,
}

impl ApiError {
    fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "invalid API key")
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    fn request_timeout() -> Self {
        Self::new(StatusCode::REQUEST_TIMEOUT, "request body timeout")
    }

    fn rate_limited(retry_after: u64) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "rate limit exceeded".into(),
            retry_after: Some(retry_after),
        }
    }

    fn overloaded() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "server busy".into(),
            retry_after: Some(1),
        }
    }

    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({"error": {"message": self.message, "type": "tproxy_error"}});
        let mut response = (self.status, Json(body)).into_response();
        if let Some(retry_after) = self.retry_after
            && let Ok(value) = HeaderValue::from_str(&retry_after.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_upstream_urls() {
        assert_eq!(
            upstream_url("https://api.example.com/v1/", "/v1/chat/completions", None),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            upstream_url("https://api.example.com/v1", "/v1/models/a", Some("x=1")),
            "https://api.example.com/v1/models/a?x=1"
        );
    }

    #[test]
    fn rejects_unsafe_paths() {
        assert!(safe_upstream_path("/v1/chat/completions"));
        assert!(safe_upstream_path("/v1/models/a"));
        assert!(!safe_upstream_path("/v1/../admin"));
        assert!(!safe_upstream_path("/v1/%2e%2e/admin"));
        assert!(!safe_upstream_path("/v1/a\\b"));
    }

    #[test]
    fn redacts_secrets() {
        let secrets = vec![b"sk-secret".to_vec()];
        assert_eq!(
            redact_bytes(b"token=sk-secret&x=1", &secrets),
            b"token=[redacted]&x=1"
        );
        assert_eq!(redact_bytes(b"nothing here", &secrets), b"nothing here");
    }

    #[test]
    fn rate_limits_per_user_and_resets() {
        let limiter = RateLimiter::new(2);
        let now = Instant::now();
        assert!(limiter.check_at("a", now).is_ok());
        assert!(limiter.check_at("a", now).is_ok());
        let limited = limiter.check_at("a", now).unwrap_err();
        assert_eq!(limited.retry_after, 30);
        assert!(limiter.check_at("b", now).is_ok());
        assert!(limiter.check_at("a", now + Duration::from_secs(30)).is_ok());
        assert!(
            limiter
                .check_at("a", now + Duration::from_secs(30))
                .is_err()
        );
        assert!(limiter.check_at("a", now + Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn ip_total_limit_precedes_auth() {
        let mut config = Config::default();
        config.server.ip_rate_limit = 2;
        config.users.insert("me".into(), "sk-test".into());
        let state = AppState::new(config).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer sk-test".parse().unwrap());
        let ip: IpAddr = "127.0.0.1".parse().unwrap();

        assert!(state.authorize(&headers, ip).is_ok());
        assert!(state.authorize(&headers, ip).is_ok());
        let limited = state.authorize(&headers, ip).unwrap_err();
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn parses_bearer_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer sk-test".parse().unwrap());
        assert_eq!(bearer(&headers), Some("sk-test"));
        headers.insert(header::AUTHORIZATION, "Basic sk-test".parse().unwrap());
        assert_eq!(bearer(&headers), None);
    }
}
