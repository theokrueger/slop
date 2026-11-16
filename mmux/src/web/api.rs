use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
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
use uuid::Uuid;

use super::{AppState, AuthUser, MMUX_HEADER, SESSION_USER};
use crate::memory::{
    DocKind, Filters, Memory, MemoryError, NewDoc, Origin, Update, Workspace, identity,
};
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
pub struct ReorderProjectsRequest {
    pub project_ids: Vec<ProjectId>,
}

pub async fn reorder_projects(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    headers: HeaderMap,
    Json(request): Json<ReorderProjectsRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_mutation(&headers)?;
    if !state.sessions.reorder_projects(&request.project_ids).await {
        return Err(ApiError::bad_request(
            "project order does not match the open projects",
        ));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
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
    if !state.sessions.archive_project(ProjectId::new(id)).await {
        return Err(ApiError::not_found("no such project"));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Project memory
// ---------------------------------------------------------------------------

fn memory(state: &AppState) -> Result<Arc<Memory>, ApiError> {
    state
        .memory
        .clone()
        .ok_or_else(|| ApiError::bad_request("project memory is disabled"))
}

/// Memory is keyed by working directory: the query parameter wins, then the
/// most recently used workspace, then the first open project, then the server's
/// own directory.
async fn resolve_workspace(
    state: &AppState,
    memory: Arc<Memory>,
    requested: Option<String>,
    create: bool,
) -> Result<Workspace, ApiError> {
    let requested = requested.filter(|path| !path.trim().is_empty());
    let fallback = if requested.is_none() {
        let snapshot = state.sessions.snapshot().await;
        snapshot
            .first()
            .map(|project| PathBuf::from(&project.cwd))
            .or_else(|| std::env::current_dir().ok())
    } else {
        None
    };

    tokio::task::spawn_blocking(move || -> anyhow::Result<Workspace> {
        if let Some(path) = requested {
            let path = std::path::PathBuf::from(path);
            return if create {
                memory.workspace_for(&path)
            } else {
                memory.workspace_view(&path)
            };
        }
        if let Some(workspace) = memory.known_workspaces()?.into_iter().next() {
            return Ok(workspace);
        }
        let cwd = fallback.unwrap_or_else(|| PathBuf::from("."));
        if create {
            memory.workspace_for(&cwd)
        } else {
            memory.workspace_view(&cwd)
        }
    })
    .await
    .map_err(ApiError::from_join)?
    .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

fn filters(
    kind: Option<&str>,
    tag: Option<String>,
    since: Option<String>,
) -> Result<Filters, ApiError> {
    let kind = match kind.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => {
            Some(DocKind::parse(value).map_err(|err| ApiError::bad_request(err.to_string()))?)
        }
        None => None,
    };
    Ok(Filters { kind, tag, since })
}

#[derive(Debug, Serialize)]
pub struct MemoryStatusView {
    pub enabled: bool,
    pub state: &'static str,
    pub message: Option<String>,
    pub advice: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    /// `?refresh=1` re-runs the read-only probe. It is cheap (one file read and
    /// a PATH lookup), so the memory panel can re-check after a config edit.
    #[serde(default)]
    pub refresh: Option<String>,
}

pub async fn memory_status(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    Query(query): Query<StatusQuery>,
) -> Json<MemoryStatusView> {
    let refresh = query
        .refresh
        .as_deref()
        .is_some_and(|value| matches!(value, "1" | "true" | "yes"));
    if refresh {
        let verdict = crate::memory::readiness::check(&state.config.memory);
        *state.readiness.write() = verdict;
    }
    let readiness = state.readiness.read();
    Json(MemoryStatusView {
        enabled: state.memory.is_some(),
        state: readiness.state_name(),
        message: readiness.message(),
        advice: readiness.advice(),
    })
}

#[derive(Debug, Deserialize)]
pub struct DocsQuery {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
}

pub async fn memory_docs(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    Query(query): Query<DocsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let memory = memory(&state)?;
    let workspace = resolve_workspace(&state, memory.clone(), query.workspace, false).await?;
    let filters = filters(query.kind.as_deref(), query.tag, None)?;
    let search = query
        .q
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let handle = workspace.clone();

    let docs =
        tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>, MemoryError> {
            match search {
                Some(text) => {
                    let hits = memory.search(Some(&handle), &text, &filters, false)?;
                    Ok(hits
                        .into_iter()
                        .map(|hit| {
                            serde_json::json!({
                                "slug": hit.slug,
                                "title": hit.title,
                                "kind": hit.kind,
                                "tags": [],
                                "created": "",
                                "updated": hit.updated,
                                "author": hit.author,
                            })
                        })
                        .collect())
                }
                None => {
                    let summaries = memory.summaries(&handle, &filters)?;
                    Ok(summaries
                        .into_iter()
                        .map(|summary| serde_json::to_value(summary).unwrap_or_default())
                        .collect())
                }
            }
        })
        .await
        .map_err(ApiError::from_join)?
        .map_err(ApiError::from)?;

    Ok(Json(serde_json::json!({
        "workspace": { "key": workspace.key, "path": workspace.path },
        "docs": docs,
    })))
}

fn doc_view(doc: crate::memory::Doc) -> Result<serde_json::Value, ApiError> {
    let hash = doc.hash();
    let mut value = serde_json::to_value(&doc)
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    value["hash"] = serde_json::Value::String(hash);
    Ok(value)
}

pub async fn memory_doc_get(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    Path(slug): Path<String>,
    Query(query): Query<DocsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let memory = memory(&state)?;
    let workspace = resolve_workspace(&state, memory.clone(), query.workspace, false).await?;
    let doc = tokio::task::spawn_blocking(move || memory.read(&workspace, &slug))
        .await
        .map_err(ApiError::from_join)?
        .map_err(ApiError::from)?;
    Ok(Json(doc_view(doc)?))
}

#[derive(Debug, Deserialize)]
pub struct NewDocRequest {
    pub title: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub supersedes: Vec<String>,
    #[serde(default)]
    pub workspace: Option<String>,
}

pub async fn memory_doc_create(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    headers: HeaderMap,
    Json(request): Json<NewDocRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    require_mutation(&headers)?;
    let memory = memory(&state)?;
    let workspace = resolve_workspace(&state, memory.clone(), request.workspace, true).await?;
    let kind = match request.kind.as_deref() {
        Some(value) => {
            DocKind::parse(value).map_err(|err| ApiError::bad_request(err.to_string()))?
        }
        None => DocKind::Note,
    };
    let author = identity::author(Origin::Ui);
    let doc = tokio::task::spawn_blocking(move || {
        memory.add(
            &workspace,
            NewDoc {
                title: request.title,
                kind,
                tags: request.tags,
                body: request.body,
                paths: request.paths,
                supersedes: request.supersedes,
            },
            &author,
        )
    })
    .await
    .map_err(ApiError::from_join)?
    .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(doc_view(doc)?)))
}

