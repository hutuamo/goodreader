use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, ORIGIN, REFERRER_POLICY,
    SET_COOKIE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use rand::distr::{Alphanumeric, SampleString};
use serde::Deserialize;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower_http::trace::TraceLayer;

use crate::agent::AgentCoordinator;
use crate::db::Database;
use crate::generation::ImportManager;
use crate::importer::import_html_directory;
use crate::library::{resolve_package_file, scan_books};
use crate::models::{
    AgentRuntime, AgentTask, Annotation, BackupInfo, BookAiWorkspace, BookPackage, BookSummary,
    Bootstrap, ChapterSummary, CreateAnnotation, CreateCustomAgentRuntime, CreateQuestion,
    ImportBookResponse, ImportPreflight, ImportPreflightRequest, ImportSourceKind, ImportTaskEvent,
    ImportTaskSummary, ImportedBookSummary, MoveImportTaskRequest, ParallelText,
    ResumeImportRequest, SaveProgress, SaveSetting, StartImportRequest, SwitchAgentRuntime,
    UpdateAnnotation,
};

const APP_INDEX: &str = include_str!("../../frontend/dist/index.html");
const APP_JS: &[u8] = include_bytes!("../../frontend/dist/assets/app.js");
const APP_CSS: &[u8] = include_bytes!("../../frontend/dist/assets/app.css");
const READER_JS: &[u8] = include_bytes!("../../frontend/dist/assets/reader.js");
const READER_CSS: &[u8] = include_bytes!("../../frontend/dist/assets/reader.css");

#[derive(Clone)]
pub struct AppState {
    books_dir: std::path::PathBuf,
    catalog: Arc<RwLock<crate::models::Catalog>>,
    database: Arc<Database>,
    agent: Arc<AgentCoordinator>,
    imports: Arc<ImportManager>,
    session: Arc<String>,
    nonce: Arc<String>,
    origin: Arc<String>,
}

pub struct ServerHandle {
    pub bootstrap_url: String,
    pub origin: String,
    pub shutdown: oneshot::Sender<()>,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: error.to_string(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({
                "error": self.message
            })),
        )
            .into_response()
    }
}

pub async fn start(
    books_dir: std::path::PathBuf,
    agent_tasks_dir: std::path::PathBuf,
    database: Arc<Database>,
) -> Result<ServerHandle> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("无法绑定本地回环服务")?;
    let address = listener.local_addr()?;
    let origin = format!("http://{address}");
    let session = session_token();
    let nonce = Alphanumeric.sample_string(&mut rand::rng(), 32);
    let import_tasks_dir = agent_tasks_dir
        .parent()
        .unwrap_or(&agent_tasks_dir)
        .join("ImportTasks");
    let agent = Arc::new(AgentCoordinator::new(agent_tasks_dir, database.clone())?);
    let imports = Arc::new(ImportManager::new(
        import_tasks_dir,
        books_dir.clone(),
        agent.clone(),
    )?);
    let state = AppState {
        catalog: Arc::new(RwLock::new(scan_books(&books_dir))),
        books_dir,
        database,
        agent,
        imports,
        session: Arc::new(session.clone()),
        nonce: Arc::new(nonce),
        origin: Arc::new(origin.clone()),
    };

    let protected = Router::new()
        .route("/assets/app.js", get(app_js))
        .route("/assets/app.css", get(app_css))
        .route("/runtime/reader.js", get(reader_js))
        .route("/runtime/reader.css", get(reader_css))
        .route("/api/bootstrap", get(bootstrap))
        .route("/api/rescan", post(rescan))
        .route("/api/import-book", post(import_book))
        .route("/api/import/preflight", post(import_preflight))
        .route(
            "/api/import/tasks",
            get(list_import_tasks).post(start_import_task),
        )
        .route("/api/import/tasks/:task_id", get(get_import_task))
        .route(
            "/api/import/tasks/:task_id/events",
            get(get_import_task_events),
        )
        .route("/api/import/tasks/:task_id/pause", post(pause_import_task))
        .route(
            "/api/import/tasks/:task_id/resume",
            post(resume_import_task),
        )
        .route(
            "/api/import/tasks/:task_id/cancel",
            post(cancel_import_task),
        )
        .route("/api/import/tasks/:task_id/move", post(move_import_task))
        .route("/api/open-library", post(open_library))
        .route("/api/open-external", post(open_external))
        .route(
            "/api/agent/runtimes",
            get(list_agent_runtimes).post(create_custom_agent_runtime),
        )
        .route(
            "/api/agent/runtimes/:runtime_id",
            delete(delete_custom_agent_runtime),
        )
        .route("/api/agent/tasks/:task_id", get(get_agent_task))
        .route("/api/agent/tasks/:task_id/retry", post(retry_agent_task))
        .route("/api/books/:book_id", get(book_detail))
        .route(
            "/api/books/:book_id/ai",
            get(book_ai_workspace).delete(clear_book_ai_workspace),
        )
        .route(
            "/api/books/:book_id/ai/questions",
            post(create_book_question),
        )
        .route(
            "/api/books/:book_id/progress",
            get(get_progress).put(save_progress),
        )
        .route(
            "/api/books/:book_id/annotations",
            get(list_annotations).post(create_annotation),
        )
        .route(
            "/api/books/:book_id/annotation-count",
            get(annotation_count),
        )
        .route("/api/books/:book_id/package", delete(delete_book_package))
        .route("/api/books/:book_id/forget", delete(forget_book))
        .route(
            "/api/books/:book_id/parallel/:chapter_id",
            get(parallel_text),
        )
        .route(
            "/api/annotations/:annotation_id",
            put(update_note).delete(delete_annotation),
        )
        .route("/api/backups", get(list_backups).post(create_backup))
        .route("/api/backups/:name/restore", post(restore_backup))
        .route("/api/settings/:key", get(get_setting).put(save_setting))
        .route("/books/:book_id/*relative", get(book_asset))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ));

    let router = Router::new()
        .route("/", get(index))
        .merge(protected)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
        {
            eprintln!("GoodReader 本地服务异常退出：{error}");
        }
    });

    Ok(ServerHandle {
        bootstrap_url: format!("{origin}/?bootstrap={session}"),
        origin,
        shutdown: shutdown_tx,
    })
}

