use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use mmux::auth::{AuthError, Authenticator};
use mmux::config::{Config, PaneConfig, Slot};
use mmux::session::SessionStore;
use mmux::web::{self, AppState};
use mmux::workspace::Workspace;

const USER: &str = "me";
const PASSWORD: &str = "correct horse";

struct MockAuth;

impl Authenticator for MockAuth {
    fn authenticate(&self, username: &str, password: &str) -> Result<(), AuthError> {
        if username == USER && password == PASSWORD {
            Ok(())
        } else {
            Err(AuthError::InvalidCredentials)
        }
    }
}

struct TestApp {
    app: Router,
    sessions: Arc<SessionStore>,
    root: TempDir,
}

impl TestApp {
    fn new(template: Vec<PaneConfig>) -> Self {
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("alpha")).unwrap();
        std::fs::create_dir(root.path().join("beta")).unwrap();
        std::fs::create_dir(root.path().join("alpha/nested")).unwrap();

        let mut config = Config::default();
        config.workspace.root = root.path().to_path_buf();
        config.terminal.rows = 24;
        config.terminal.cols = 80;
        config.new_tab.panes = template;
        config.validate().unwrap();
        let config = Arc::new(config);

        let sessions = Arc::new(SessionStore::new(
            config.new_tab.panes.clone(),
            (config.terminal.rows, config.terminal.cols),
        ));
        let workspace = Arc::new(Workspace::new(root.path()).unwrap());
        let state = AppState::new(
            config.clone(),
            sessions.clone(),
            workspace,
            Arc::new(MockAuth),
            USER.to_owned(),
        );
        // Give each test its own session store and router so the login
        // governor is not shared between tests.
        let app = web::router(state);
        Self {
            app,
            sessions,
            root,
        }
    }

    fn default_template() -> Vec<PaneConfig> {
        vec![PaneConfig {
            slot: Slot::TopLeft,
            command: vec!["sh".into()],
            env: BTreeMap::new(),
        }]
    }

    async fn shutdown(&self) {
        self.sessions.shutdown().await;
    }

    async fn call(&self, request: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, body)
    }

    async fn login(&self) -> String {
        let response = self
            .call(json_request(
                Method::POST,
                "/login",
                json!({ "username": USER, "password": PASSWORD }),
                &[],
                true,
            ))
            .await;
        assert_eq!(
            response.0,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&response.2)
        );
        cookie_from_headers(&response.1)
    }

    async fn state(&self, cookie: &str) -> Value {
        let (status, _, body) = self
            .call(
                Request::builder()
                    .uri("/api/state")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }
}

