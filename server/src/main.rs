use std::{
    env,
    net::SocketAddr,
    panic,
    path::{Path as StdPath, PathBuf},
    str::FromStr,
    sync::Arc,
};

use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    FromRow, SqlitePool,
};
use tokio::{fs, io::AsyncWriteExt};
use tower_http::{cors::CorsLayer, services::ServeDir, trace::TraceLayer};
use tracing::{error, info};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    data_dir: PathBuf,
    max_chunk_bytes: usize,
    web_dir: PathBuf,
}

#[derive(Deserialize)]
struct CreateSessionReq {
    mime_type: Option<String>,
}

#[derive(Serialize)]
struct CreateSessionResp {
    session_id: String,
    secret_path: String,
    upload_base: String,
    file_path: String,
    delete_path: String,
}

#[derive(Deserialize)]
struct UploadChunkReq {
    idx: u32,
    data_b64: String,
}

#[derive(Serialize)]
struct UploadChunkResp {
    ok: bool,
    idx: u32,
    already_present: bool,
    sha256_hex: String,
}

#[derive(Serialize, FromRow)]
struct SessionRow {
    id: String,
    status: String,
    mime_type: Option<String>,
    created_at: String,
    completed_at: Option<String>,
    output_path: Option<String>,
    secret_token: Option<String>,
}

#[derive(Deserialize)]
struct FinalizeReq {
    extension: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    if let Err(err) = run().await {
        error!(error = %err, "recorder-server fatal error");
        eprintln!("recorder-server fatal error: {err:#}");
        return Err(err);
    }

    Ok(())
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "recorder_server=info,tower_http=info".into()),
        )
        .with_writer(std::io::stdout)
        .with_ansi(false)
        .init();

    panic::set_hook(Box::new(|panic_info| {
        eprintln!("panic: {panic_info}");
        eprintln!("backtrace (enable with RUST_BACKTRACE=1)");
    }));
}

fn normalize_path(input: &str) -> Result<PathBuf> {
    let p = PathBuf::from(input);
    if p.is_absolute() {
        return Ok(p);
    }

    let cwd = env::current_dir().context("resolving current working directory")?;
    Ok(cwd.join(p))
}

fn sqlite_url_from_path(path: &StdPath) -> String {
    format!("sqlite://{}", path.to_string_lossy())
}