fn session_token() -> String {
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var("GOODREADER_E2E_SESSION") {
        if value.len() >= 16
            && value
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        {
            return value;
        }
    }
    Alphanumeric.sample_string(&mut rand::rng(), 64)
}

async fn index(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Some(candidate) = query.get("bootstrap") {
        if secure_eq(candidate, state.session.as_str()) {
            let mut response = Redirect::to("/").into_response();
            let cookie = format!(
                "gr_session={}; HttpOnly; SameSite=Strict; Path=/",
                state.session
            );
            response.headers_mut().insert(
                SET_COOKIE,
                HeaderValue::from_str(&cookie).expect("会话 Cookie 只含字母数字"),
            );
            return response;
        }
    }

    if !has_valid_cookie(&headers, state.session.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    html_response(APP_INDEX.as_bytes(), app_csp())
}

async fn require_session(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !has_valid_cookie(request.headers(), state.session.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        let origin_ok = request
            .headers()
            .get(ORIGIN)
            .and_then(|value| value.to_str().ok())
            .map(|origin| secure_eq(origin, state.origin.as_str()))
            .unwrap_or(false);
        if !origin_ok {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error":"请求来源无效"})),
            )
                .into_response();
        }
        let json_content = request
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.starts_with("application/json"))
            .unwrap_or(false);
        if !json_content {
            return (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                Json(serde_json::json!({"error":"写请求必须使用 application/json"})),
            )
                .into_response();
        }
    }
    next.run(request).await
}

async fn app_js() -> Response {
    asset_response(APP_JS, "text/javascript; charset=utf-8", app_csp())
}

async fn app_css() -> Response {
    asset_response(APP_CSS, "text/css; charset=utf-8", app_csp())
}

async fn reader_js() -> Response {
    asset_response(READER_JS, "text/javascript; charset=utf-8", app_csp())
}

async fn reader_css() -> Response {
    asset_response(READER_CSS, "text/css; charset=utf-8", app_csp())
}

async fn bootstrap(State(state): State<AppState>) -> Result<Json<Bootstrap>, ApiError> {
    Ok(Json(build_bootstrap(&state).map_err(ApiError::internal)?))
}

async fn rescan(State(state): State<AppState>) -> Result<Json<Bootstrap>, ApiError> {
    let catalog = scan_books(&state.books_dir);
    *state.catalog.write().expect("书库写锁") = catalog;
    Ok(Json(build_bootstrap(&state).map_err(ApiError::internal)?))
}

