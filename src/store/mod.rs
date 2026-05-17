use crate::model::{Repo, Run, RunStatus, StepRecord, StepStatus};
use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use std::str::FromStr;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    pub async fn new(db_url: &str) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(db_url)
            .context("parse db url")?
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(opts)
            .await
            .context("connect to sqlite")?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("run migrations")?;
        Ok(Self { pool })
    }

    pub async fn create_run(&self, run: &Run) -> Result<()> {
        sqlx::query(
            "INSERT INTO runs (id, repo, branch, \"commit\", clone_url, status, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&run.id)
        .bind(&run.repo)
        .bind(&run.branch)
        .bind(&run.commit)
        .bind(&run.clone_url)
        .bind(run.status.as_str())
        .bind(run.created_at.to_rfc3339())
        .bind(run.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .context("insert run")?;
        Ok(())
    }

    pub async fn create_step(&self, step: &StepRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO steps (run_id, name, status) VALUES (?, ?, ?)",
        )
        .bind(&step.run_id)
        .bind(&step.name)
        .bind(step.status.as_str())
        .execute(&self.pool)
        .await
        .context("insert step")?;
        Ok(())
    }

    pub async fn get_run(&self, id: &str) -> Result<Option<Run>> {
        let row = sqlx::query_as::<_, RunRow>(
            "SELECT id, repo, branch, \"commit\", clone_url, status, created_at, updated_at
             FROM runs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .context("get run")?;
        Ok(row.map(Into::into))
    }

    pub async fn list_runs(&self, limit: i64) -> Result<Vec<Run>> {
        let rows = sqlx::query_as::<_, RunRow>(
            "SELECT id, repo, branch, \"commit\", clone_url, status, created_at, updated_at
             FROM runs ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("list runs")?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn list_steps(&self, run_id: &str) -> Result<Vec<StepRecord>> {
        let rows = sqlx::query_as::<_, StepRow>(
            "SELECT run_id, name, status, started_at, finished_at FROM steps WHERE run_id = ?",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await
        .context("list steps")?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn update_run_status(&self, id: &str, status: RunStatus) -> Result<()> {
        sqlx::query("UPDATE runs SET status = ?, updated_at = ? WHERE id = ?")
            .bind(status.as_str())
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(&self.pool)
            .await
            .context("update run status")?;
        Ok(())
    }

    pub async fn update_step_status(
        &self,
        run_id: &str,
        step: &str,
        status: StepStatus,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let (started_at, finished_at) = match status {
            StepStatus::Running => (Some(now.clone()), None),
            StepStatus::Success | StepStatus::Failure | StepStatus::Skipped => {
                (None, Some(now.clone()))
            }
            StepStatus::Pending => (None, None),
        };
        if let Some(started) = started_at {
            sqlx::query(
                "UPDATE steps SET status = ?, started_at = ? WHERE run_id = ? AND name = ?",
            )
            .bind(status.as_str())
            .bind(started)
            .bind(run_id)
            .bind(step)
            .execute(&self.pool)
            .await
            .context("update step status")?;
        } else if let Some(finished) = finished_at {
            sqlx::query(
                "UPDATE steps SET status = ?, finished_at = ? WHERE run_id = ? AND name = ?",
            )
            .bind(status.as_str())
            .bind(finished)
            .bind(run_id)
            .bind(step)
            .execute(&self.pool)
            .await
            .context("update step status")?;
        } else {
            sqlx::query("UPDATE steps SET status = ? WHERE run_id = ? AND name = ?")
                .bind(status.as_str())
                .bind(run_id)
                .bind(step)
                .execute(&self.pool)
                .await
                .context("update step status")?;
        }
        Ok(())
    }

    pub async fn append_log(&self, run_id: &str, step: &str, line: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO log_lines (run_id, step, line, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(run_id)
        .bind(step)
        .bind(line)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await
        .context("append log")?;
        Ok(())
    }

    pub async fn get_logs(&self, run_id: &str) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT step, line FROM log_lines WHERE run_id = ? ORDER BY id",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await
        .context("get logs")?;
        Ok(rows)
    }

    pub async fn get_logs_from(&self, run_id: &str, after_id: i64) -> Result<Vec<(i64, String, String)>> {
        let rows = sqlx::query_as::<_, (i64, String, String)>(
            "SELECT id, step, line FROM log_lines WHERE run_id = ? AND id > ? ORDER BY id",
        )
        .bind(run_id)
        .bind(after_id)
        .fetch_all(&self.pool)
        .await
        .context("get logs from")?;
        Ok(rows)
    }

    pub async fn create_repo(&self, repo: &Repo) -> Result<()> {
        sqlx::query(
            "INSERT INTO repos (id, name, clone_url, branch) VALUES (?, ?, ?, ?)",
        )
        .bind(&repo.id)
        .bind(&repo.name)
        .bind(&repo.clone_url)
        .bind(&repo.branch)
        .execute(&self.pool)
        .await
        .context("insert repo")?;
        Ok(())
    }

    pub async fn list_repos(&self) -> Result<Vec<Repo>> {
        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT id, name, clone_url, branch FROM repos ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await
        .context("list repos")?;
        Ok(rows
            .into_iter()
            .map(|(id, name, clone_url, branch)| Repo { id, name, clone_url, branch })
            .collect())
    }

    pub async fn delete_repo(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM repos WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("delete repo")?;
        Ok(result.rows_affected() > 0)
    }

    /// Returns the next queued run (job claim for agent).
    pub async fn claim_next_job(&self) -> Result<Option<Run>> {
        let row = sqlx::query_as::<_, RunRow>(
            "SELECT id, repo, branch, \"commit\", clone_url, status, created_at, updated_at
             FROM runs WHERE status = 'queued' ORDER BY created_at LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .context("claim job")?;

        if let Some(r) = &row {
            sqlx::query("UPDATE runs SET status = 'running', updated_at = ? WHERE id = ?")
                .bind(Utc::now().to_rfc3339())
                .bind(&r.id)
                .execute(&self.pool)
                .await
                .context("mark run running")?;
        }

        Ok(row.map(Into::into))
    }
}

// ── internal row types ────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct RunRow {
    id: String,
    repo: String,
    branch: String,
    commit: String,
    clone_url: String,
    status: String,
    created_at: String,
    updated_at: String,
}

impl From<RunRow> for Run {
    fn from(r: RunRow) -> Self {
        Run {
            id: r.id,
            repo: r.repo,
            branch: r.branch,
            commit: r.commit,
            clone_url: r.clone_url,
            status: RunStatus::from_str(&r.status),
            created_at: r.created_at.parse().unwrap_or_default(),
            updated_at: r.updated_at.parse().unwrap_or_default(),
        }
    }
}

#[derive(sqlx::FromRow)]
struct StepRow {
    run_id: String,
    name: String,
    status: String,
    started_at: Option<String>,
    finished_at: Option<String>,
}

impl From<StepRow> for StepRecord {
    fn from(r: StepRow) -> Self {
        StepRecord {
            run_id: r.run_id,
            name: r.name,
            status: StepStatus::from_str(&r.status),
            started_at: r.started_at.and_then(|s| s.parse().ok()),
            finished_at: r.finished_at.and_then(|s| s.parse().ok()),
        }
    }
}

impl RunStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Cancelled => "cancelled",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "success" => Self::Success,
            "failure" => Self::Failure,
            "cancelled" => Self::Cancelled,
            _ => Self::Queued,
        }
    }
}

impl StepStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Skipped => "skipped",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "success" => Self::Success,
            "failure" => Self::Failure,
            "skipped" => Self::Skipped,
            _ => Self::Pending,
        }
    }
}
