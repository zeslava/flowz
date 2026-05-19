use crate::{
    executor::{ExecutorKind, StepContext},
    model::{Run, RunStatus, StepStatus},
    pipeline,
};
use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct AgentConfig {
    pub server_url: String,
    pub agent_name: String,
    pub workspace_dir: PathBuf,
    pub executor: ExecutorKind,
}

pub async fn run_loop(cfg: AgentConfig) -> Result<()> {
    let client = Client::new();

    loop {
        match poll_job(&client, &cfg.server_url).await {
            Ok(Some((run, github_token))) => {
                tracing::info!(run_id = %run.id, repo = %run.repo, "claimed job");
                if let Err(e) = execute_run(&client, &cfg, &run, github_token.as_deref()).await {
                    tracing::error!(run_id = %run.id, "run failed: {e}");
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("poll error: {e}, retrying in 5s");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
}

async fn poll_job(client: &Client, server_url: &str) -> Result<Option<(Run, Option<String>)>> {
    let resp = client
        .get(format!("{server_url}/api/jobs/poll"))
        .send()
        .await
        .context("poll request")?;

    if resp.status().as_u16() == 204 {
        return Ok(None);
    }

    let body: serde_json::Value = resp.json().await.context("poll response body")?;
    let run: Run = serde_json::from_value(body["job"].clone()).context("deserialize run")?;
    let github_token = body["github_token"].as_str().map(|s| s.to_string());
    Ok(Some((run, github_token)))
}

async fn execute_run(
    client: &Client,
    cfg: &AgentConfig,
    run: &Run,
    github_token: Option<&str>,
) -> Result<()> {
    let workspace = cfg.workspace_dir.join(&run.id);
    std::fs::create_dir_all(&workspace).context("create workspace")?;

    let result = do_execute(client, cfg, run, &workspace, github_token).await;

    if let Err(ref e) = result {
        post_logs(
            client,
            &cfg.server_url,
            &run.id,
            "agent",
            vec![format!("error: {e:#}")],
        )
        .await;
        post_run_status(client, &cfg.server_url, &run.id, RunStatus::Failure).await;
    }

    if let Err(e) = remove_dir_all_force(&workspace) {
        tracing::warn!(run_id = %run.id, "cleanup workspace: {e}");
    }

    result
}

async fn do_execute(
    client: &Client,
    cfg: &AgentConfig,
    run: &Run,
    workspace: &Path,
    github_token: Option<&str>,
) -> Result<()> {
    // git clone
    git_clone(&run.clone_url, workspace).context("git clone")?;

    // parse pipeline
    let yaml_path = workspace.join(".flowz.yaml");
    let yaml = std::fs::read_to_string(&yaml_path)
        .with_context(|| format!("read {}", yaml_path.display()))?;
    let pipeline = pipeline::parse(&yaml).context("parse .flowz.yaml")?;

    let mut overall_success = true;

    for (step_name, step) in &pipeline.steps {
        // honor only.branch filter
        if let Some(only) = &step.only {
            if let Some(branch_filter) = &only.branch {
                if branch_filter != &run.branch {
                    post_step_status(
                        client,
                        &cfg.server_url,
                        &run.id,
                        step_name,
                        StepStatus::Skipped,
                    )
                    .await;
                    continue;
                }
            }
        }

        post_step_status(
            client,
            &cfg.server_url,
            &run.id,
            step_name,
            StepStatus::Running,
        )
        .await;

        let env: Vec<(String, String)> = pipeline
            .env
            .iter()
            .chain(step.env.iter())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let ctx = StepContext {
            run_id: &run.id,
            step_name,
            run_file: &step.run,
            workspace,
            env,
        };

        // run_step is sync — run in blocking thread pool.
        // Use a channel so the blocking thread can hand off log lines to a
        // single async consumer that naturally batches bursty output and sends
        // one HTTP request per burst instead of one per line.
        let outcome = {
            let run_file = step.run.clone();
            let workspace = workspace.to_path_buf();
            let env = ctx.env.clone();
            let step_name_owned = step_name.clone();
            let run_id = run.id.clone();
            let executor_kind = cfg.executor;

            let (log_tx, mut log_rx) = tokio::sync::mpsc::channel::<String>(256);

            let client_consumer = client.clone();
            let server_url_consumer = cfg.server_url.clone();
            let run_id_consumer = run.id.clone();
            let step_name_consumer = step_name.to_string();
            let consumer = tokio::spawn(async move {
                while let Some(line) = log_rx.recv().await {
                    let mut batch = vec![line];
                    while let Ok(line) = log_rx.try_recv() {
                        batch.push(line);
                    }
                    post_logs(
                        &client_consumer,
                        &server_url_consumer,
                        &run_id_consumer,
                        &step_name_consumer,
                        batch,
                    )
                    .await;
                }
            });

            let blocking_result = tokio::task::spawn_blocking(move || {
                let ctx = StepContext {
                    run_id: &run_id,
                    step_name: &step_name_owned,
                    run_file: &run_file,
                    workspace: &workspace,
                    env,
                };
                let executor = executor_kind.build();
                executor.run_step(&ctx, &mut |line| {
                    let _ = log_tx.blocking_send(line);
                })
                // log_tx dropped here, which closes the channel
            })
            .await
            .context("spawn_blocking")?;

            consumer.await.context("log consumer")?;

            blocking_result
        };

        let step_status = match &outcome {
            Ok(o) => {
                let marker = if o.success { "✓" } else { "✗" };
                post_logs(
                    client,
                    &cfg.server_url,
                    &run.id,
                    step_name,
                    vec![format!("{marker} exit {}", o.exit_code)],
                )
                .await;
                if o.success {
                    if !step.artifacts.is_empty() {
                        upload_artifacts(
                            client,
                            &cfg.server_url,
                            &run.id,
                            step_name,
                            &step.artifacts,
                            workspace,
                        )
                        .await;
                    }
                    StepStatus::Success
                } else {
                    overall_success = false;
                    StepStatus::Failure
                }
            }
            Err(e) => {
                post_logs(
                    client,
                    &cfg.server_url,
                    &run.id,
                    step_name,
                    vec![format!("✗ executor error: {e:#}")],
                )
                .await;
                overall_success = false;
                StepStatus::Failure
            }
        };

        post_step_status(client, &cfg.server_url, &run.id, step_name, step_status).await;

        if !overall_success {
            // fail fast: remaining steps stay pending
            break;
        }
    }

    let run_status = if overall_success {
        RunStatus::Success
    } else {
        RunStatus::Failure
    };

    post_run_status(client, &cfg.server_url, &run.id, run_status).await;

    if let Some(token) = github_token {
        post_commit_status(client, token, &run.repo, &run.commit, overall_success).await;
    }

    Ok(())
}

fn git_clone(clone_url: &str, workspace: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["clone", "--depth=1", clone_url, "."])
        .current_dir(workspace)
        .status()
        .context("spawn git clone")?;

    if !status.success() {
        anyhow::bail!("git clone exited with {status}");
    }
    Ok(())
}

async fn post_logs(
    client: &Client,
    server_url: &str,
    run_id: &str,
    step: &str,
    lines: Vec<String>,
) {
    let url = format!("{server_url}/api/runs/{run_id}/steps/{step}/logs");
    if let Err(e) = client
        .post(&url)
        .json(&json!({ "lines": lines }))
        .send()
        .await
    {
        tracing::warn!("post logs: {e}");
    }
}

async fn upload_artifacts(
    client: &Client,
    server_url: &str,
    run_id: &str,
    step: &str,
    paths: &[String],
    workspace: &Path,
) {
    for spec in paths {
        let abs = workspace.join(spec);
        if !abs.exists() {
            post_logs(
                client,
                server_url,
                run_id,
                step,
                vec![format!("⚠ artifact not found: {spec}")],
            )
            .await;
            continue;
        }

        let mut files: Vec<PathBuf> = Vec::new();
        if abs.is_dir() {
            collect_files(&abs, &mut files);
        } else {
            files.push(abs);
        }

        for file in files {
            let rel = match file.strip_prefix(workspace) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            let bytes = match std::fs::read(&file) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("read artifact {}: {e}", file.display());
                    continue;
                }
            };
            let size = bytes.len();
            let url = format!("{server_url}/api/runs/{run_id}/steps/{step}/artifacts/{rel}");
            match client.post(&url).body(bytes).send().await {
                Ok(resp) if resp.status().is_success() => {
                    post_logs(
                        client,
                        server_url,
                        run_id,
                        step,
                        vec![format!("↑ artifact {rel} ({size} bytes)")],
                    )
                    .await;
                }
                Ok(resp) => {
                    tracing::warn!("upload artifact {rel}: status {}", resp.status());
                }
                Err(e) => tracing::warn!("upload artifact {rel}: {e}"),
            }
        }
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("read artifact dir {}: {e}", dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

async fn post_step_status(
    client: &Client,
    server_url: &str,
    run_id: &str,
    step: &str,
    status: StepStatus,
) {
    let status_str = match status {
        StepStatus::Pending => "pending",
        StepStatus::Running => "running",
        StepStatus::Success => "success",
        StepStatus::Failure => "failure",
        StepStatus::Skipped => "skipped",
    };
    let url = format!("{server_url}/api/runs/{run_id}/status");
    if let Err(e) = client
        .post(&url)
        .json(&json!({
            "status": "running",
            "step": step,
            "step_status": status_str,
        }))
        .send()
        .await
    {
        tracing::warn!("post step status: {e}");
    }
}

async fn post_run_status(client: &Client, server_url: &str, run_id: &str, status: RunStatus) {
    let status_str = match status {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::Success => "success",
        RunStatus::Failure => "failure",
        RunStatus::Cancelled => "cancelled",
    };
    let url = format!("{server_url}/api/runs/{run_id}/status");
    if let Err(e) = client
        .post(&url)
        .json(&json!({ "status": status_str }))
        .send()
        .await
    {
        tracing::warn!("post run status: {e}");
    }
}

async fn post_commit_status(client: &Client, token: &str, repo: &str, sha: &str, success: bool) {
    let state = if success { "success" } else { "failure" };
    let url = format!("https://api.github.com/repos/{repo}/statuses/{sha}");
    let result = client
        .post(&url)
        .bearer_auth(token)
        .header("User-Agent", "flowz-agent")
        .json(&json!({
            "state": state,
            "context": "flowz",
        }))
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!(repo, sha, state, "posted GitHub commit status");
        }
        Ok(resp) => {
            tracing::warn!(repo, sha, status = %resp.status(), "GitHub commit status rejected");
        }
        Err(e) => {
            tracing::warn!(repo, sha, "post commit status: {e}");
        }
    }
}

// git sets .git/objects dirs to 0555; make everything writable before removal.
fn remove_dir_all_force(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // ensure this dir is readable+writable+executable before touching its contents
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o700);
        let _ = std::fs::set_permissions(path, perms);
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let p = entry.path();
        if entry.file_type()?.is_dir() {
            remove_dir_all_force(&p)?;
            std::fs::remove_dir(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
    }
    std::fs::remove_dir(path)
}