#[derive(Debug, Deserialize)]
pub struct UpdateDocRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
}

pub async fn memory_doc_update(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(request): Json<UpdateDocRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_mutation(&headers)?;
    let expected = headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().trim_matches('"').to_owned())
        .filter(|value| !value.is_empty());
    let memory = memory(&state)?;
    let workspace = resolve_workspace(&state, memory.clone(), request.workspace, true).await?;
    let kind = match request.kind.as_deref() {
        Some(value) => {
            Some(DocKind::parse(value).map_err(|err| ApiError::bad_request(err.to_string()))?)
        }
        None => None,
    };
    let doc = tokio::task::spawn_blocking(move || {
        memory.update(
            &workspace,
            &slug,
            Update {
                title: request.title,
                kind,
                tags: request.tags,
                body: request.body,
                expected_hash: expected,
            },
        )
    })
    .await
    .map_err(ApiError::from_join)?
    .map_err(ApiError::from)?;
    Ok(Json(doc_view(doc)?))
}

pub async fn memory_doc_delete(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Query(query): Query<DocsQuery>,
) -> Result<StatusCode, ApiError> {
    require_mutation(&headers)?;
    let memory = memory(&state)?;
    let workspace = resolve_workspace(&state, memory.clone(), query.workspace, true).await?;
    tokio::task::spawn_blocking(move || memory.remove(&workspace, &slug))
        .await
        .map_err(ApiError::from_join)?
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct MemorySearchQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub all_workspaces: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
}

