use anyhow::{Context, Result};
use flowz::agent::{run_loop, AgentConfig};
use flowz::executor::ExecutorKind;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Config {
    server_url: String,
    #[serde(default = "default_agent_name")]
    agent_name: String,
    #[serde(default = "default_workspace")]
    workspace_dir: PathBuf,
    #[serde(default)]
    executor: ExecutorKind,
}

fn default_agent_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "agent".to_string())
}

fn default_workspace() -> PathBuf {
    PathBuf::from("/tmp/flowz-workspace")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config_path = std::env::var("FLOWZ_AGENT_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("flowz-agent.yaml"));

    let config: Config = if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path)
            .with_context(|| format!("read {}", config_path.display()))?;
        serde_yaml_ng::from_str(&raw).context("parse config")?
    } else {
        let server_url = std::env::var("FLOWZ_SERVER_URL")
            .context("FLOWZ_SERVER_URL not set and no config file found")?;
        Config {
            server_url,
            agent_name: default_agent_name(),
            workspace_dir: default_workspace(),
            executor: ExecutorKind::default(),
        }
    };

    std::fs::create_dir_all(&config.workspace_dir)
        .with_context(|| format!("create workspace dir {}", config.workspace_dir.display()))?;

    tracing::info!(
        server = %config.server_url,
        agent = %config.agent_name,
        workspace = %config.workspace_dir.display(),
        "agent starting"
    );

    run_loop(AgentConfig {
        server_url: config.server_url,
        agent_name: config.agent_name,
        workspace_dir: config.workspace_dir,
        executor: config.executor,
    })
    .await
}
