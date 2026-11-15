use std::convert::Infallible;
use std::path::PathBuf;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;
use tower_sessions::Session;

use super::{AppState, AuthUser, MMUX_HEADER, SESSION_USER};
use crate::session::{ProjectId, ProjectView, SessionError, SessionEvent};
use crate::theme::Appearance;
use crate::workspace::WorkspaceError;

#[derive(Debug, Serialize)]
pub struct SessionView {
    pub user: String,
    pub workspace_root: String,
    pub appearance: Appearance,
    pub projects: Vec<ProjectView>,
}

pub async fn state(State(state): State<AppState>, AuthUser(user): AuthUser) -> Json<SessionView> {
    Json(SessionView {
        user,
        workspace_root: state.workspace.root().to_string_lossy().into_owned(),
        appearance: state.config.terminal.appearance,
        projects: state.sessions.snapshot().await,
    })
}

pub async fn events(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.sessions.subscribe()).map(|result| {
        let event = match result {
            Ok(event) => event,
            // A slow client missed events. Send a generic change event so it
            // re-fetches the authoritative snapshot.
            Err(_lagged) => SessionEvent::ProjectsChanged,
        };
        let data = serde_json::to_string(&event)
            .unwrap_or_else(|_| r#"{"type":"projects_changed"}"#.to_owned());
        Ok(Event::default().data(data))
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

#[derive(Debug, serde::Serialize)]
pub struct ThemeView {
    pub appearance: Appearance,
}

/// Public appearance for the login page, which has no session yet.
pub async fn theme(State(state): State<AppState>) -> Json<ThemeView> {
    Json(ThemeView {
        appearance: state.config.terminal.appearance,
    })
}

#[derive(Debug, Deserialize)]
pub struct FsQuery {
    pub path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FsListing {
    pub root: String,
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<FsEntry>,
}

#[derive(Debug, Serialize)]
pub struct FsEntry {
    pub name: String,
    pub path: String,
}

pub async fn list_directory(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    Query(query): Query<FsQuery>,
) -> Result<Json<FsListing>, ApiError> {
    if query
        .path
        .as_deref()
        .is_some_and(|path| path.contains('\0'))
    {
        return Err(ApiError::bad_request("invalid path"));
    }
    let workspace = state.workspace.clone();
    let requested = query.path.map(PathBuf::from);
    let listing = tokio::task::spawn_blocking(move || workspace.list(requested.as_deref()))
        .await
        .map_err(ApiError::from_join)?
        .map_err(ApiError::from)?;

    Ok(Json(FsListing {
        root: listing.root.to_string_lossy().into_owned(),
        path: listing.path.to_string_lossy().into_owned(),
        parent: listing
            .parent
            .map(|path| path.to_string_lossy().into_owned()),
        entries: listing
            .entries
            .into_iter()
            .map(|entry| FsEntry {
                name: entry.name,
                path: entry.path.to_string_lossy().into_owned(),
            })
            .collect(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct CreateProjectRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

pub async fn create_project(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    headers: HeaderMap,
    Json(request): Json<CreateProjectRequest>,
) -> Result<(StatusCode, Json<ProjectView>), ApiError> {
    require_mutation(&headers)?;
    if request
        .cwd
        .as_deref()
        .is_some_and(|path| path.contains('\0'))
    {
        return Err(ApiError::bad_request("invalid working directory"));
    }

    let workspace = state.workspace.clone();
    let requested = request
        .cwd
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.root().to_path_buf());
    let cwd = tokio::task::spawn_blocking(move || workspace.resolve(&requested))
        .await
        .map_err(ApiError::from_join)?
        .map_err(ApiError::from)?;

    let project = state
        .sessions
        .create_project(request.name, cwd)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(project)))
}

#[derive(Debug, Deserialize)]
pub struct RenameProjectRequest {
    pub name: String,
}

pub async fn rename_project(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    headers: HeaderMap,
    Path(id): Path<u64>,
    Json(request): Json<RenameProjectRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_mutation(&headers)?;
    let found = state
        .sessions
        .rename_project(ProjectId::new(id), &request.name)
        .await
        .map_err(ApiError::from)?;
    if !found {
        return Err(ApiError::not_found("no such project"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn close_project(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Result<StatusCode, ApiError> {
    require_mutation(&headers)?;
    if !state.sessions.close_project(ProjectId::new(id)).await {
        return Err(ApiError::not_found("no such project"));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub fn require_mutation(headers: &HeaderMap) -> Result<(), ApiError> {
    let present = headers
        .get(MMUX_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "1");
    if present {
        Ok(())
    } else {
        Err(ApiError::forbidden("missing request header"))
    }
}

/// Verifies a session belongs to the signed-in Unix user.
pub async fn session_user(
    session: &Session,
) -> Result<Option<String>, tower_sessions::session::Error> {
    session.get::<String>(SESSION_USER).await
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn from_join(err: tokio::task::JoinError) -> Self {
        tracing::error!(error = %err, "blocking task failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

impl From<WorkspaceError> for ApiError {
    fn from(err: WorkspaceError) -> Self {
        match err {
            WorkspaceError::NotFound => Self::not_found("no such directory"),
            WorkspaceError::NotADirectory => Self::bad_request("not a directory"),
            WorkspaceError::OutsideRoot => {
                Self::bad_request("directory is outside the workspace root")
            }
            WorkspaceError::PermissionDenied => Self::forbidden("permission denied"),
            WorkspaceError::Io(err) => {
                tracing::warn!(error = %err, "directory listing failed");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not read directory",
                )
            }
        }
    }
}

impl From<SessionError> for ApiError {
    fn from(err: SessionError) -> Self {
        match err {
            SessionError::InvalidName(message) => Self::bad_request(message),
            SessionError::Other(err) => {
                tracing::warn!(error = %err, "session operation failed");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not create project",
                )
            }
        }
    }
}