async fn run() -> Result<()> {
    let data_dir =
        normalize_path(&env::var("APP_DATA_DIR").unwrap_or_else(|_| "./data".to_string()))?;
    let db_path =
        normalize_path(&env::var("APP_DB_PATH").unwrap_or_else(|_| "./data/app.db".to_string()))?;
    let max_chunk_bytes = env::var("APP_MAX_CHUNK_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(10 * 1024 * 1024);
    let web_dir = normalize_path(&env::var("APP_WEB_DIR").unwrap_or_else(|_| "./web".to_string()))?;

    info!(
        data_dir = %data_dir.display(),
        db_path = %db_path.display(),
        max_chunk_bytes,
        web_dir = %web_dir.display(),
        "starting recorder-server"
    );

    fs::create_dir_all(&data_dir)
        .await
        .with_context(|| format!("creating data dir {:?}", data_dir))?;

    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating db parent dir {:?}", parent))?;
    }

    let db_url = sqlite_url_from_path(&db_path);
    let db_connect_options = SqliteConnectOptions::from_str(&db_url)
        .with_context(|| format!("parsing sqlite url {db_url}"))?
        .create_if_missing(true);
    let db = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(db_connect_options)
        .await
        .with_context(|| format!("connecting sqlite {db_url}"))?;

    migrate(&db).await?;

    let state = Arc::new(AppState {
        db,
        data_dir,
        max_chunk_bytes,
        web_dir,
    });

    let app = Router::new()
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/sessions", post(create_session))
        .route(
            "/api/sessions/{id}",
            get(get_session).delete(delete_session_by_id),
        )
        .route("/api/sessions/{id}/chunks", post(upload_chunk))
        .route("/api/sessions/{id}/finalize", post(finalize_session))
        .route("/api/sessions/{id}/file", get(download_file_legacy))
        .route("/api/r/{token}/chunks", post(upload_chunk_by_token))
        .route("/api/r/{token}/finalize", post(finalize_by_token))
        .route("/api/r/{token}", delete(delete_by_token))
        .route("/r/{token}/file", get(download_file_by_token))
        .route("/r/{token}", get(secret_recorder_page))
        .route("/r/{token}/", get(secret_recorder_page))
        .nest_service(
            "/",
            ServeDir::new(state.web_dir.clone()).append_index_html_on_directories(true),
        )
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    info!("recorder-server listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .await
        .context("axum server terminated unexpectedly")?;
    Ok(())
}

async fn migrate(db: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            status TEXT NOT NULL,
            mime_type TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            completed_at TEXT,
            output_path TEXT,
            secret_token TEXT
        );
        "#,
    )
    .execute(db)
    .await?;

    let alter_result = sqlx::query("ALTER TABLE sessions ADD COLUMN secret_token TEXT")
        .execute(db)
        .await;
    if let Err(err) = alter_result {
        let msg = err.to_string().to_lowercase();
        if !msg.contains("duplicate") && !msg.contains("already exists") {
            return Err(err.into());
        }
    }

    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_secret_token ON sessions(secret_token)",
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS chunks (
            session_id TEXT NOT NULL,
            idx INTEGER NOT NULL,
            byte_len INTEGER NOT NULL,
            sha256_hex TEXT NOT NULL,
            file_path TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (session_id, idx)
        );
        "#,
    )
    .execute(db)
    .await?;

    Ok(())
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateSessionReq>,
) -> Result<Json<CreateSessionResp>, (StatusCode, String)> {
    let session_id = Uuid::new_v4().to_string();
    let secret_token = format!(
        "{}{}",
        Uuid::new_v4().as_simple(),
        Uuid::new_v4().as_simple()
    );

    sqlx::query(
        "INSERT INTO sessions (id, status, mime_type, secret_token) VALUES (?, 'recording', ?, ?)",
    )
    .bind(&session_id)
    .bind(req.mime_type)
    .bind(&secret_token)
    .execute(&state.db)
    .await
    .map_err(internal_err)?;

    let session_dir = state.data_dir.join("chunks").join(&session_id);
    fs::create_dir_all(&session_dir)
        .await
        .map_err(internal_err)?;

    Ok(Json(CreateSessionResp {
        session_id,
        secret_path: format!("/r/{secret_token}/"),
        upload_base: format!("/api/r/{secret_token}"),
        file_path: format!("/r/{secret_token}/file"),
        delete_path: format!("/api/r/{secret_token}"),
    }))
}

