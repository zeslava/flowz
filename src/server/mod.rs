use crate::{
    model::{Repo, Run, RunStatus, StepRecord, StepStatus},
    store::Store,
    webhook::{self, WebhookError},
};
use anyhow::Result;
use askama::Template;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub webhook_secret: String,
    pub github_token: Option<String>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(ui_runs))
        .route("/runs/{id}", get(ui_run))
        .route("/webhook/github", post(github_webhook))
        .route("/api/jobs/poll", get(poll_job))
        .route("/api/runs/{id}/steps/{step}/logs", post(ingest_logs))
        .route("/api/runs/{id}/status", post(update_run_status))
        .route("/api/repos", get(list_repos_api).post(create_repo_api))
        .route("/api/repos/{id}", axum::routing::delete(delete_repo_api))
        .route("/api/runs", get(list_runs).post(create_run))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/logs", get(get_logs))
        .with_state(state)
}

// ── View models ───────────────────────────────────────────────────────────────

struct RunRow {
    id: String,
    id_short: String,
    repo: String,
    branch: String,
    commit_short: String,
    status: String,
    created_at: String,
}

impl From<&Run> for RunRow {
    fn from(r: &Run) -> Self {
        RunRow {
            id_short: r.id[..8.min(r.id.len())].to_string(),
            id: r.id.clone(),
            repo: r.repo.clone(),
            branch: r.branch.clone(),
            commit_short: r.commit[..7.min(r.commit.len())].to_string(),
            status: run_status_str(&r.status).to_string(),
            created_at: r.created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
        }
    }
}

struct StepRow {
    name: String,
    status: String,
    started_at: String,
    finished_at: String,
}

impl From<&StepRecord> for StepRow {
    fn from(s: &StepRecord) -> Self {
        StepRow {
            name: s.name.clone(),
            status: step_status_str(&s.status).to_string(),
            started_at: s.started_at.map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default(),
            finished_at: s.finished_at.map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default(),
        }
    }
}

struct LogGroup {
    step: String,
    status: String,
    lines: Vec<String>,
}

fn run_status_str(s: &RunStatus) -> &'static str {
    match s {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::Success => "success",
        RunStatus::Failure => "failure",
        RunStatus::Cancelled => "cancelled",
    }
}

fn step_status_str(s: &StepStatus) -> &'static str {
    match s {
        StepStatus::Pending => "pending",
        StepStatus::Running => "running",
        StepStatus::Success => "success",
        StepStatus::Failure => "failure",
        StepStatus::Skipped => "skipped",
    }
}

// ── Templates ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "runs.html")]
struct RunsTemplate {
    runs: Vec<RunRow>,
}

#[derive(Template)]
#[template(path = "run.html")]
struct RunTemplate {
    run: RunRow,
    steps: Vec<StepRow>,
    log_groups: Vec<LogGroup>,
}