async fn import_book(State(state): State<AppState>) -> Result<Json<ImportBookResponse>, ApiError> {
    let source = tokio::task::spawn_blocking(choose_import_directory)
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    let Some(source) = source else {
        return Ok(Json(ImportBookResponse {
            cancelled: true,
            imported: None,
            bootstrap: build_bootstrap(&state).map_err(ApiError::internal)?,
        }));
    };

    let books_dir = state.books_dir.clone();
    let imported = tokio::task::spawn_blocking(move || import_html_directory(&source, &books_dir))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::bad_request)?;

    let catalog = scan_books(&state.books_dir);
    *state.catalog.write().expect("书库写锁") = catalog;
    Ok(Json(ImportBookResponse {
        cancelled: false,
        imported: Some(ImportedBookSummary {
            id: imported.id,
            title: imported.title,
            chapter_count: imported.chapter_count,
            warnings: imported.warnings,
        }),
        bootstrap: build_bootstrap(&state).map_err(ApiError::internal)?,
    }))
}

async fn import_preflight(
    State(state): State<AppState>,
    Json(input): Json<ImportPreflightRequest>,
) -> Result<Json<ImportPreflight>, ApiError> {
    let imports = state.imports.clone();
    let result = match input.kind {
        ImportSourceKind::Url => {
            let url = input
                .url
                .context("请输入在线书籍链接")
                .map_err(ApiError::bad_request)?;
            tokio::task::spawn_blocking(move || imports.preflight_url(&url))
                .await
                .map_err(ApiError::internal)?
        }
        kind @ (ImportSourceKind::Html | ImportSourceKind::Pdf) => {
            let kind_for_dialog = kind.clone();
            let source =
                tokio::task::spawn_blocking(move || choose_import_source(&kind_for_dialog))
                    .await
                    .map_err(ApiError::internal)?
                    .map_err(ApiError::internal)?;
            let Some(source) = source else {
                return Err(ApiError::bad_request("已取消选择来源"));
            };
            tokio::task::spawn_blocking(move || imports.preflight_local(kind, &source))
                .await
                .map_err(ApiError::internal)?
        }
    };
    Ok(Json(result.map_err(ApiError::bad_request)?))
}

async fn start_import_task(
    State(state): State<AppState>,
    Json(input): Json<StartImportRequest>,
) -> Result<Json<ImportTaskSummary>, ApiError> {
    Ok(Json(
        state.imports.start(input).map_err(ApiError::bad_request)?,
    ))
}

async fn list_import_tasks(
    State(state): State<AppState>,
) -> Result<Json<Vec<ImportTaskSummary>>, ApiError> {
    Ok(Json(
        state.imports.list_tasks().map_err(ApiError::internal)?,
    ))
}

async fn get_import_task(
    State(state): State<AppState>,
    AxumPath(task_id): AxumPath<String>,
) -> Result<Json<ImportTaskSummary>, ApiError> {
    let task = state
        .imports
        .task(&task_id)
        .map_err(|error| ApiError::not_found(error.to_string()))?;
    if task.status == "completed" {
        *state.catalog.write().expect("书库写锁") = scan_books(&state.books_dir);
    }
    Ok(Json(task))
}

