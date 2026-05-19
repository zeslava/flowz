use anyhow::{Context, Result};
use flowz::{server, store::Store};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Config {
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default = "default_db")]
    db: String,
    #[serde(default = "default_artifacts_dir")]
    artifacts_dir: PathBuf,
    webhook_secret: String,
    github_token: Option<String>,
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config_path = std::env::var("FLOWZ_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("flowz-server.yaml"));

    let config: Config = if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path)
            .with_context(|| format!("read {}", config_path.display()))?;
        serde_yaml_ng::from_str(&raw).context("parse config")?
    } else {
        let secret = std::env::var("FLOWZ_WEBHOOK_SECRET")
            .context("FLOWZ_WEBHOOK_SECRET not set and no config file found")?;
        Config {
            listen: default_listen(),
            db: default_db(),
            artifacts_dir: default_artifacts_dir(),
            webhook_secret: secret,
            github_token: std::env::var("FLOWZ_GITHUB_TOKEN").ok(),
        }
    };

    let db_url = format!("sqlite:{}", config.db);
    let store = Store::new(&db_url).await.context("init store")?;

    std::fs::create_dir_all(&config.artifacts_dir)
        .with_context(|| format!("create artifacts dir {}", config.artifacts_dir.display()))?;

    let state = server::AppState {
        store,
        webhook_secret: config.webhook_secret,
        github_token: config.github_token,
        artifacts_dir: config.artifacts_dir,
    };

    server::run(&config.listen, state).await
}