async fn get_session(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<SessionRow>, (StatusCode, String)> {
    let row = sqlx::query_as::<_, SessionRow>(
        "SELECT id, status, mime_type, created_at, completed_at, output_path, secret_token FROM sessions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(internal_err)?
    .ok_or((StatusCode::NOT_FOUND, "session not found".to_string()))?;

    Ok(Json(row))
}

async fn upload_chunk(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<UploadChunkReq>,
) -> Result<Json<UploadChunkResp>, (StatusCode, String)> {
    handle_upload_for_session(id, state, req).await
}

async fn upload_chunk_by_token(
    Path(token): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<UploadChunkReq>,
) -> Result<Json<UploadChunkResp>, (StatusCode, String)> {
    let session_id = find_session_id_by_token(&state.db, &token).await?;
    handle_upload_for_session(session_id, state, req).await
}

async fn handle_upload_for_session(
    session_id: String,
    state: Arc<AppState>,
    req: UploadChunkReq,
) -> Result<Json<UploadChunkResp>, (StatusCode, String)> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(req.data_b64)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid base64: {e}")))?;

    if bytes.len() > state.max_chunk_bytes {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "chunk too large for server limit".to_string(),
        ));
    }

    let sha256_hex = format!("{:x}", Sha256::digest(&bytes));

    let existing: Option<(String,)> =
        sqlx::query_as("SELECT sha256_hex FROM chunks WHERE session_id = ? AND idx = ?")
            .bind(&session_id)
            .bind(req.idx as i64)
            .fetch_optional(&state.db)
            .await
            .map_err(internal_err)?;

    if let Some((existing_sha,)) = existing {
        return Ok(Json(UploadChunkResp {
            ok: true,
            idx: req.idx,
            already_present: true,
            sha256_hex: existing_sha,
        }));
    }

    let chunk_path = state
        .data_dir
        .join("chunks")
        .join(&session_id)
        .join(format!("{:08}.chunk", req.idx));
    if let Some(parent) = chunk_path.parent() {
        fs::create_dir_all(parent).await.map_err(internal_err)?;
    }

    let mut f = fs::File::create(&chunk_path).await.map_err(internal_err)?;
    f.write_all(&bytes).await.map_err(internal_err)?;
    f.flush().await.map_err(internal_err)?;

    sqlx::query(
        "INSERT INTO chunks (session_id, idx, byte_len, sha256_hex, file_path) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&session_id)
    .bind(req.idx as i64)
    .bind(bytes.len() as i64)
    .bind(&sha256_hex)
    .bind(chunk_path.to_string_lossy().to_string())
    .execute(&state.db)
    .await
    .map_err(internal_err)?;

    Ok(Json(UploadChunkResp {
        ok: true,
        idx: req.idx,
        already_present: false,
        sha256_hex,
    }))
}

async fn finalize_session(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<FinalizeReq>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    finalize_by_session_id(id, state, req).await
}

async fn finalize_by_token(
    Path(token): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<FinalizeReq>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let session_id = find_session_id_by_token(&state.db, &token).await?;
    finalize_by_session_id(session_id, state, req).await
}

async fn finalize_by_session_id(
    id: String,
    state: Arc<AppState>,
    req: FinalizeReq,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let ext = req.extension.unwrap_or_else(|| "webm".to_string());
    let out_path = state.data_dir.join("final").join(format!("{id}.{ext}"));

    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).await.map_err(internal_err)?;
    }

    let chunks: Vec<(i64, String)> =
        sqlx::query_as("SELECT idx, file_path FROM chunks WHERE session_id = ? ORDER BY idx ASC")
            .bind(&id)
            .fetch_all(&state.db)
            .await
            .map_err(internal_err)?;

    if chunks.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no chunks uploaded".to_string()));
    }

    let mut out = fs::File::create(&out_path).await.map_err(internal_err)?;
    for (_, file_path) in chunks {
        let b = fs::read(file_path).await.map_err(internal_err)?;
        out.write_all(&b).await.map_err(internal_err)?;
    }
    out.flush().await.map_err(internal_err)?;

    sqlx::query(
        "UPDATE sessions SET status = 'finalized', completed_at = datetime('now'), output_path = ? WHERE id = ?",
    )
    .bind(out_path.to_string_lossy().to_string())
    .bind(&id)
    .execute(&state.db)
    .await
    .map_err(internal_err)?;

    let secret_token = find_secret_token_by_session_id(&state.db, &id).await?;
    Ok((StatusCode::OK, format!("finalized: /r/{secret_token}/file")))
}

async fn download_file_legacy(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, (StatusCode, String)> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT output_path FROM sessions WHERE id = ?")
            .bind(&id)
            .fetch_optional(&state.db)
            .await
            .map_err(internal_err)?;

    let output_path = row.and_then(|(path,)| path).ok_or((
        StatusCode::NOT_FOUND,
        "finalized file not found".to_string(),
    ))?;

    build_audio_response(&id, output_path).await
}