pub async fn memory_search(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    Query(query): Query<MemorySearchQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let memory = memory(&state)?;
    let workspace = resolve_workspace(&state, memory.clone(), query.workspace, false).await?;
    let filters = filters(query.kind.as_deref(), query.tag, query.since)?;
    let all = query
        .all_workspaces
        .as_deref()
        .map(|value| matches!(value, "1" | "true" | "yes"))
        .unwrap_or(false);
    let text = query.q.unwrap_or_default();
    let limit = query.limit.unwrap_or(20).clamp(1, 200);
    let hits =
        tokio::task::spawn_blocking(move || memory.search(Some(&workspace), &text, &filters, all))
            .await
            .map_err(ApiError::from_join)?
            .map_err(ApiError::from)?;
    let hits: Vec<_> = hits.into_iter().take(limit).collect();
    Ok(Json(serde_json::json!({ "hits": hits })))
}

// ---------------------------------------------------------------------------
// Project archive
// ---------------------------------------------------------------------------

pub async fn archive_list(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
) -> Json<serde_json::Value> {
    match state.sessions.archived_projects().await {
        Ok(entries) => {
            let entries: Vec<serde_json::Value> = entries
                .into_iter()
                .map(|entry| {
                    let memory_docs = state.memory.as_ref().and_then(|memory| {
                        memory
                            .existing_workspace_for(std::path::Path::new(&entry.cwd))
                            .map(|workspace| memory.doc_count(&workspace))
                    });
                    serde_json::json!({
                        "id": entry.id,
                        "name": entry.name,
                        "cwd": entry.cwd,
                        "archived_at": entry.archived_at,
                        "pane_count": entry.pane_count,
                        "memory_docs": memory_docs,
                    })
                })
                .collect();
            Json(serde_json::json!({
                "readable": true,
                "error": null,
                "entries": entries,
            }))
        }
        Err(err) => {
            tracing::warn!(error = %err, "could not read the project archive");
            Json(serde_json::json!({
                "readable": false,
                "error": err.to_string(),
                "entries": [],
            }))
        }
    }
}

fn archive_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError::bad_request("invalid archived project id"))
}

pub async fn archive_restore(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<ProjectView>), ApiError> {
    require_mutation(&headers)?;
    let id = archive_id(&id)?;
    let project = state
        .sessions
        .restore_archived(id)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(project)))
}

pub async fn archive_delete(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_mutation(&headers)?;
    let id = archive_id(&id)?;
    state.sessions.delete_archived(id).await.map_err(|err| {
        if err.to_string().contains("no archived project") {
            ApiError::not_found("no such archived project")
        } else {
            tracing::warn!(error = %err, "could not delete archived project");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not delete archived project",
            )
        }
    })?;
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

impl From<MemoryError> for ApiError {
    fn from(err: MemoryError) -> Self {
        match err {
            MemoryError::NotFound(message) => Self::not_found(message),
            MemoryError::Conflict(message) => Self::new(StatusCode::CONFLICT, message),
            MemoryError::Invalid(message) => Self::bad_request(message),
            MemoryError::Disabled => Self::bad_request("project memory is disabled"),
            MemoryError::Other(err) => {
                tracing::warn!(error = %err, "memory operation failed");
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "memory operation failed")
            }
        }
    }
}

impl From<SessionError> for ApiError {
    fn from(err: SessionError) -> Self {
        match err {
            SessionError::InvalidName(message) => Self::bad_request(message),
            SessionError::Conflict(message) => Self::new(StatusCode::CONFLICT, message),
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
