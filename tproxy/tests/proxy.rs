use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tproxy::{
    config::{Config, Model},
    proxy::{self, AppState},
};

#[derive(Clone)]
struct Capture(mpsc::UnboundedSender<(HeaderMap, Value)>);

async fn capture(State(capture): State<Capture>, headers: HeaderMap, body: Bytes) -> Response {
    let payload = serde_json::from_slice(&body).unwrap();
    capture.0.send((headers, payload)).unwrap();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream")],
        "data: {\"ok\":true}\n\n",
    )
        .into_response()
}

async fn leaky(headers: HeaderMap) -> Response {
    let key = headers[header::AUTHORIZATION].to_str().unwrap().to_string();
    let mut response = Response::new(Body::from(format!("{{\"error\":\"incorrect key {key}\"}}")));
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    response
        .headers_mut()
        .insert("x-api-key", key.parse().unwrap());
    response
        .headers_mut()
        .insert(header::AUTHORIZATION, key.parse().unwrap());
    response
}

async fn spawn(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    addr
}

async fn setup() -> (SocketAddr, mpsc::UnboundedReceiver<(HeaderMap, Value)>) {
    setup_with(|_| {}).await
}

async fn setup_with(
    configure: impl FnOnce(&mut Config),
) -> (SocketAddr, mpsc::UnboundedReceiver<(HeaderMap, Value)>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let upstream = Router::new()
        .route("/v1/chat/completions", post(capture))
        .route("/v1/error", post(leaky))
        .with_state(Capture(tx));
    let upstream_addr = spawn(upstream).await;

    let mut config = Config::default();
    config.models.insert(
        "alias".into(),
        Model {
            endpoint: format!("http://{upstream_addr}/v1"),
            modelname: Some("real-model".into()),
            api_key: Some("upstream-key".into()),
            name: Some("Alias Model".into()),
            context_window: 64_000,
            max_tokens: 8_000,
            ..Default::default()
        },
    );
    config.users.insert("me".into(), "sk-test".into());
    config.server.allowed_paths.push("/v1/error".into());
    configure(&mut config);
    let proxy_addr = spawn(proxy::router(AppState::new(config).unwrap())).await;
    (proxy_addr, rx)
}

#[tokio::test]
async fn forwards_request_and_streams_response() {
    let (proxy_addr, mut captured) = setup().await;
    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/chat/completions"))
        .bearer_auth("sk-test")
        .header("x-test", "yes")
        .header("x-forwarded-for", "203.0.113.1")
        .header("openai-beta", "test-beta")
        .json(&json!({"model": "alias", "messages": []}))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.text().await.unwrap(), "data: {\"ok\":true}\n\n");

    let (headers, payload) = captured.recv().await.unwrap();
    assert_eq!(payload["model"], "real-model");
    assert_eq!(headers[header::AUTHORIZATION], "Bearer upstream-key");
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(headers["openai-beta"], "test-beta");
    assert!(headers.get("x-test").is_none());
    assert!(headers.get("x-forwarded-for").is_none());
}

