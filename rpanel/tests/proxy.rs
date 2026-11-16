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
            "{} {} host={} x-test={} proto={} xff={} real={} xfh={} xfp={} origin={} referer={} body={}",
            parts.method,
            parts.uri,
            get(&parts.headers, "host"),
            get(&parts.headers, "x-test"),
            get(&parts.headers, "x-forwarded-proto"),
            get(&parts.headers, "x-forwarded-for"),
            get(&parts.headers, "x-real-ip"),
            get(&parts.headers, "x-forwarded-host"),
            get(&parts.headers, "x-forwarded-prefix"),
            get(&parts.headers, "origin"),
            get(&parts.headers, "referer"),
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
        client: build_client(false).unwrap(),
        panel: None,
    })
}

fn app(state: Arc<AppState>) -> Router {
    Router::new().fallback(proxy::handler).with_state(state)
}

#[test]
fn example_config_parses() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/rpanel.example.yaml");
    let text = std::fs::read_to_string(path).unwrap();
    let config = Config::parse(&text).unwrap();
    assert_eq!(config.groups.len(), 3);
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
async fn sets_forwarded_prefix_without_trailing_slash() {
    let host = upstream().await;
    let config = Config::parse(&format!(
        r#"
server: {{ allow_insecure: true }}
groups:
  app:
    host: "{host}"
    forwarded_prefix: true
    paths:
      "/app/*": "/*"
"#
    ))
    .unwrap();

    let request = Request::builder()
        .uri("/app/x")
        .header("host", "example.test")
        .body(Body::empty())
        .unwrap();
    let response = app(state(config)).oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("GET /x"), "{text}");
    assert!(text.contains("xfp=/app"), "{text}");
}

#[tokio::test]
async fn keeps_cross_origin_headers_but_rewrites_same_origin() {
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

    let request_with = |origin: &str| {
        Request::builder()
            .uri("/api/page")
            .header("host", "example.test")
            .header("origin", origin)
            .header("referer", format!("{origin}/api/page"))
            .body(Body::empty())
            .unwrap()
    };

    let same = app(state(config.clone()))
        .oneshot(request_with("http://example.test"))
        .await
        .unwrap();
    let body = to_bytes(same.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains(&format!("origin=http://{host}")), "{text}");
    assert!(
        text.contains(&format!("referer=http://{host}/api/page")),
        "{text}"
    );

    let cross = app(state(config))
        .oneshot(request_with("http://evil.test"))
        .await
        .unwrap();
    let body = to_bytes(cross.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("origin=http://evil.test"), "{text}");
    assert!(text.contains("referer=http://evil.test/api/page"), "{text}");
}

#[tokio::test]
async fn exact_route_wins_over_catch_all_without_redirect() {
    let host = upstream().await;
    let config = Config::parse(&format!(
        r#"
server: {{ allow_insecure: true }}
groups:
  root:
    host: "{host}"
    paths:
      "/*": "/root/*"
  health:
    host: "{host}"
    paths:
      "/health": "/health"
"#
    ))
    .unwrap();

    let request = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let response = app(state(config)).oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("GET /health"));
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
        client: build_client(false).unwrap(),
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
    assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
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

