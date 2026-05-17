use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct PipelineFile {
    pub version: u32,
    #[serde(default)]
    pub triggers: Vec<Trigger>,
    #[serde(default)]
    pub env: IndexMap<String, String>,
    pub steps: IndexMap<String, Step>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", tag = "on")]
pub enum Trigger {
    Push { branches: Option<Vec<String>> },
    PullRequest { branches: Option<Vec<String>> },
    Cron { schedule: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    pub run: String,
    #[serde(default)]
    pub needs: Vec<String>,
    pub only: Option<Only>,
    #[serde(default)]
    pub env: IndexMap<String, String>,
    pub timeout: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Only {
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Success,
    Failure,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Success,
    Failure,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub repo: String,
    pub branch: String,
    pub commit: String,
    pub clone_url: String,
    pub status: RunStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    pub id: String,
    pub name: String,
    pub clone_url: String,
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub run_id: String,
    pub name: String,
    pub status: StepStatus,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}