async fn download_file_by_token(
    Path(token): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, (StatusCode, String)> {
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT id, output_path FROM sessions WHERE secret_token = ?")
            .bind(&token)
            .fetch_optional(&state.db)
            .await
            .map_err(internal_err)?;

    let (session_id, output_path) =
        row.ok_or((StatusCode::NOT_FOUND, "recording not found".to_string()))?;
    let output_path = output_path.ok_or((
        StatusCode::NOT_FOUND,
        "finalized file not found".to_string(),
    ))?;

    build_audio_response(&session_id, output_path).await
}

async fn build_audio_response(
    session_id: &str,
    output_path: String,
) -> Result<Response, (StatusCode, String)> {
    let data = fs::read(output_path).await.map_err(internal_err)?;

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "audio/webm")
        .header(
            "content-disposition",
            format!("inline; filename=\"{session_id}.webm\""),
        )
        .body(axum::body::Body::from(data))
        .map_err(internal_err)
}

async fn delete_by_token(
    Path(token): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let session_id = find_session_id_by_token(&state.db, &token).await?;
    delete_session_files_and_metadata(session_id, state).await
}

async fn delete_session_by_id(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    delete_session_files_and_metadata(id, state).await
}

async fn delete_session_files_and_metadata(
    session_id: String,
    state: Arc<AppState>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT output_path FROM sessions WHERE id = ?")
            .bind(&session_id)
            .fetch_optional(&state.db)
            .await
            .map_err(internal_err)?;

    if row.is_none() {
        return Err((StatusCode::NOT_FOUND, "session not found".to_string()));
    }

    let output_path = row.and_then(|(path,)| path);

    let chunk_rows: Vec<(String,)> =
        sqlx::query_as("SELECT file_path FROM chunks WHERE session_id = ?")
            .bind(&session_id)
            .fetch_all(&state.db)
            .await
            .map_err(internal_err)?;

    for (chunk_path,) in chunk_rows {
        let _ = fs::remove_file(chunk_path).await;
    }

    let chunk_dir = state.data_dir.join("chunks").join(&session_id);
    let _ = fs::remove_dir_all(chunk_dir).await;

    if let Some(path) = output_path {
        let _ = fs::remove_file(path).await;
    }

    sqlx::query("DELETE FROM chunks WHERE session_id = ?")
        .bind(&session_id)
        .execute(&state.db)
        .await
        .map_err(internal_err)?;

    sqlx::query("DELETE FROM sessions WHERE id = ?")
        .bind(&session_id)
        .execute(&state.db)
        .await
        .map_err(internal_err)?;

    Ok((StatusCode::OK, "deleted"))
}

async fn secret_recorder_page(
    Path(_token): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Html<String>, (StatusCode, String)> {
    let page = fs::read_to_string(state.web_dir.join("index.html"))
        .await
        .map_err(internal_err)?;
    Ok(Html(page))
}

async fn find_session_id_by_token(
    db: &SqlitePool,
    token: &str,
) -> Result<String, (StatusCode, String)> {
    let row: Option<(String,)> = sqlx::query_as("SELECT id FROM sessions WHERE secret_token = ?")
        .bind(token)
        .fetch_optional(db)
        .await
        .map_err(internal_err)?;

    row.map(|(id,)| id)
        .ok_or((StatusCode::NOT_FOUND, "session not found".to_string()))
}

async fn find_secret_token_by_session_id(
    db: &SqlitePool,
    session_id: &str,
) -> Result<String, (StatusCode, String)> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT secret_token FROM sessions WHERE id = ?")
            .bind(session_id)
            .fetch_optional(db)
            .await
            .map_err(internal_err)?;

    row.and_then(|(token,)| token)
        .ok_or((StatusCode::NOT_FOUND, "secret token not found".to_string()))
}

fn internal_err<E: std::fmt::Display>(err: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}
