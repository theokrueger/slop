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
use mmux::memory::{Memory, MemoryReadiness};
use mmux::session::SessionStore;
use mmux::theme::Appearance;
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
    /// Kept alive for the lifetime of the app: memory writes land here.
    _memory: Option<Arc<Memory>>,
    root: TempDir,
    /// Kept alive so the state directory (projects and memory) outlives the app.
    _state: TempDir,
}

impl TestApp {
    fn new(template: Vec<PaneConfig>) -> Self {
        Self::with_appearance(template, Appearance::Dark)
    }

    fn with_appearance(template: Vec<PaneConfig>, appearance: Appearance) -> Self {
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("alpha")).unwrap();
        std::fs::create_dir(root.path().join("beta")).unwrap();
        std::fs::create_dir(root.path().join("alpha/nested")).unwrap();

        let state = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace.root = root.path().to_path_buf();
        config.terminal.rows = 24;
        config.terminal.cols = 80;
        config.new_project.panes = template;
        config.terminal.appearance = appearance;
        config.memory.dir = Some(state.path().join("memory"));
        // Keep the readiness probe away from the real user config.
        config.memory.pi_agent_dir = state.path().join("pi");
        config.validate().unwrap();
        let memory = Memory::from_config(&config.memory, None).map(Arc::new);
        let config = Arc::new(config);