fn json_request(
    method: Method,
    uri: &str,
    body: Value,
    extra_headers: &[(&str, &str)],
    mutation_header: bool,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if mutation_header {
        builder = builder.header("x-mmux", "1");
    }
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn cookie_from_headers(headers: &HeaderMap) -> String {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("mmux_session="))
        .unwrap_or_else(|| panic!("no session cookie in {headers:?}"))
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn unauthenticated(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn unauthenticated_requests_are_denied_or_redirected() {
    let app = TestApp::new(TestApp::default_template());

    let (status, headers, _) = app.call(unauthenticated(Method::GET, "/")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers.get(header::LOCATION).unwrap(), "login");

    let (status, _, _) = app.call(unauthenticated(Method::GET, "/api/state")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, body) = app.call(unauthenticated(Method::GET, "/login")).await;
    assert_eq!(status, StatusCode::OK);
    let login_html = String::from_utf8_lossy(&body);
    assert!(login_html.contains("Sign in"));
    // Relative asset URLs keep the login page working behind a path prefix.
    assert!(login_html.contains(r#"href="assets/style.css""#));
    assert!(!login_html.contains(r#"href="/assets/"#));

    app.shutdown().await;
}

#[tokio::test]
async fn login_sets_an_http_only_strict_session_cookie() {
    let app = TestApp::new(TestApp::default_template());
    let response = app
        .call(json_request(
            Method::POST,
            "/login",
            json!({ "username": USER, "password": PASSWORD }),
            &[],
            true,
        ))
        .await;
    assert_eq!(response.0, StatusCode::OK);
    let cookie = response
        .1
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("mmux_session="))
        .unwrap();
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");

    let cookie = cookie.split(';').next().unwrap();
    let state = app.state(cookie).await;
    assert_eq!(state["user"], USER);
    assert_eq!(state["tabs"].as_array().unwrap().len(), 0);
    assert_eq!(
        state["workspace_root"],
        app.root.path().to_string_lossy().as_ref()
    );

    let (status, _, body) = app
        .call(
            Request::builder()
                .uri("/")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let index_html = String::from_utf8_lossy(&body);
    assert!(index_html.contains("new tab"));
    // Relative asset URLs keep the app working behind a path prefix.
    assert!(index_html.contains(r#"src="assets/app.js""#));
    assert!(!index_html.contains(r#"src="/assets/"#));

    app.shutdown().await;
}

#[tokio::test]
async fn bad_credentials_and_missing_csrf_header_are_rejected() {
    let app = TestApp::new(TestApp::default_template());

    let (status, _, _) = app
        .call(json_request(
            Method::POST,
            "/login",
            json!({ "username": USER, "password": "wrong" }),
            &[],
            true,
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, _) = app
        .call(json_request(
            Method::POST,
            "/login",
            json!({ "username": USER, "password": PASSWORD }),
            &[],
            false,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let cookie = app.login().await;
    let (status, _, _) = app
        .call(json_request(
            Method::POST,
            "/api/tabs",
            json!({ "cwd": app.root.path().join("alpha") }),
            &[(header::COOKIE.as_str(), &cookie)],
            false,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    app.shutdown().await;
}

#[tokio::test]
async fn login_is_rate_limited() {
    let app = TestApp::new(TestApp::default_template());
    let mut saw_rate_limit = false;
    for _ in 0..10 {
        let (status, _, _) = app
            .call(json_request(
                Method::POST,
                "/login",
                json!({ "username": USER, "password": "wrong" }),
                &[],
                true,
            ))
            .await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            saw_rate_limit = true;
            break;
        }
    }
    assert!(saw_rate_limit, "login attempts were never rate limited");
    app.shutdown().await;
}

#[tokio::test]
async fn directory_listing_is_interactive_and_confined_to_the_root() {
    let app = TestApp::new(TestApp::default_template());
    let cookie = app.login().await;

    let (status, _, body) = app
        .call(
            Request::builder()
                .uri("/api/fs?path=alpha")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let listing: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        listing["path"],
        app.root.path().join("alpha").to_string_lossy().as_ref()
    );
    assert_eq!(
        listing["parent"],
        app.root.path().to_string_lossy().as_ref()
    );
    assert_eq!(listing["entries"][0]["name"], "nested");

    let (status, _, _) = app
        .call(
            Request::builder()
                .uri("/api/fs?path=..")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    app.shutdown().await;
}

#[tokio::test]
async fn tabs_are_created_in_the_chosen_directory_and_can_be_renamed_and_closed() {
    let app = TestApp::new(TestApp::default_template());
    let cookie = app.login().await;
    let alpha = app.root.path().join("alpha");
    let unknown = app.root.path().join("missing");

    let (status, _, body) = app
        .call(json_request(
            Method::POST,
            "/api/tabs",
            json!({ "name": "agents", "cwd": alpha }),
            &[(header::COOKIE.as_str(), &cookie)],
            true,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let tab: Value = serde_json::from_slice(&body).unwrap();
    let tab_id = tab["id"].as_u64().unwrap();
    assert_eq!(tab["name"], "agents");
    assert_eq!(tab["cwd"], alpha.to_string_lossy().as_ref());
    assert_eq!(tab["panes"].as_array().unwrap().len(), 1);
    assert_eq!(tab["panes"][0]["slot"], "top-left");
    assert_eq!(tab["panes"][0]["status"]["state"], "running");

    let state = app.state(&cookie).await;
    assert_eq!(state["tabs"][0]["name"], "agents");
    assert_eq!(state["tabs"][0]["cwd"], alpha.to_string_lossy().as_ref());

    let (status, _, _) = app
        .call(json_request(
            Method::PATCH,
            &format!("/api/tabs/{tab_id}"),
            json!({ "name": "renamed" }),
            &[(header::COOKIE.as_str(), &cookie)],
            true,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(app.state(&cookie).await["tabs"][0]["name"], "renamed");

    let (status, _, _) = app
        .call(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/tabs/{tab_id}"))
                .header(header::COOKIE, &cookie)
                .header("x-mmux", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.state(&cookie).await["tabs"].as_array().unwrap().len(),
        0
    );

    let (status, _, _) = app
        .call(json_request(
            Method::POST,
            "/api/tabs",
            json!({ "cwd": unknown }),
            &[(header::COOKIE.as_str(), &cookie)],
            true,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    app.shutdown().await;
}

#[tokio::test]
async fn websocket_attaches_to_shared_terminal_state() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let app = TestApp::new(TestApp::default_template());
    let cookie = app.login().await;

    let (status, _, body) = app
        .call(json_request(
            Method::POST,
            "/api/tabs",
            json!({ "cwd": app.root.path().join("alpha") }),
            &[(header::COOKIE.as_str(), &cookie)],
            true,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tab: Value = serde_json::from_slice(&body).unwrap();
    let pane_id = tab["panes"][0]["id"].as_u64().unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_router = app.app.clone();
    let server = tokio::spawn(async move { axum::serve(listener, server_router).await });

    let url = format!("ws://{addr}/ws/panes/{pane_id}");
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert(header::COOKIE, cookie.parse().unwrap());
    let (mut socket, _) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .expect("websocket connect timed out")
    .expect("websocket connect failed");

    let first = next_binary(&mut socket).await;
    assert_eq!(first[0], 0, "first frame must be a full snapshot");
    assert!(first.len() > 5);
    assert_eq!(u16::from_be_bytes([first[1], first[2]]), 24);
    assert_eq!(u16::from_be_bytes([first[3], first[4]]), 80);

    // Resize the shared screen to 20x60.
    let mut resize = Vec::from([0u8]);
    resize.extend_from_slice(&60u16.to_be_bytes());
    resize.extend_from_slice(&20u16.to_be_bytes());
    socket.send(Message::Binary(resize.into())).await.unwrap();

    let mut resized = false;
    let mut transcript = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let frame = match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(Some(Ok(Message::Binary(frame)))) => frame,
            _ => break,
        };
        if frame.is_empty() {
            continue;
        }
        if frame[0] == 0 && frame.len() >= 5 {
            let rows = u16::from_be_bytes([frame[1], frame[2]]);
            let cols = u16::from_be_bytes([frame[3], frame[4]]);
            if (rows, cols) == (20, 60) {
                resized = true;
                break;
            }
        } else if frame[0] == 1 {
            transcript.push_str(&String::from_utf8_lossy(&frame[1..]));
        }
    }
    assert!(resized, "server never sent a 20x60 snapshot");

    // Input goes to the one shared PTY and comes back as a terminal diff.
    let mut input = vec![1u8];
    input.extend_from_slice(b"printf 'MMUX_WS_OK\\n'\n");
    socket.send(Message::Binary(input.into())).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut marker_seen = false;
    while tokio::time::Instant::now() < deadline {
        let frame = match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(Some(Ok(Message::Binary(frame)))) => frame,
            _ => break,
        };
        if frame.is_empty() {
            continue;
        }
        if frame[0] == 1 {
            transcript.push_str(&String::from_utf8_lossy(&frame[1..]));
        } else if frame[0] == 0 && frame.len() >= 5 {
            transcript.push_str(&String::from_utf8_lossy(&frame[5..]));
        }
        if transcript.contains("MMUX_WS_OK") {
            marker_seen = true;
            break;
        }
    }
    assert!(
        marker_seen,
        "terminal output never contained marker: {transcript:?}"
    );

    // OSC 52 clipboard writes from the program are forwarded to the browser.
    let mut osc52 = vec![1u8];
    osc52.extend_from_slice(b"printf '\\033]52;c;TU1VWF9DTElQ\\a'\n");
    socket.send(Message::Binary(osc52.into())).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut clipboard = None;
    while tokio::time::Instant::now() < deadline {
        let frame = match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(Some(Ok(Message::Binary(frame)))) => frame,
            _ => break,
        };
        if frame.first() == Some(&2) {
            clipboard = Some(String::from_utf8_lossy(&frame[1..]).into_owned());
            break;
        }
    }
    assert_eq!(
        clipboard.as_deref(),
        Some("MMUX_CLIP"),
        "OSC 52 clipboard write was not forwarded"
    );

    let _ = socket.close(None).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let state = app.state(&cookie).await;
    assert_eq!(
        state["tabs"][0]["panes"][0]["status"]["state"], "running",
        "closing a browser socket must not stop the pane"
    );
    server.abort();
    app.shutdown().await;
}

async fn next_binary<S>(socket: &mut S) -> Vec<u8>
where
    S: futures_util::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(frame)))) => {
                return frame.to_vec();
            }
            Ok(Some(Ok(_))) => continue,
            other => panic!("expected binary websocket frame, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn logout_invalidates_the_session_cookie() {
    let app = TestApp::new(TestApp::default_template());
    let cookie = app.login().await;

    let (status, _, _) = app
        .call(
            Request::builder()
                .method(Method::POST)
                .uri("/logout")
                .header(header::COOKIE, &cookie)
                .header("x-mmux", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, _) = app
        .call(
            Request::builder()
                .uri("/api/state")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    app.shutdown().await;
}