async fn get_import_task_events(
    State(state): State<AppState>,
    AxumPath(task_id): AxumPath<String>,
    Query(query): Query<ImportTaskEventsQuery>,
) -> Result<Json<Vec<ImportTaskEvent>>, ApiError> {
    Ok(Json(
        state
            .imports
            .events_since(&task_id, query.after_seq.unwrap_or(0))
            .map_err(|error| ApiError::not_found(error.to_string()))?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportTaskEventsQuery {
    after_seq: Option<u64>,
}

async fn pause_import_task(
    State(state): State<AppState>,
    AxumPath(task_id): AxumPath<String>,
) -> Result<Json<ImportTaskSummary>, ApiError> {
    Ok(Json(
        state
            .imports
            .pause(&task_id)
            .map_err(ApiError::bad_request)?,
    ))
}

async fn resume_import_task(
    State(state): State<AppState>,
    AxumPath(task_id): AxumPath<String>,
    Json(input): Json<ResumeImportRequest>,
) -> Result<Json<ImportTaskSummary>, ApiError> {
    Ok(Json(
        state
            .imports
            .resume(&task_id, input.runtime_id.as_deref())
            .map_err(ApiError::bad_request)?,
    ))
}

async fn cancel_import_task(
    State(state): State<AppState>,
    AxumPath(task_id): AxumPath<String>,
) -> Result<Json<ImportTaskSummary>, ApiError> {
    Ok(Json(
        state
            .imports
            .cancel(&task_id)
            .map_err(ApiError::bad_request)?,
    ))
}

async fn move_import_task(
    State(state): State<AppState>,
    AxumPath(task_id): AxumPath<String>,
    Json(input): Json<MoveImportTaskRequest>,
) -> Result<Json<Vec<ImportTaskSummary>>, ApiError> {
    Ok(Json(
        state
            .imports
            .move_queued(&task_id, input.direction)
            .map_err(ApiError::bad_request)?,
    ))
}

async fn book_detail(
    State(state): State<AppState>,
    AxumPath(book_id): AxumPath<String>,
) -> Result<Json<BookSummary>, ApiError> {
    let package = book_package(&state, &book_id)?;
    let progress = state
        .database
        .progress(&book_id)
        .map_err(ApiError::internal)?;
    Ok(Json(book_summary(&package, progress)))
}

async fn list_agent_runtimes(
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentRuntime>>, ApiError> {
    Ok(Json(
        state.agent.runtimes().await.map_err(ApiError::internal)?,
    ))
}

async fn create_custom_agent_runtime(
    State(state): State<AppState>,
    Json(input): Json<CreateCustomAgentRuntime>,
) -> Result<Json<Vec<AgentRuntime>>, ApiError> {
    state
        .database
        .save_custom_agent_runtime(&input.name, &input.executable, &input.arguments)
        .map_err(ApiError::bad_request)?;
    Ok(Json(
        state.agent.runtimes().await.map_err(ApiError::internal)?,
    ))
}

async fn delete_custom_agent_runtime(
    State(state): State<AppState>,
    AxumPath(runtime_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    if !runtime_id.starts_with("custom-") {
        return Err(ApiError::bad_request("内置 Agent 运行时不能删除"));
    }
    if state
        .database
        .delete_custom_agent_runtime(&runtime_id)
        .map_err(ApiError::internal)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("自定义 Agent 运行时不存在"))
    }
}

async fn book_ai_workspace(
    State(state): State<AppState>,
    AxumPath(book_id): AxumPath<String>,
) -> Result<Json<BookAiWorkspace>, ApiError> {
    book_package(&state, &book_id)?;
    Ok(Json(BookAiWorkspace {
        book_id: book_id.clone(),
        messages: state
            .database
            .ai_messages(&book_id)
            .map_err(ApiError::internal)?,
        active_tasks: state
            .database
            .active_agent_tasks(&book_id)
            .map_err(ApiError::internal)?,
    }))
}

async fn create_book_question(
    State(state): State<AppState>,
    AxumPath(book_id): AxumPath<String>,
    Json(input): Json<CreateQuestion>,
) -> Result<(StatusCode, Json<AgentTask>), ApiError> {
    let package = book_package(&state, &book_id)?;
    let runtimes = state.agent.runtimes().await.map_err(ApiError::internal)?;
    let runtime = runtimes
        .iter()
        .find(|runtime| runtime.id == input.runtime_id)
        .ok_or_else(|| ApiError::bad_request("未知 Agent 运行时"))?;
    if !runtime.available {
        return Err(ApiError::bad_request(
            runtime
                .detail
                .as_deref()
                .unwrap_or("所选 Agent 运行时当前不可用"),
        ));
    }

    let task = state
        .database
        .create_question_task(&book_id, &input.runtime_id, &input.content)
        .map_err(ApiError::bad_request)?;
    let annotations = state
        .database
        .annotations(&book_id)
        .map_err(ApiError::internal)?;
    state
        .agent
        .start_question(package, annotations, task.clone());
    Ok((StatusCode::ACCEPTED, Json(task)))
}

async fn get_agent_task(
    State(state): State<AppState>,
    AxumPath(task_id): AxumPath<String>,
) -> Result<Json<AgentTask>, ApiError> {
    state
        .database
        .agent_task(&task_id)
        .map_err(ApiError::internal)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("Agent 任务不存在"))
}

async fn retry_agent_task(
    State(state): State<AppState>,
    AxumPath(task_id): AxumPath<String>,
    Json(input): Json<SwitchAgentRuntime>,
) -> Result<(StatusCode, Json<AgentTask>), ApiError> {
    let current = state
        .database
        .agent_task(&task_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("Agent 任务不存在"))?;
    let runtimes = state.agent.runtimes().await.map_err(ApiError::internal)?;
    let runtime = runtimes
        .iter()
        .find(|runtime| runtime.id == input.runtime_id)
        .ok_or_else(|| ApiError::bad_request("未知 Agent 运行时"))?;
    if !runtime.available {
        return Err(ApiError::bad_request(
            runtime
                .detail
                .as_deref()
                .unwrap_or("所选 Agent 运行时当前不可用"),
        ));
    }

    let package = book_package(&state, &current.book_id)?;
    let annotations = state
        .database
        .annotations(&current.book_id)
        .map_err(ApiError::internal)?;
    let task = state
        .database
        .retry_agent_task(&task_id, &input.runtime_id)
        .map_err(ApiError::bad_request)?;
    state
        .agent
        .start_question(package, annotations, task.clone());
    Ok((StatusCode::ACCEPTED, Json(task)))
}

async fn clear_book_ai_workspace(
    State(state): State<AppState>,
    AxumPath(book_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    book_package(&state, &book_id)?;
    state
        .database
        .clear_ai_workspace(&book_id)
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_progress(
    State(state): State<AppState>,
    AxumPath(book_id): AxumPath<String>,
) -> Result<Json<Option<crate::models::Progress>>, ApiError> {
    book_package(&state, &book_id)?;
    Ok(Json(
        state
            .database
            .progress(&book_id)
            .map_err(ApiError::internal)?,
    ))
}

async fn save_progress(
    State(state): State<AppState>,
    AxumPath(book_id): AxumPath<String>,
    Json(input): Json<SaveProgress>,
) -> Result<Json<crate::models::Progress>, ApiError> {
    let package = book_package(&state, &book_id)?;
    if !package
        .manifest
        .chapters
        .iter()
        .any(|chapter| chapter.id == input.chapter_id)
    {
        return Err(ApiError::bad_request("进度引用了未知章节"));
    }
    Ok(Json(
        state
            .database
            .save_progress(&book_id, &input)
            .map_err(ApiError::bad_request)?,
    ))
}

async fn list_annotations(
    State(state): State<AppState>,
    AxumPath(book_id): AxumPath<String>,
) -> Result<Json<Vec<Annotation>>, ApiError> {
    book_package(&state, &book_id)?;
    Ok(Json(
        state
            .database
            .annotations(&book_id)
            .map_err(ApiError::internal)?,
    ))
}

async fn create_annotation(
    State(state): State<AppState>,
    AxumPath(book_id): AxumPath<String>,
    Json(input): Json<CreateAnnotation>,
) -> Result<Json<Annotation>, ApiError> {
    let package = book_package(&state, &book_id)?;
    if !package
        .manifest
        .chapters
        .iter()
        .any(|chapter| chapter.id == input.chapter_id)
    {
        return Err(ApiError::bad_request("标注引用了未知章节"));
    }
    Ok(Json(
        state
            .database
            .create_annotation(&book_id, &input)
            .map_err(ApiError::bad_request)?,
    ))
}

async fn update_note(
    State(state): State<AppState>,
    AxumPath(annotation_id): AxumPath<String>,
    Json(input): Json<UpdateAnnotation>,
) -> Result<Json<Annotation>, ApiError> {
    Ok(Json(
        state
            .database
            .update_note(&annotation_id, &input.note)
            .map_err(ApiError::bad_request)?,
    ))
}

async fn delete_annotation(
    State(state): State<AppState>,
    AxumPath(annotation_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    if state
        .database
        .delete_annotation(&annotation_id)
        .map_err(ApiError::internal)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("标注不存在"))
    }
}

async fn annotation_count(
    State(state): State<AppState>,
    AxumPath(book_id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let count = state
        .database
        .annotation_count(&book_id)
        .map_err(ApiError::internal)?;
    Ok(Json(serde_json::json!({ "count": count })))
}

async fn delete_book_package(
    State(state): State<AppState>,
    AxumPath(book_id): AxumPath<String>,
) -> Result<Json<Bootstrap>, ApiError> {
    let package = book_package(&state, &book_id)?;
    let books_dir = state.books_dir.clone();
    tokio::task::spawn_blocking(move || move_book_package_to_trash(&books_dir, &package.root))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;

    let catalog = scan_books(&state.books_dir);
    *state.catalog.write().expect("书库写锁") = catalog;
    Ok(Json(build_bootstrap(&state).map_err(ApiError::internal)?))
}

async fn forget_book(
    State(state): State<AppState>,
    AxumPath(book_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    state
        .database
        .forget_book(&book_id)
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn parallel_text(
    State(state): State<AppState>,
    AxumPath((book_id, chapter_id)): AxumPath<(String, String)>,
) -> Result<Json<ParallelText>, ApiError> {
    let package = book_package(&state, &book_id)?;
    let chapter = package
        .manifest
        .chapters
        .iter()
        .find(|chapter| chapter.id == chapter_id)
        .ok_or_else(|| ApiError::not_found("章节不存在"))?;
    let relative = chapter
        .parallel_text
        .as_deref()
        .ok_or_else(|| ApiError::not_found("本章没有对照文本"))?;
    let path = resolve_package_file(&package.root, relative).map_err(ApiError::bad_request)?;
    let text = fs::read_to_string(&path).map_err(ApiError::internal)?;
    let parallel = serde_json::from_str(&text).map_err(ApiError::bad_request)?;
    Ok(Json(parallel))
}

async fn book_asset(
    State(state): State<AppState>,
    AxumPath((book_id, relative)): AxumPath<(String, String)>,
) -> Result<Response, ApiError> {
    let package = book_package(&state, &book_id)?;
    let path = resolve_package_file(&package.root, &relative).map_err(ApiError::bad_request)?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if matches!(extension.as_str(), "html" | "htm") {
        let html = fs::read_to_string(&path).map_err(ApiError::internal)?;
        let chapter_id = package
            .manifest
            .chapters
            .iter()
            .find_map(|chapter| {
                resolve_package_file(&package.root, &chapter.path)
                    .ok()
                    .filter(|chapter_path| chapter_path == &path)
                    .map(|_| chapter.id.as_str())
            })
            .unwrap_or_default();
        let injected = inject_reader(
            &html,
            &package.manifest.id,
            chapter_id,
            state.nonce.as_str(),
        );
        return Ok(html_response(
            injected.as_bytes(),
            book_csp(state.nonce.as_str()),
        ));
    }

    let bytes = fs::read(&path).map_err(ApiError::internal)?;
    Ok(asset_response(
        &bytes,
        content_type_for(&extension),
        book_csp(state.nonce.as_str()),
    ))
}

async fn open_library(State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    Command::new("open")
        .arg(&state.books_dir)
        .spawn()
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

fn choose_import_directory() -> Result<Option<PathBuf>> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("GOODREADER_IMPORT_SOURCE") {
        return Ok(Some(PathBuf::from(path)));
    }

    let output = Command::new("osascript")
        .args([
            "-e",
            r#"POSIX path of (choose folder with prompt "选择要转换并导入的 HTML 书籍目录")"#,
        ])
        .output()
        .context("无法打开 macOS 文件夹选择器")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("(-128)") || error.contains("User canceled") {
            return Ok(None);
        }
        bail!("文件夹选择器失败：{}", error.trim());
    }

    let path = String::from_utf8(output.stdout)
        .context("文件夹选择器返回了无效路径")?
        .trim()
        .to_string();
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(path)))
}

fn choose_import_source(kind: &ImportSourceKind) -> Result<Option<PathBuf>> {
    match kind {
        ImportSourceKind::Html => choose_import_directory(),
        ImportSourceKind::Pdf => choose_import_pdf_file(),
        ImportSourceKind::Url => bail!("在线来源不使用文件选择器"),
    }
}

fn choose_import_pdf_file() -> Result<Option<PathBuf>> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("GOODREADER_IMPORT_PDF") {
        return Ok(Some(PathBuf::from(path)));
    }

    let output = Command::new("osascript")
        .args([
            "-e",
            r#"POSIX path of (choose file with prompt "选择要生成书籍的 PDF" of type {"com.adobe.pdf"})"#,
        ])
        .output()
        .context("无法打开 macOS PDF 选择器")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("(-128)") || error.contains("User canceled") {
            return Ok(None);
        }
        bail!("PDF 选择器失败：{}", error.trim());
    }
    let path = String::from_utf8(output.stdout)
        .context("PDF 选择器返回了无效路径")?
        .trim()
        .to_string();
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}

fn move_book_package_to_trash(books_dir: &Path, package_root: &Path) -> Result<()> {
    let package_root = deletion_target(books_dir, package_root)?;
    let script = r#"
ObjC.import('Foundation');
function run(argv) {
  const url = $.NSURL.fileURLWithPath(argv[0]);
  const resultingUrl = Ref();
  const error = Ref();
  const moved = $.NSFileManager.defaultManager
    .trashItemAtURLResultingItemURLError(url, resultingUrl, error);
  if (!moved) {
    const message = error[0]
      ? ObjC.unwrap(error[0].localizedDescription)
      : '未知错误';
    throw new Error(message);
  }
}
"#;
    let output = Command::new("osascript")
        .args(["-l", "JavaScript", "-e", script])
        .arg(&package_root)
        .output()
        .context("无法调用 macOS 废纸篓服务")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!("无法将书籍移到废纸篓：{}", error.trim());
    }
    Ok(())
}

