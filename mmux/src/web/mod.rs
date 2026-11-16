pub mod api;
pub mod assets;
pub mod ws;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{FromRequestParts, Path, Request};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use time::Duration;
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::GlobalKeyExtractor;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tower_sessions::cookie::SameSite;
use tower_sessions::{Expiry, MemoryStore, Session, SessionManagerLayer};

use crate::auth::Authenticator;
use crate::config::Config;
use crate::memory::{Memory, MemoryReadiness};
use crate::session::SessionStore;
use crate::workspace::Workspace;

pub const SESSION_USER: &str = "user";
pub const MMUX_HEADER: &str = "x-mmux";

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
    script-src 'self'; \
    style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; \
    font-src 'self'; \
    connect-src 'self' ws: wss:; \
    base-uri 'none'; \
    form-action 'self'; \
    frame-ancestors 'none'; \
    object-src 'none'";

/// Shared application state. All browser clients read from the same store.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub sessions: Arc<SessionStore>,
    pub workspace: Arc<Workspace>,
    pub auth: Arc<dyn Authenticator>,
    pub user: String,
    /// The memory vault, absent when memory is disabled or there is no state
    /// directory. Every feature degrades to "memory unavailable".
    pub memory: Option<Arc<Memory>>,
    /// Result of the startup probe of pi's MCP configuration. Replaced when a
    /// client asks for a refresh, so fixing `mcp.json` needs no mmux restart.
    pub readiness: Arc<parking_lot::RwLock<MemoryReadiness>>,
    next_client: Arc<AtomicU64>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Arc<Config>,
        sessions: Arc<SessionStore>,
        workspace: Arc<Workspace>,
        auth: Arc<dyn Authenticator>,
        user: String,
        memory: Option<Arc<Memory>>,
        readiness: MemoryReadiness,
    ) -> Self {
        Self {
            config,
            sessions,
            workspace,
            auth,
            user,
            memory,
            readiness: Arc::new(parking_lot::RwLock::new(readiness)),
            next_client: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn next_client_id(&self) -> u64 {
        self.next_client.fetch_add(1, Ordering::Relaxed)
    }
}

/// Builds the complete HTTP application.
pub fn router(state: AppState) -> Router {
    let session_layer = SessionManagerLayer::new(MemoryStore::default())
        .with_name("mmux_session")
        .with_http_only(true)
        .with_secure(state.config.server.secure_cookies)
        .with_same_site(SameSite::Strict)
        .with_path("/")
        .with_expiry(Expiry::OnInactivity(Duration::hours(
            state.config.auth.session_hours as i64,
        )));

    // Only login attempts are rate limited. A global limit is appropriate
    // because mmux exposes a single Unix account; it also survives being
    // placed behind a reverse proxy without trusting forwarding headers.
    let mut governor = GovernorConfigBuilder::default().key_extractor(GlobalKeyExtractor);
    governor.per_second(4).burst_size(5);
    let governor = governor.finish().expect("valid login rate limit");
    let login_routes = get(login_page).merge(post(login).layer(GovernorLayer::new(governor)));

    Router::new()
        .route("/", get(index))
        .route("/login", login_routes)
        .route("/logout", post(logout))
        .route("/healthz", get(health))
        .route("/api/theme", get(api::theme))
        .route("/assets/{*path}", get(asset))
        .route("/api/state", get(api::state))
        .route("/api/events", get(api::events))
        .route("/api/fs", get(api::list_directory))
        .route("/api/projects", post(api::create_project))
        .route("/api/projects/reorder", post(api::reorder_projects))
        .route(
            "/api/projects/{id}",
            patch(api::rename_project).delete(api::close_project),
        )
        .route("/api/memory/status", get(api::memory_status))
        .route(
            "/api/memory/docs",
            get(api::memory_docs).post(api::memory_doc_create),
        )
        .route(
            "/api/memory/docs/{slug}",
            get(api::memory_doc_get)
                .put(api::memory_doc_update)
                .delete(api::memory_doc_delete),
        )
        .route("/api/memory/search", get(api::memory_search))
        .route("/api/archive", get(api::archive_list))
        .route("/api/archive/{id}/restore", post(api::archive_restore))
        .route("/api/archive/{id}", delete(api::archive_delete))
        .route("/ws/panes/{id}", get(ws::pane_socket))
        .fallback(not_found)
        .layer(middleware::from_fn(cache_policy))
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CONTENT_SECURITY_POLICY),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .layer(TraceLayer::new_for_http())
        .layer(session_layer)
        .with_state(state)
}

/// An authenticated session belonging to the mmux service user.
pub struct AuthUser(pub String);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let session = Session::from_request_parts(parts, state)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        match api::session_user(&session).await {
            Ok(Some(user)) if user == state.user => Ok(Self(user)),
            Ok(Some(_)) | Ok(None) => Err(StatusCode::UNAUTHORIZED),
            Err(err) => {
                tracing::error!(error = %err, "could not load session");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }
}

async fn index(session: Session) -> Response {
    match api::session_user(&session).await {
        Ok(Some(_)) => assets::html_utf8("index.html"),
        Ok(None) => Redirect::to("login").into_response(),
        Err(err) => {
            tracing::error!(error = %err, "could not load session");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn login_page(session: Session) -> Response {
    match api::session_user(&session).await {
        Ok(Some(_)) => Redirect::to("./").into_response(),
        Ok(None) => assets::html_utf8("login.html"),
        Err(err) => {
            tracing::error!(error = %err, "could not load session");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login(
    axum::extract::State(state): axum::extract::State<AppState>,
    session: Session,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Response {
    if let Err(err) = api::require_mutation(&headers) {
        return err.into_response();
    }

    let username = request.username.trim().to_owned();
    let password = request.password;
    if username.is_empty() || password.is_empty() {
        return error_response(StatusCode::UNAUTHORIZED, "invalid username or password");
    }

    let authenticator = state.auth.clone();
    let result =
        tokio::task::spawn_blocking(move || authenticator.authenticate(&username, &password)).await;
    match result {
        Ok(Ok(())) => {
            if let Err(err) = session.cycle_id().await {
                tracing::error!(error = %err, "could not rotate session id");
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "could not sign in");
            }
            if let Err(err) = session.insert(SESSION_USER, state.user.clone()).await {
                tracing::error!(error = %err, "could not store session");
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "could not sign in");
            }
            tracing::info!(user = %state.user, "login succeeded");
            (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
        }
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "login failed");
            error_response(StatusCode::UNAUTHORIZED, "invalid username or password")
        }
        Err(err) => {
            tracing::error!(error = %err, "authentication task failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "could not sign in")
        }
    }
}

async fn logout(session: Session, headers: HeaderMap) -> Response {
    if let Err(err) = api::require_mutation(&headers) {
        return err.into_response();
    }
    if let Err(err) = session.flush().await {
        tracing::error!(error = %err, "could not clear session");
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "could not sign out");
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

async fn health() -> &'static str {
    "ok"
}

async fn asset(Path(path): Path<String>) -> Response {
    assets::serve(&path)
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

async fn cache_policy(request: Request, next: Next) -> Response {
    let is_asset = request.uri().path().starts_with("/assets/");
    let mut response = next.run(request).await;
    if !is_asset {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}