        let sessions = Arc::new(SessionStore::with_state_dir(
            config.new_project.panes.clone(),
            (config.terminal.rows, config.terminal.cols),
            state.path().to_path_buf(),
        ));
        let workspace = Arc::new(Workspace::new(root.path()).unwrap());
        let app_state = AppState::new(
            config.clone(),
            sessions.clone(),
            workspace,
            Arc::new(MockAuth),
            USER.to_owned(),
            memory.clone(),
            MemoryReadiness::NotApplicable,
        );
        // Give each test its own session store and router so the login
        // governor is not shared between tests.
        let app = web::router(app_state);
        Self {
            app,
            sessions,
            _memory: memory,
            root,
            _state: state,
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
    assert_eq!(state["projects"].as_array().unwrap().len(), 0);
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
    assert!(index_html.contains("new project"));
    assert!(!index_html.contains(r#"<div class="brand">mmux</div>"#));
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
            "/api/projects",
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
async fn projects_are_created_in_the_chosen_directory_and_can_be_renamed_and_closed() {
    let app = TestApp::new(TestApp::default_template());
    let cookie = app.login().await;
    let alpha = app.root.path().join("alpha");
    let unknown = app.root.path().join("missing");

    let (status, _, body) = app
        .call(json_request(
            Method::POST,
            "/api/projects",
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
    let project: Value = serde_json::from_slice(&body).unwrap();
    let project_id = project["id"].as_u64().unwrap();
    assert_eq!(project["name"], "agents");
    assert_eq!(project["cwd"], alpha.to_string_lossy().as_ref());
    assert_eq!(project["panes"].as_array().unwrap().len(), 1);
    assert_eq!(project["panes"][0]["slot"], "top-left");
    assert_eq!(project["panes"][0]["status"]["state"], "running");

    let state = app.state(&cookie).await;
    assert_eq!(state["projects"][0]["name"], "agents");
    assert_eq!(
        state["projects"][0]["cwd"],
        alpha.to_string_lossy().as_ref()
    );

    let (status, _, _) = app
        .call(json_request(
            Method::PATCH,
            &format!("/api/projects/{project_id}"),
            json!({ "name": "renamed" }),
            &[(header::COOKIE.as_str(), &cookie)],
            true,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(app.state(&cookie).await["projects"][0]["name"], "renamed");

    let (status, _, _) = app
        .call(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/projects/{project_id}"))
                .header(header::COOKIE, &cookie)
                .header("x-mmux", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.state(&cookie).await["projects"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let (status, _, _) = app
        .call(json_request(
            Method::POST,
            "/api/projects",
            json!({ "cwd": unknown }),
            &[(header::COOKIE.as_str(), &cookie)],
            true,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    app.shutdown().await;
}

#[tokio::test]
async fn projects_can_be_reordered_without_accepting_stale_orders() {
    let app = TestApp::new(TestApp::default_template());
    let cookie = app.login().await;
    let mut project_ids = Vec::new();
    for (name, directory) in [("first", "alpha"), ("second", "beta")] {
        let (status, _, body) = app
            .call(json_request(
                Method::POST,
                "/api/projects",
                json!({ "name": name, "cwd": app.root.path().join(directory) }),
                &[(header::COOKIE.as_str(), &cookie)],
                true,
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED);
        let project: Value = serde_json::from_slice(&body).unwrap();
        project_ids.push(project["id"].as_u64().unwrap());
    }

    let reordered = json!({ "project_ids": [project_ids[1], project_ids[0]] });
    let (status, _, body) = app
        .call(json_request(
            Method::POST,
            "/api/projects/reorder",
            reordered,
            &[(header::COOKIE.as_str(), &cookie)],
            true,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let projects = app.state(&cookie).await;
    assert_eq!(projects["projects"][0]["name"], "second");
    assert_eq!(projects["projects"][1]["name"], "first");

    let (status, _, _) = app
        .call(json_request(
            Method::POST,
            "/api/projects/reorder",
            json!({ "project_ids": [project_ids[1], project_ids[1]] }),
            &[(header::COOKIE.as_str(), &cookie)],
            true,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let projects = app.state(&cookie).await;
    assert_eq!(projects["projects"][0]["name"], "second");
    assert_eq!(projects["projects"][1]["name"], "first");

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
            "/api/projects",
            json!({ "cwd": app.root.path().join("alpha") }),
            &[(header::COOKIE.as_str(), &cookie)],
            true,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let project: Value = serde_json::from_slice(&body).unwrap();
    let pane_id = project["panes"][0]["id"].as_u64().unwrap();

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
        if frame[0] == 3 || frame[0] == 1 {
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
        state["projects"][0]["panes"][0]["status"]["state"], "running",
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

#[tokio::test]
async fn memory_documents_round_trip_through_the_api() {
    let app = TestApp::new(TestApp::default_template());
    let cookie = app.login().await;
    let workspace = app.root.path().to_string_lossy().into_owned();

    let status = app
        .call(
            Request::builder()
                .uri("/api/memory/status")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status.0, StatusCode::OK);
    let view: Value = serde_json::from_slice(&status.2).unwrap();
    assert_eq!(view["enabled"], true);
    assert_eq!(view["state"], "not_applicable");

    // A refresh re-runs the read-only probe without restarting mmux.
    let (status, _, body) = app
        .call(
            Request::builder()
                .uri("/api/memory/status?refresh=1")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let refreshed: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        matches!(
            refreshed["state"].as_str(),
            Some("missing") | Some("not_applicable")
        ),
        "{refreshed}"
    );

    // Creating a document requires the mutation header.
    let (status, _, _) = app
        .call(json_request(
            Method::POST,
            "/api/memory/docs",
            json!({ "title": "Restart policy", "kind": "decision", "body": "panes relaunch", "tags": ["session"], "workspace": workspace }),
            &[(header::COOKIE.as_str(), &cookie)],
            false,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _, body) = app
        .call(json_request(
            Method::POST,
            "/api/memory/docs",
            json!({ "title": "Restart policy", "kind": "decision", "body": "panes relaunch", "tags": ["session"], "workspace": workspace }),
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
    let doc: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc["slug"], "restart-policy");
    assert_eq!(doc["kind"], "decision");
    assert_eq!(doc["author"], "user");
    let hash = doc["hash"].as_str().unwrap().to_owned();

    let (status, _, body) = app
        .call(
            Request::builder()
                .uri(format!(
                    "/api/memory/docs?workspace={}",
                    urlencode(&workspace)
                ))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let listing: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(listing["docs"].as_array().unwrap().len(), 1);
    assert_eq!(listing["docs"][0]["slug"], "restart-policy");
    assert_eq!(listing["workspace"]["path"], workspace);

    let uri = format!(
        "/api/memory/docs/restart-policy?workspace={}",
        urlencode(&workspace)
    );
    let (status, _, body) = app
        .call(
            Request::builder()
                .uri(&uri)
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    // Optimistic update with the current hash succeeds.
    let (status, _, body) = app
        .call(json_request(
            Method::PUT,
            &uri,
            json!({ "body": "panes relaunch with the same session id" }),
            &[
                (header::COOKIE.as_str(), &cookie),
                (header::IF_MATCH.as_str(), &hash),
            ],
            true,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let updated: Value = serde_json::from_slice(&body).unwrap();
    assert_ne!(updated["hash"], hash);

    // A stale hash is a conflict.
    let (status, _, _) = app
        .call(json_request(
            Method::PUT,
            &uri,
            json!({ "body": "another edit" }),
            &[
                (header::COOKIE.as_str(), &cookie),
                (header::IF_MATCH.as_str(), &hash),
            ],
            true,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _, body) = app
        .call(
            Request::builder()
                .uri(format!(
                    "/api/memory/search?q=session&workspace={}",
                    urlencode(&workspace)
                ))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let hits: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(hits["hits"].as_array().unwrap().len(), 1);
    assert_eq!(hits["hits"][0]["slug"], "restart-policy");

    let (status, _, _) = app
        .call(
            Request::builder()
                .method(Method::DELETE)
                .uri(&uri)
                .header(header::COOKIE, &cookie)
                .header("x-mmux", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = app
        .call(
            Request::builder()
                .uri(&uri)
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    app.shutdown().await;
}

#[tokio::test]
async fn closing_a_project_archives_it_and_it_can_be_restored_or_deleted() {
    let app = TestApp::new(TestApp::default_template());
    let cookie = app.login().await;
    let alpha = app.root.path().join("alpha");

    let (status, _, body) = app
        .call(json_request(
            Method::POST,
            "/api/projects",
            json!({ "name": "archived one", "cwd": alpha }),
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
    let project: Value = serde_json::from_slice(&body).unwrap();
    let project_id = project["id"].as_u64().unwrap();

    // Closing moves it to the archive instead of deleting it.
    let (status, _, _) = app
        .call(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/projects/{project_id}"))
                .header(header::COOKIE, &cookie)
                .header("x-mmux", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.state(&cookie).await["projects"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let (status, _, body) = app
        .call(
            Request::builder()
                .uri("/api/archive")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let archive: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(archive["readable"], true);
    let entries = archive["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "archived one");
    assert_eq!(entries[0]["pane_count"], 1);
    let archived_id = entries[0]["id"].as_str().unwrap().to_owned();

    // Restoring brings the same project back.
    let (status, _, body) = app
        .call(json_request(
            Method::POST,
            &format!("/api/archive/{archived_id}/restore"),
            json!({}),
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
    let restored: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(restored["name"], "archived one");
    assert_eq!(
        app.state(&cookie).await["projects"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Archive it again, then delete it permanently.
    let restored_id = restored["id"].as_u64().unwrap();
    app.call(
        Request::builder()
            .method(Method::DELETE)
            .uri(format!("/api/projects/{restored_id}"))
            .header(header::COOKIE, &cookie)
            .header("x-mmux", "1")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    // Server-generated permalink uses the durable UUID, not the runtime id.
    let (_, _, body) = app
        .call(
            Request::builder()
                .uri("/api/archive")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let archive: Value = serde_json::from_slice(&body).unwrap();
    let archived_id = archive["entries"][0]["id"].as_str().unwrap().to_owned();

    let (status, _, _) = app
        .call(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/archive/{archived_id}"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _, _) = app
        .call(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/archive/{archived_id}"))
                .header(header::COOKIE, &cookie)
                .header("x-mmux", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, _, body) = app
        .call(
            Request::builder()
                .uri("/api/archive")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let archive: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(archive["entries"].as_array().unwrap().len(), 0);

    app.shutdown().await;
}

fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[tokio::test]
async fn deleted_memories_move_to_the_archive_and_can_be_restored() {
    let app = TestApp::new(TestApp::default_template());
    let cookie = app.login().await;
    let workspace = app.root.path().to_string_lossy().into_owned();
    let ws = urlencode(&workspace);

    let (status, _, body) = app
        .call(json_request(
            Method::POST,
            "/api/memory/docs",
            json!({ "title": "Archived note", "kind": "decision", "body": "secret body", "workspace": workspace }),
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

    // Deleting moves it out of the live docs and into the archive.
    let (status, _, _) = app
        .call(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/memory/docs/archived-note?workspace={ws}"))
                .header(header::COOKIE, &cookie)
                .header("x-mmux", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, _, body) = app
        .call(
            Request::builder()
                .uri(format!("/api/memory/docs?workspace={ws}"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["docs"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let (status, _, _) = app
        .call(
            Request::builder()
                .uri(format!("/api/memory/docs/archived-note?workspace={ws}"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _, body) = app
        .call(
            Request::builder()
                .uri(format!("/api/memory/archive?workspace={ws}"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let archive: Value = serde_json::from_slice(&body).unwrap();
    let entries = archive["docs"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["slug"], "archived-note");
    assert_eq!(entries[0]["kind"], "decision");
    assert!(!entries[0]["archived_at"].as_str().unwrap().is_empty());
    let id = entries[0]["id"].as_str().unwrap().to_owned();

    // The body is readable from the archive, and restore puts it back.
    let (status, _, body) = app
        .call(
            Request::builder()
                .uri(format!("/api/memory/archive/{id}?workspace={ws}"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["body"],
        "secret body"
    );

    let (status, _, _) = app
        .call(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/memory/archive/{id}/restore?workspace={ws}"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _, body) = app
        .call(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/memory/archive/{id}/restore?workspace={ws}"))
                .header(header::COOKIE, &cookie)
                .header("x-mmux", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["slug"],
        "archived-note"
    );

    let (_, _, body) = app
        .call(
            Request::builder()
                .uri(format!("/api/memory/archive?workspace={ws}"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["docs"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let (_, _, body) = app
        .call(
            Request::builder()
                .uri(format!("/api/memory/docs?workspace={ws}"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["docs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Archive again and purge it permanently.
    app.call(
        Request::builder()
            .method(Method::DELETE)
            .uri(format!("/api/memory/docs/archived-note?workspace={ws}"))
            .header(header::COOKIE, &cookie)
            .header("x-mmux", "1")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let (_, _, body) = app
        .call(
            Request::builder()
                .uri(format!("/api/memory/archive?workspace={ws}"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let archive: Value = serde_json::from_slice(&body).unwrap();
    let id = archive["docs"][0]["id"].as_str().unwrap().to_owned();
    let (status, _, _) = app
        .call(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/memory/archive/{id}?workspace={ws}"))
                .header(header::COOKIE, &cookie)
                .header("x-mmux", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, _, body) = app
        .call(
            Request::builder()
                .uri(format!("/api/memory/archive?workspace={ws}"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["docs"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    app.shutdown().await;
}

#[tokio::test]
async fn favicon_follows_the_configured_appearance() {
    let dark = TestApp::new(TestApp::default_template());
    let (status, headers, body) = dark
        .call(
            Request::builder()
                .uri("/assets/favicon.svg")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "image/svg+xml");
    let svg = String::from_utf8(body.to_vec()).unwrap();
    assert!(svg.contains("#002b36"), "{svg}");
    assert!(!svg.contains("#fdf6e3"), "{svg}");
    dark.shutdown().await;

    let light = TestApp::with_appearance(TestApp::default_template(), Appearance::Light);
    let (status, _, body) = light
        .call(
            Request::builder()
                .uri("/assets/favicon.svg")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let svg = String::from_utf8(body.to_vec()).unwrap();
    assert!(svg.contains("#fdf6e3"), "{svg}");
    assert!(svg.contains("#586e75"), "{svg}");
    assert!(!svg.contains("#002b36"), "{svg}");
    light.shutdown().await;
}