fn deletion_target(books_dir: &Path, package_root: &Path) -> Result<PathBuf> {
    let books_dir = books_dir
        .canonicalize()
        .with_context(|| format!("无法解析书库目录 {}", books_dir.display()))?;
    let package_root = package_root
        .canonicalize()
        .with_context(|| format!("无法解析书籍目录 {}", package_root.display()))?;
    if package_root.parent() != Some(books_dir.as_path()) || !package_root.is_dir() {
        bail!("只允许删除 GoodReader 书库中的直接子目录");
    }
    Ok(package_root)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalUrl {
    url: String,
}

async fn open_external(Json(input): Json<ExternalUrl>) -> Result<StatusCode, ApiError> {
    let url = input
        .url
        .parse::<axum::http::Uri>()
        .map_err(ApiError::bad_request)?;
    if !matches!(url.scheme_str(), Some("http" | "https")) {
        return Err(ApiError::bad_request("只允许打开 HTTP 或 HTTPS 外链"));
    }
    Command::new("open")
        .arg(&input.url)
        .spawn()
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_backups(State(state): State<AppState>) -> Result<Json<Vec<BackupInfo>>, ApiError> {
    Ok(Json(
        state.database.list_backups().map_err(ApiError::internal)?,
    ))
}

async fn create_backup(State(state): State<AppState>) -> Result<Json<BackupInfo>, ApiError> {
    Ok(Json(
        state
            .database
            .create_backup("manual")
            .map_err(ApiError::internal)?,
    ))
}

async fn restore_backup(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    state
        .database
        .restore_backup(&name)
        .map_err(ApiError::bad_request)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_setting(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_setting_key(&key)?;
    let value = state.database.setting(&key).map_err(ApiError::internal)?;
    Ok(Json(serde_json::json!({ "value": value })))
}

async fn save_setting(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
    Json(input): Json<SaveSetting>,
) -> Result<StatusCode, ApiError> {
    validate_setting_key(&key)?;
    if input.value.len() > 256 {
        return Err(ApiError::bad_request("设置值过长"));
    }
    state
        .database
        .save_setting(&key, &input.value)
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

fn build_bootstrap(state: &AppState) -> Result<Bootstrap> {
    let progress = state
        .database
        .all_progress()?
        .into_iter()
        .map(|progress| (progress.book_id.clone(), progress))
        .collect::<HashMap<_, _>>();
    let catalog = state.catalog.read().expect("书库读锁");
    let books = catalog
        .books
        .values()
        .map(|package| book_summary(package, progress.get(&package.manifest.id).cloned()))
        .collect();
    Ok(Bootstrap {
        books,
        issues: catalog.issues.clone(),
        library_path: state.books_dir.display().to_string(),
    })
}

fn book_summary(package: &BookPackage, progress: Option<crate::models::Progress>) -> BookSummary {
    let id = &package.manifest.id;
    BookSummary {
        id: id.clone(),
        title: package.manifest.title.clone(),
        original_title: package.manifest.original_title.clone(),
        author: package.manifest.author.clone(),
        language: package.manifest.language.clone(),
        cover_url: format!("/books/{id}/{}", package.manifest.cover),
        entry_url: format!("/books/{id}/{}", package.manifest.entry),
        chapters: package
            .manifest
            .chapters
            .iter()
            .map(|chapter| ChapterSummary {
                id: chapter.id.clone(),
                title: chapter.title.clone(),
                url: format!("/books/{id}/{}", chapter.path),
                has_parallel_text: chapter.parallel_text.is_some(),
            })
            .collect(),
        progress,
    }
}

fn book_package(state: &AppState, book_id: &str) -> Result<BookPackage, ApiError> {
    state
        .catalog
        .read()
        .expect("书库读锁")
        .books
        .get(book_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("书籍不存在或当前不可用"))
}

fn inject_reader(html: &str, book_id: &str, chapter_id: &str, nonce: &str) -> String {
    let head = r#"<link rel="stylesheet" href="/runtime/reader.css">"#;
    let script = format!(
        r#"<script nonce="{nonce}" defer src="/runtime/reader.js" data-goodreader-book="{book_id}" data-goodreader-chapter="{chapter_id}"></script>"#
    );
    let with_head = if let Some(index) = html.to_ascii_lowercase().find("</head>") {
        format!("{}{}{}", &html[..index], head, &html[index..])
    } else {
        format!("{head}{html}")
    };
    if let Some(index) = with_head.to_ascii_lowercase().rfind("</body>") {
        format!("{}{}{}", &with_head[..index], script, &with_head[index..])
    } else {
        format!("{with_head}{script}")
    }
}

fn html_response(bytes: &[u8], csp: String) -> Response {
    asset_response_with_cache(bytes, "text/html; charset=utf-8", csp, "no-store")
}

fn asset_response(bytes: &[u8], content_type: &str, csp: String) -> Response {
    asset_response_with_cache(bytes, content_type, csp, "private, max-age=3600")
}

fn asset_response_with_cache(
    bytes: &[u8],
    content_type: &str,
    csp: String,
    cache: &str,
) -> Response {
    let mut response = Response::new(Body::from(bytes.to_vec()));
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(content_type).expect("固定 Content-Type"),
    );
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&csp).expect("固定 CSP"),
    );
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_str(cache).expect("固定缓存策略"),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

fn app_csp() -> String {
    "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
     connect-src 'self'; object-src 'none'; frame-src 'none'; base-uri 'none'; form-action 'none'"
        .to_string()
}

fn book_csp(nonce: &str) -> String {
    format!(
        "default-src 'self'; script-src 'nonce-{nonce}'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; font-src 'self' data:; connect-src 'self'; \
         object-src 'none'; frame-src 'none'; base-uri 'none'; form-action 'none'"
    )
}

fn content_type_for(extension: &str) -> &'static str {
    match extension {
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn has_valid_cookie(headers: &HeaderMap, session: &str) -> bool {
    headers
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == "gr_session").then_some(value)
            })
        })
        .map(|value| secure_eq(value, session))
        .unwrap_or(false)
}