fn render<T: Template>(tmpl: T) -> axum::response::Response {
    match tmpl.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── GET / ─────────────────────────────────────────────────────────────────────

async fn ui_runs(State(state): State<AppState>) -> axum::response::Response {
    let runs = match state.store.list_runs(50).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("ui list runs: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    render(RunsTemplate { runs: runs.iter().map(RunRow::from).collect() })
}

// ── GET /runs/:id ─────────────────────────────────────────────────────────────

async fn ui_run(State(state): State<AppState>, Path(run_id): Path<String>) -> axum::response::Response {
    let run = match state.store.get_run(&run_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => {
            tracing::error!("ui get run: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    let steps = match state.store.list_steps(&run_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("ui list steps: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    let raw_logs = match state.store.get_logs(&run_id).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("ui get logs: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };

    let step_status_map: std::collections::HashMap<String, String> = steps
        .iter()
        .map(|s| (s.name.clone(), step_status_str(&s.status).to_string()))
        .collect();

    let mut log_groups: Vec<LogGroup> = Vec::new();
    for (step, line) in raw_logs {
        if let Some(g) = log_groups.last_mut().filter(|g| g.step == step) {
            g.lines.push(line);
        } else {
            let status = step_status_map.get(&step).cloned().unwrap_or_default();
            log_groups.push(LogGroup { step, status, lines: vec![line] });
        }
    }

    render(RunTemplate {
        run: RunRow::from(&run),
        steps: steps.iter().map(StepRow::from).collect(),
        log_groups,
    })
}

// ── POST /webhook/github ──────────────────────────────────────────────────────

async fn github_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let sig = match headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s.to_string(),
        None => return (StatusCode::BAD_REQUEST, "missing signature").into_response(),
    };

    if let Err(e) = webhook::verify_signature(&state.webhook_secret, &body, &sig) {
        return (StatusCode::UNAUTHORIZED, e.to_string()).into_response();
    }

    let event = match webhook::parse_push(&body) {
        Ok(e) => e,
        Err(WebhookError::NotPushEvent) => {
            return (StatusCode::OK, "ignored").into_response();
        }
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };

    let now = Utc::now();
    let run = Run {
        id: Uuid::new_v4().to_string(),
        repo: event.repo,
        branch: event.branch,
        commit: event.commit,
        clone_url: event.clone_url,
        status: RunStatus::Queued,
        created_at: now,
        updated_at: now,
    };

    if let Err(e) = state.store.create_run(&run).await {
        tracing::error!("create run: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
    }

    tracing::info!(run_id = %run.id, repo = %run.repo, branch = %run.branch, "queued run");
    (StatusCode::CREATED, Json(serde_json::json!({ "run_id": run.id }))).into_response()
}

// ── GET /api/jobs/poll ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PollQuery {
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}

fn default_timeout() -> u64 {
    25
}

async fn poll_job(
    State(state): State<AppState>,
    Query(q): Query<PollQuery>,
) -> impl IntoResponse {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(q.timeout_secs);
    loop {
        match state.store.claim_next_job().await {
            Ok(Some(run)) => {
                return Json(serde_json::json!({
                    "job": run,
                    "github_token": state.github_token,
                })).into_response();
            }
            Ok(None) => {
                if tokio::time::Instant::now() >= deadline {
                    return (StatusCode::NO_CONTENT, "").into_response();
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => {
                tracing::error!("poll job: {e}");
                return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
            }
        }
    }
}

// ── POST /api/runs/:id/steps/:step/logs ──────────────────────────────────────

#[derive(Deserialize)]
struct LogsBody {
    lines: Vec<String>,
}

async fn ingest_logs(
    State(state): State<AppState>,
    Path((run_id, step)): Path<(String, String)>,
    Json(body): Json<LogsBody>,
) -> impl IntoResponse {
    for line in &body.lines {
        if let Err(e) = state.store.append_log(&run_id, &step, line).await {
            tracing::error!("append log: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    }
    StatusCode::NO_CONTENT
}

// ── POST /api/runs/:id/status ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct StatusBody {
    status: String,
    step: Option<String>,
    step_status: Option<String>,
}

async fn update_run_status(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<StatusBody>,
) -> impl IntoResponse {
    if let (Some(step), Some(ss)) = (&body.step, &body.step_status) {
        let step_status = match ss.as_str() {
            "running" => StepStatus::Running,
            "success" => StepStatus::Success,
            "failure" => StepStatus::Failure,
            "skipped" => StepStatus::Skipped,
            _ => StepStatus::Pending,
        };
        if let Err(e) = state
            .store
            .update_step_status(&run_id, step, step_status)
            .await
        {
            tracing::error!("update step status: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    }

    let run_status = match body.status.as_str() {
        "running" => RunStatus::Running,
        "success" => RunStatus::Success,
        "failure" => RunStatus::Failure,
        "cancelled" => RunStatus::Cancelled,
        _ => RunStatus::Queued,
    };

    if let Err(e) = state.store.update_run_status(&run_id, run_status).await {
        tracing::error!("update run status: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR;
    }

    StatusCode::NO_CONTENT
}

// ── GET /api/repos ────────────────────────────────────────────────────────────

async fn list_repos_api(State(state): State<AppState>) -> impl IntoResponse {
    match state.store.list_repos().await {
        Ok(repos) => Json(repos).into_response(),
        Err(e) => {
            tracing::error!("list repos: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

// ── POST /api/repos ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateRepoBody {
    name: Option<String>,
    clone_url: String,
    branch: Option<String>,
}

async fn create_repo_api(
    State(state): State<AppState>,
    Json(body): Json<CreateRepoBody>,
) -> impl IntoResponse {
    let name = body.name.unwrap_or_else(|| repo_from_clone_url(&body.clone_url));
    let repo = Repo {
        id: Uuid::new_v4().to_string(),
        name,
        clone_url: body.clone_url,
        branch: body.branch.unwrap_or_else(|| "main".to_string()),
    };
    match state.store.create_repo(&repo).await {
        Ok(()) => (StatusCode::CREATED, Json(repo)).into_response(),
        Err(e) => {
            tracing::error!("create repo: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

// ── DELETE /api/repos/:id ─────────────────────────────────────────────────────

async fn delete_repo_api(
    State(state): State<AppState>,
    Path(repo_id): Path<String>,
) -> impl IntoResponse {
    match state.store.delete_repo(&repo_id).await {
        Ok(true) => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("delete repo: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

// ── POST /api/runs ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateRunBody {
    clone_url: String,
    branch: String,
    sha: Option<String>,
}

fn repo_from_clone_url(url: &str) -> String {
    // https://github.com/owner/repo.git  →  owner/repo
    // git@github.com:owner/repo.git      →  owner/repo
    let stripped = url.trim_end_matches(".git");
    let after_colon = stripped.rsplit_once(':').map(|(_, r)| r).unwrap_or(stripped);
    let parts: Vec<&str> = after_colon.trim_start_matches('/').split('/').collect();
    if parts.len() >= 2 {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        after_colon.to_string()
    }
}

async fn create_run(
    State(state): State<AppState>,
    Json(body): Json<CreateRunBody>,
) -> impl IntoResponse {
    let now = Utc::now();
    let run = Run {
        id: Uuid::new_v4().to_string(),
        repo: repo_from_clone_url(&body.clone_url),
        branch: body.branch,
        commit: body.sha.unwrap_or_else(|| "HEAD".to_string()),
        clone_url: body.clone_url,
        status: RunStatus::Queued,
        created_at: now,
        updated_at: now,
    };

    if let Err(e) = state.store.create_run(&run).await {
        tracing::error!("create run: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
    }

    tracing::info!(run_id = %run.id, repo = %run.repo, branch = %run.branch, "manual run queued");
    (StatusCode::CREATED, Json(serde_json::json!({ "run_id": run.id }))).into_response()
}

// ── GET /api/runs ─────────────────────────────────────────────────────────────

async fn list_runs(State(state): State<AppState>) -> impl IntoResponse {
    match state.store.list_runs(50).await {
        Ok(runs) => Json(runs).into_response(),
        Err(e) => {
            tracing::error!("list runs: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

// ── GET /api/runs/:id ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct RunDetail {
    #[serde(flatten)]
    run: Run,
    steps: Vec<StepRecord>,
}

async fn get_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> impl IntoResponse {
    let run = match state.store.get_run(&run_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => {
            tracing::error!("get run: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    let steps = match state.store.list_steps(&run_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("list steps: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    Json(RunDetail { run, steps }).into_response()
}

// ── GET /api/runs/:id/logs ────────────────────────────────────────────────────

#[derive(Serialize)]
struct LogEntry {
    step: String,
    line: String,
}

async fn get_logs(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> impl IntoResponse {
    match state.store.get_logs(&run_id).await {
        Ok(rows) => {
            let entries: Vec<LogEntry> = rows
                .into_iter()
                .map(|(step, line)| LogEntry { step, line })
                .collect();
            Json(entries).into_response()
        }
        Err(e) => {
            tracing::error!("get logs: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

pub async fn run(listen: &str, state: AppState) -> Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!("listening on {listen}");
    axum::serve(listener, app).await?;
    Ok(())
}
