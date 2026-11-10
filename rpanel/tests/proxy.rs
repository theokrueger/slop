use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, Request},
    http::{HeaderMap, StatusCode, header},
    routing::{any, get},
};
use rpanel::{
    config::Config,
    net::NetworkHistory,
    panel, proxy,
    route::build_routes,
    state::{AppState, PanelState, build_client},
};
use tower::ServiceExt;

async fn upstream() -> String {
    let app = Router::new().fallback(any(|request: Request| async move {
        let (parts, body) = request.into_parts();
        let body = to_bytes(body, 1024 * 1024).await.unwrap();
        let get = |headers: &HeaderMap, name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_owned()
        };
        let text = format!(
            "{} {} host={} x-test={} proto={} xff={} real={} xfh={} body={}",
            parts.method,
            parts.uri,
            get(&parts.headers, "host"),
            get(&parts.headers, "x-test"),
            get(&parts.headers, "x-forwarded-proto"),
            get(&parts.headers, "x-forwarded-for"),
            get(&parts.headers, "x-real-ip"),
            get(&parts.headers, "x-forwarded-host"),
            String::from_utf8_lossy(&body)
        );
        (
            StatusCode::CREATED,
            [(header::HeaderName::from_static("x-upstream"), "yes")],
            text,
        )
    }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr.to_string()
}

fn state(config: Config) -> Arc<AppState> {
    let routes = Arc::new(build_routes(&config.groups));
    Arc::new(AppState {
        config: Arc::new(config),
        routes,
        client: build_client().unwrap(),
        panel: None,
    })
}

fn app(state: Arc<AppState>) -> Router {
    Router::new().fallback(proxy::handler).with_state(state)
}

#[tokio::test]
async fn proxies_and_rewrites_paths() {
    let host = upstream().await;
    let config = Config::parse(&format!(
        r#"
server: {{ allow_insecure: true }}
groups:
  api:
    host: "{host}"
    paths:
      "/api/*": "/v1/*"
"#
    ))
    .unwrap();

    let mut request = Request::builder()
        .method("POST")
        .uri("/api/hello?x=1")
        .header("host", "example.test")
        .header("x-test", "yes")
        .body(Body::from("hi"))
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([192, 0, 2, 1], 1234))));
    let response = app(state(config)).oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["x-upstream"], "yes");
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("POST /v1/hello?x=1"), "{text}");
    assert!(text.contains(&format!("host={host}")), "{text}");
    assert!(text.contains("x-test=yes"), "{text}");
    assert!(text.contains("proto=http"), "{text}");
    assert!(text.contains("xff=192.0.2.1"), "{text}");
    assert!(text.contains("real=192.0.2.1"), "{text}");
    assert!(text.contains("xfh=example.test"), "{text}");
    assert!(text.contains("body=hi"), "{text}");
}

#[tokio::test]
async fn returns_404_for_unmatched_paths() {
    let config = Config::parse(
        r#"
server: { allow_insecure: true }
groups:
  api:
    host: "127.0.0.1:1"
    paths:
      "/api/*": "/v1/*"
"#,
    )
    .unwrap();

    let request = Request::builder()
        .uri("/other")
        .body(Body::empty())
        .unwrap();
    let response = app(state(config)).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn serves_panel_at_root() {
    let config = Config::parse(
        r#"
server: { allow_insecure: true }
panel:
  status: { storage: false, ram: false, uptime: false, network: false }
  links: { Tary: /tary }
groups: {}
"#,
    )
    .unwrap();

    let panel_state = Arc::new(PanelState {
        config: config.panel.clone(),
        network: Arc::new(NetworkHistory::new(Duration::from_secs(60))),
    });
    let state = Arc::new(AppState {
        config: Arc::new(config),
        routes: Arc::new(Vec::new()),
        client: build_client().unwrap(),
        panel: Some(panel_state),
    });
    let app = Router::new()
        .route("/", get(panel::handler))
        .fallback(proxy::handler)
        .with_state(state);

    let request = Request::builder().uri("/").body(Body::empty()).unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        response.headers()[header::X_CONTENT_TYPE_OPTIONS],
        "nosniff"
    );
    assert!(
        response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("default-src 'none'")
    );
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("rpanel"), "{text}");
    assert!(text.contains("Tary"), "{text}");
}