fn secure_eq(left: &str, right: &str) -> bool {
    left.len() == right.len() && left.as_bytes().ct_eq(right.as_bytes()).into()
}

fn validate_setting_key(key: &str) -> Result<(), ApiError> {
    if matches!(
        key,
        "highlight-color" | "annotation-filter" | "reader-theme" | "ai-send-key" | "topbar-pinned"
    ) {
        Ok(())
    } else {
        Err(ApiError::bad_request("不支持的设置项"))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::{
        book_csp, book_summary, deletion_target, inject_reader, secure_eq, validate_setting_key,
    };
    use crate::models::{BookManifest, BookPackage, ChapterManifest};

    #[test]
    fn injects_only_the_goodreader_runtime() {
        let html = "<html><head><title>书</title></head><body><p>正文</p></body></html>";
        let injected = inject_reader(html, "book", "ch1", "nonce");
        assert!(injected.contains("/runtime/reader.css"));
        assert!(injected.contains("data-goodreader-book=\"book\""));
        assert!(injected.contains("nonce=\"nonce\""));
        assert!(injected.find("/runtime/reader.js").unwrap() < injected.find("</body>").unwrap());
    }

    #[test]
    fn csp_allows_only_the_injected_nonce() {
        let csp = book_csp("abc");
        assert!(csp.contains("script-src 'nonce-abc'"));
        assert!(!csp.contains("'unsafe-eval'"));
    }

    #[test]
    fn constant_time_comparison_checks_length_and_value() {
        assert!(secure_eq("abc", "abc"));
        assert!(!secure_eq("abc", "abd"));
        assert!(!secure_eq("abc", "abcd"));
    }

    #[test]
    fn deletion_only_accepts_a_direct_child_of_the_library() {
        let root = tempdir().unwrap();
        let books = root.path().join("Books");
        let book = books.join("book");
        let nested = book.join("nested");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            deletion_target(&books, &book).unwrap(),
            book.canonicalize().unwrap()
        );
        assert!(deletion_target(&books, &nested).is_err());
        assert!(deletion_target(&books, root.path()).is_err());
    }

    #[test]
    fn reader_theme_is_an_allowed_setting() {
        assert!(validate_setting_key("reader-theme").is_ok());
        assert!(validate_setting_key("ai-send-key").is_ok());
        assert!(validate_setting_key("topbar-pinned").is_ok());
        assert!(validate_setting_key("unknown-theme").is_err());
    }

    #[test]
    fn chapter_summary_exposes_available_parallel_text() {
        let package = BookPackage {
            root: PathBuf::new(),
            manifest: BookManifest {
                schema_version: 1,
                id: "4b68c6b0-f3ad-4472-96b5-3d38d774aeef".to_string(),
                title: "双语书".to_string(),
                original_title: None,
                author: "作者".to_string(),
                language: Some("zh-CN".to_string()),
                source_language: None,
                target_language: None,
                cover: "cover.png".to_string(),
                entry: "index.html".to_string(),
                chapters: vec![ChapterManifest {
                    id: "chapter-0001".to_string(),
                    title: "第一章".to_string(),
                    path: "chapters/ch1.html".to_string(),
                    parallel_text: Some("parallel/chapter-0001.en.json".to_string()),
                }],
            },
        };

        let summary = book_summary(&package, None);
        assert_eq!(summary.language.as_deref(), Some("zh-CN"));
        assert!(summary.chapters[0].has_parallel_text);
    }
}