/// An upstream that emits root-absolute URLs and echoes the accepted encoding,
/// like a WebUI that does not know it is served under a path prefix.
async fn upstream_absolute() -> String {
    let app = Router::new().fallback(any(|request: Request| async move {
        let encoding = request
            .headers()
            .get(header::ACCEPT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("none")
            .to_owned();
        let body = format!(
            r#"<!doctype html><link href="/css/style.css"><script>fetch("/api/v2/x")</script><!-- encoding: {encoding} -->"#
        );
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html"),
                (header::LOCATION, "/login"),
            ],
            body,
        )
    }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr.to_string()
}

async fn proxy_html(rewrite: bool) -> (StatusCode, HeaderMap, String) {
    let host = upstream_absolute().await;
    let config = Config::parse(&format!(
        r#"
server: {{ allow_insecure: true }}
groups:
  app:
    host: "{host}"
    rewrite: {rewrite}
    paths:
      "/app/*": "/*"
"#
    ))
    .unwrap();

    let request = Request::builder()
        .uri("/app/")
        .header("accept-encoding", "gzip")
        .body(Body::empty())
        .unwrap();
    let response = app(state(config)).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn rewrites_absolute_urls_and_redirects() {
    let (status, headers, body) = proxy_html(true).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::LOCATION], "/app/login");
    assert!(body.contains(r#"href="/app/css/style.css""#), "{body}");
    // Script strings are never rewritten (that corrupts regex literals);
    // only markup attributes and CSS urls are.
    assert!(body.contains(r#"fetch("/api/v2/x")"#), "{body}");
    // The upstream was asked for an uncompressed body so it can be rewritten.
    assert!(body.contains("encoding: none"), "{body}");
    assert_eq!(
        headers[header::CONTENT_LENGTH],
        body.len().to_string().as_str()
    );
}

#[tokio::test]
async fn leaves_responses_alone_without_rewrite() {
    let (status, headers, body) = proxy_html(false).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::LOCATION], "/login");
    assert!(body.contains(r#"href="/css/style.css""#), "{body}");
    assert!(body.contains(r#"fetch("/api/v2/x")"#), "{body}");
    assert!(body.contains("encoding: gzip"), "{body}");
}

#[tokio::test]
async fn preserves_origin_and_referer_for_reverse_proxy_aware_apps() {
    let host = upstream().await;
    let config = Config::parse(&format!(
        r#"
server: {{ allow_insecure: true }}
groups:
  app:
    host: "{host}"
    preserve_origin: true
    paths:
      "/app/*": "/*"
"#
    ))
    .unwrap();

    let request = Request::builder()
        .uri("/app/x")
        .header("host", "example.test")
        .header("origin", "https://example.test")
        .header("referer", "https://example.test/app/x")
        .body(Body::empty())
        .unwrap();
    let response = app(state(config)).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    // qBittorrent compares these against X-Forwarded-Host, so they must arrive
    // as the client sent them.
    assert!(text.contains("origin=https://example.test"), "{text}");
    assert!(
        text.contains("referer=https://example.test/app/x"),
        "{text}"
    );
    assert!(text.contains("xfh=example.test"), "{text}");
}

#[tokio::test]
async fn canonicalizes_default_ports_for_origin_checks() {
    let host = upstream().await;
    let config = Config::parse(&format!(
        r#"
server: {{ cert: "/dev/null", key: "/dev/null" }}
groups:
  app:
    host: "{host}"
    preserve_origin: true
    paths:
      "/app/*": "/*"
"#
    ))
    .unwrap();

    // A browser on https://example.test:443 sends `Origin`/`Referer` without
    // the default port, but the `Host` header can keep it.
    let request = Request::builder()
        .uri("/app/x")
        .header("host", "example.test:443")
        .header("origin", "https://example.test:443")
        .header("referer", "https://example.test:443/app/x")
        .body(Body::empty())
        .unwrap();
    let response = app(state(config)).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("xfh=example.test "), "{text}");
    assert!(
        text.contains("origin=https://example.test referer="),
        "{text}"
    );
    assert!(
        text.contains("referer=https://example.test/app/x "),
        "{text}"
    );
}

async fn upstream_cookies() -> String {
    let app = Router::new().fallback(any(|request: Request| async move {
        let cookie = request
            .headers()
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        (
            StatusCode::OK,
            [
                (header::SET_COOKIE, "SID=upstream; path=/; HttpOnly"),
                (header::CONTENT_TYPE, "text/plain"),
            ],
            format!("cookie={cookie}"),
        )
    }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr.to_string()
}

#[tokio::test]
async fn namespaces_cookies_per_group() {
    let host = upstream_cookies().await;
    let config = Config::parse(&format!(
        r#"
server: {{ allow_insecure: true }}
groups:
  app:
    host: "{host}"
    cookie_namespace: true
    paths:
      "/app/*": "/*"
"#
    ))
    .unwrap();

    // The browser still holds a bare `SID` for the whole origin; it must not
    // shadow the group's own cookie, and it must not be forwarded.
    let request = Request::builder()
        .uri("/app/x")
        .header("cookie", "SID=stale; other=1; rp_app_SID=fresh")
        .body(Body::empty())
        .unwrap();
    let response = app(state(config)).oneshot(request).await.unwrap();

    let cookies: Vec<_> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect();
    assert_eq!(cookies, ["rp_app_SID=upstream; path=/; HttpOnly"]);

    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&body), "cookie=other=1; SID=fresh");
}