#[tokio::test]
async fn serves_health_without_auth() {
    let (proxy_addr, _captured) = setup().await;
    let response = reqwest::Client::new()
        .get(format!("http://{proxy_addr}/health"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn rejects_disallowed_paths_and_methods() {
    let (proxy_addr, _captured) = setup().await;
    let client = reqwest::Client::new();
    let body = json!({"model": "alias"});

    let path = client
        .post(format!("http://{proxy_addr}/v1/admin"))
        .bearer_auth("sk-test")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(path.status(), 404);

    let method = client
        .get(format!("http://{proxy_addr}/v1/chat/completions"))
        .bearer_auth("sk-test")
        .send()
        .await
        .unwrap();
    assert_eq!(method.status(), 405);
}

#[tokio::test]
async fn rejects_bad_keys_and_unknown_models() {
    let (proxy_addr, _captured) = setup().await;
    let client = reqwest::Client::new();
    let url = format!("http://{proxy_addr}/v1/chat/completions");
    let body = json!({"model": "alias", "messages": []});

    let unauthorized = client
        .post(&url)
        .bearer_auth("wrong")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);

    let unknown = client
        .post(&url)
        .bearer_auth("sk-test")
        .json(&json!({"model": "nope", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
}

#[tokio::test]
async fn redacts_upstream_secrets_from_errors() {
    let (proxy_addr, _captured) = setup().await;
    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/error"))
        .bearer_auth("sk-test")
        .json(&json!({"model": "alias"}))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 401);
    assert!(response.headers().get("x-api-key").is_none());
    assert!(response.headers().get(header::AUTHORIZATION).is_none());
    let body = response.text().await.unwrap();
    assert!(!body.contains("upstream-key"));
    assert!(body.contains("[redacted]"));
}

#[tokio::test]
async fn lists_configured_models() {
    let (proxy_addr, _captured) = setup().await;
    let response = reqwest::Client::new()
        .get(format!("http://{proxy_addr}/v1/models"))
        .bearer_auth("sk-test")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let model = &body["data"][0];
    assert_eq!(model["id"], "alias");
    assert_eq!(model["name"], "Alias Model");
    assert_eq!(model["contextWindow"], 64_000);
    assert_eq!(model["maxTokens"], 8_000);
    assert_eq!(model["reasoning"], false);
    assert_eq!(model["input"][0], "text");
    assert_eq!(model["cost"]["input"], 0.0);
}

#[tokio::test]
async fn records_usage_and_serves_dashboard() {
    let (proxy_addr, _captured) = setup().await;
    let client = reqwest::Client::new();
    let url = format!("http://{proxy_addr}/v1/chat/completions");

    for _ in 0..3 {
        let response = client
            .post(&url)
            .bearer_auth("sk-test")
            .json(&json!({"model": "alias", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "data: {\"ok\":true}\n\n");
    }

    let usage: Value = client
        .get(format!("http://{proxy_addr}/usage.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(usage["retention_days"], 30);
    let recorded = usage["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|bucket| bucket["counts"]["me"]["alias"] == 3);
    assert!(recorded, "usage should record three requests for me/alias");

    let dashboard = client
        .get(format!("http://{proxy_addr}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(dashboard.status(), 200);
    assert!(
        dashboard.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(dashboard.text().await.unwrap().contains("tproxy usage"));
}

#[tokio::test]
async fn rate_limits_bad_auth_by_ip() {
    const LIMIT: u32 = 5;
    let (proxy_addr, _captured) = setup_with(|config| config.server.rate_limit = LIMIT).await;
    let client = reqwest::Client::new();
    let url = format!("http://{proxy_addr}/v1/models");

    for _ in 0..LIMIT {
        let response = client.get(&url).bearer_auth("wrong").send().await.unwrap();
        assert_eq!(response.status(), 401);
    }

    let limited = client.get(&url).bearer_auth("wrong").send().await.unwrap();
    assert_eq!(limited.status(), 429);
    assert!(limited.headers().contains_key(header::RETRY_AFTER));

    let valid = client
        .get(&url)
        .bearer_auth("sk-test")
        .send()
        .await
        .unwrap();
    assert_eq!(valid.status(), 200);
}

#[tokio::test]
async fn rate_limits_authenticated_requests() {
    const LIMIT: u32 = 5;
    let (proxy_addr, _captured) = setup_with(|config| config.server.rate_limit = LIMIT).await;
    let client = reqwest::Client::new();
    let url = format!("http://{proxy_addr}/v1/models");

    for _ in 0..LIMIT {
        let response = client
            .get(&url)
            .bearer_auth("sk-test")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }

    let limited = client
        .get(&url)
        .bearer_auth("sk-test")
        .send()
        .await
        .unwrap();
    assert_eq!(limited.status(), 429);
    assert!(limited.headers().contains_key(header::RETRY_AFTER));
}
