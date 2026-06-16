use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::path::PathBuf;
use uuid::Uuid;

use crate::agent::{run_loop, AgentConfig, WorkspaceBackend};
use crate::executor::ExecutorKind;
use crate::server;
use crate::store::Store;

#[derive(Parser)]
#[command(name = "flowz", version, about = "git-native CI/CD for FreeBSD")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server (webhook ingest, API, UI)
    Server,
    /// Run the agent worker (long-poll jobs, execute pipelines)
    Agent,
    /// Validate a .flowz.yaml pipeline
    Validate,
    /// Show run status
    Status,
    /// Fetch run logs
    Logs,
}

pub async fn run() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Command::Server => run_server().await,
        Command::Agent => run_agent().await,
        Command::Validate | Command::Status | Command::Logs => {
            anyhow::bail!("not yet implemented")
        }
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
}

// ---- server ----

#[derive(Deserialize)]
struct ServerConfig {
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default = "default_db")]
    db: String,
    #[serde(default = "default_artifacts_dir")]
    artifacts_dir: PathBuf,
    webhook_secret: String,
    github_token: Option<String>,
    #[serde(default)]
    oidc: Option<server::OidcConfig>,
    #[serde(default)]
    jwt_secret: Option<String>,
}

fn default_listen() -> String {
    "0.0.0.0:7878".to_string()
}

fn default_db() -> String {
    "flowz.db".to_string()
}

fn default_artifacts_dir() -> PathBuf {
    PathBuf::from("/var/db/flowz/artifacts")
}

async fn run_server() -> Result<()> {
    let config_path = std::env::var("FLOWZ_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("flowz-server.yaml"));

    let config: ServerConfig = if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path)
            .with_context(|| format!("read {}", config_path.display()))?;
        serde_yaml_ng::from_str(&raw).context("parse config")?
    } else {
        let secret = std::env::var("FLOWZ_WEBHOOK_SECRET")
            .context("FLOWZ_WEBHOOK_SECRET not set and no config file found")?;
        ServerConfig {
            listen: default_listen(),
            db: default_db(),
            artifacts_dir: default_artifacts_dir(),
            webhook_secret: secret,
            github_token: std::env::var("FLOWZ_GITHUB_TOKEN").ok(),
            oidc: server::OidcConfig::from_env(),
            jwt_secret: std::env::var("FLOWZ_JWT_SECRET").ok(),
        }
    };

    let db_url = format!("sqlite:{}", config.db);
    let store = Store::new(&db_url).await.context("init store")?;

    std::fs::create_dir_all(&config.artifacts_dir)
        .with_context(|| format!("create artifacts dir {}", config.artifacts_dir.display()))?;

    let oidc = config.oidc.or_else(server::OidcConfig::from_env);
    let jwt_secret = config
        .jwt_secret
        .or_else(|| std::env::var("FLOWZ_JWT_SECRET").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if oidc.is_some() {
                tracing::warn!(
                    "jwt_secret not set — generated a random one; sessions reset on restart. Set jwt_secret or FLOWZ_JWT_SECRET."
                );
            }
            format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
        });

    if oidc.is_some() {
        tracing::info!("OIDC/ZID authentication enabled");
    }

    let state = server::AppState {
        store,
        webhook_secret: config.webhook_secret,
        github_token: config.github_token,
        artifacts_dir: config.artifacts_dir,
        oidc,
        jwt_secret,
    };

    server::run(&config.listen, state).await
}

// ---- agent ----

#[derive(Deserialize)]
struct AgentFileConfig {
    server_url: String,
    #[serde(default = "default_agent_name")]
    agent_name: String,
    #[serde(default = "default_workspace")]
    workspace_dir: PathBuf,
    /// If set, run each job in a ZFS clone under this parent dataset (e.g.
    /// `zroot/flowz/ws`) instead of a plain directory. Recommended on FreeBSD
    /// with the Dail executor.
    #[serde(default)]
    zfs_pool: Option<String>,
    #[serde(default)]
    executor: ExecutorKind,
    #[serde(default)]
    dail_bin: Option<String>,
    #[serde(default)]
    priv_tool: Option<String>,
    /// HOME directory for exec: steps. Set when the agent runs as a service
    /// user with a non-existent home (e.g. /nonexistent on FreeBSD).
    #[serde(default)]
    home_dir: Option<String>,
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

async fn run_agent() -> Result<()> {
    let config_path = std::env::var("FLOWZ_AGENT_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("flowz-agent.yaml"));

    let config: AgentFileConfig = if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path)
            .with_context(|| format!("read {}", config_path.display()))?;
        serde_yaml_ng::from_str(&raw).context("parse config")?
    } else {
        let server_url = std::env::var("FLOWZ_SERVER_URL")
            .context("FLOWZ_SERVER_URL not set and no config file found")?;
        AgentFileConfig {
            server_url,
            agent_name: default_agent_name(),
            workspace_dir: default_workspace(),
            zfs_pool: None,
            executor: ExecutorKind::default(),
            dail_bin: None,
            priv_tool: None,
            home_dir: None,
        }
    };

    let workspace = match &config.zfs_pool {
        Some(pool) => WorkspaceBackend::Zfs { pool: pool.clone() },
        None => {
            std::fs::create_dir_all(&config.workspace_dir).with_context(|| {
                format!("create workspace dir {}", config.workspace_dir.display())
            })?;
            WorkspaceBackend::Dir(config.workspace_dir.clone())
        }
    };

    tracing::info!(
        server = %config.server_url,
        agent = %config.agent_name,
        workspace = match &config.zfs_pool {
            Some(pool) => pool.clone(),
            None => config.workspace_dir.display().to_string(),
        },
        "agent starting"
    );

    run_loop(AgentConfig {
        server_url: config.server_url,
        agent_name: config.agent_name,
        workspace,
        executor: config.executor,
        dail_bin: config.dail_bin,
        priv_tool: config.priv_tool,
        home_dir: config.home_dir,
    })
    .await
}
