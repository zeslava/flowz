//! Secrets resolution on the agent host, before a step starts.
//!
//! Values end up in `StepContext.env`, so the provider CLI never has to exist
//! inside a jail and the flowz server never sees a secret.

use std::collections::HashMap;
use std::process::Command;
use thiserror::Error;

/// Exit codes agreed with cfgy (see cfgy `src/main.rs`).
const EXIT_CONFIGURATION_NOT_FOUND: i32 = 3;
const EXIT_PROJECT_NOT_FOUND: i32 = 4;

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("provider binary '{bin}' not available: {source}")]
    ProviderUnavailable { bin: String, source: std::io::Error },
    #[error("configuration '{0}' not found")]
    ConfigurationNotFound(String),
    #[error("project '{0}' not found")]
    ProjectNotFound(String),
    #[error("provider failed (exit {code}): {stderr}")]
    ProviderFailed { code: i32, stderr: String },
    #[error("provider output is not valid JSON: {0}")]
    BadOutput(String),
}

pub trait SecretsProvider: Send + Sync {
    /// Fetch every parameter of `configuration` in `project`, decrypted.
    fn fetch(
        &self,
        project: &str,
        configuration: &str,
    ) -> Result<Vec<(String, String)>, SecretsError>;
}

pub struct CfgyProvider {
    pub bin: String,
    /// Passed as CFGY_SERVER_URL / CFGY_TOKEN when set; otherwise cfgy falls
    /// back to the agent's environment and ~/.cfgy/config.yaml.
    pub server_url: Option<String>,
    pub token: Option<String>,
}

impl CfgyProvider {
    pub fn new(bin: Option<String>, server_url: Option<String>, token: Option<String>) -> Self {
        Self {
            bin: bin.unwrap_or_else(|| "cfgy".to_string()),
            server_url,
            token,
        }
    }
}

impl SecretsProvider for CfgyProvider {
    fn fetch(
        &self,
        project: &str,
        configuration: &str,
    ) -> Result<Vec<(String, String)>, SecretsError> {
        let mut cmd = Command::new(&self.bin);
        cmd.args([
            "list",
            "--project",
            project,
            "-c",
            configuration,
            "-f",
            "json",
        ]);
        if let Some(url) = &self.server_url {
            cmd.env("CFGY_SERVER_URL", url);
        }
        if let Some(token) = &self.token {
            cmd.env("CFGY_TOKEN", token);
        }

        let output = cmd
            .output()
            .map_err(|source| SecretsError::ProviderUnavailable {
                bin: self.bin.clone(),
                source,
            })?;

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            return Err(match code {
                EXIT_CONFIGURATION_NOT_FOUND => {
                    SecretsError::ConfigurationNotFound(configuration.to_string())
                }
                EXIT_PROJECT_NOT_FOUND => SecretsError::ProjectNotFound(project.to_string()),
                _ => SecretsError::ProviderFailed {
                    code,
                    stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                },
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_json_map(&stdout)
    }
}

fn parse_json_map(raw: &str) -> Result<Vec<(String, String)>, SecretsError> {
    let map: HashMap<String, String> =
        serde_json::from_str(raw.trim()).map_err(|e| SecretsError::BadOutput(e.to_string()))?;
    let mut pairs: Vec<(String, String)> = map.into_iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(pairs)
}

/// Replaces known secret values in log lines before they leave the agent.
/// Without this a single `set -x` in a deploy script would persist a token in
/// the server database and UI forever.
#[derive(Default, Clone)]
pub struct Masker {
    values: Vec<String>,
}

impl Masker {
    pub fn add(&mut self, value: &str) {
        // Very short values would mask unrelated text into noise.
        if value.len() < 4 {
            return;
        }
        if !self.values.iter().any(|v| v == value) {
            self.values.push(value.to_string());
        }
        // Longest first, so an overlapping shorter value can't leave a tail.
        self.values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    }

    pub fn mask(&self, line: String) -> String {
        let mut line = line;
        for value in &self.values {
            if line.contains(value.as_str()) {
                line = line.replace(value.as_str(), "***");
            }
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_object() {
        let pairs = parse_json_map(r#"{"B":"2","A":"1"}"#).unwrap();
        assert_eq!(
            pairs,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string())
            ]
        );
    }

    #[test]
    fn rejects_non_json_output() {
        assert!(matches!(
            parse_json_map("A=1\n"),
            Err(SecretsError::BadOutput(_))
        ));
    }

    #[test]
    fn masks_values_including_substrings() {
        let mut m = Masker::default();
        m.add("supersecret");
        m.add("postgres://user:pw@host/db");
        let masked = m.mask("connecting to postgres://user:pw@host/db with supersecret".into());
        assert_eq!(masked, "connecting to *** with ***");
    }

    /// Writes a fake `cfgy` that echoes `stdout` and exits with `code`.
    fn fake_cfgy(name: &str, stdout: &str, code: i32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("flowz-fake-cfgy-{name}"));
        std::fs::write(
            &path,
            format!("#!/bin/sh\nprintf '%s' '{stdout}'\nexit {code}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn provider(bin: std::path::PathBuf) -> CfgyProvider {
        CfgyProvider::new(Some(bin.to_string_lossy().to_string()), None, None)
    }

    #[test]
    fn fetches_values_from_provider() {
        let bin = fake_cfgy("ok", r#"{"TOKEN":"abc"}"#, 0);
        let pairs = provider(bin).fetch("teka", "prod").unwrap();
        assert_eq!(pairs, vec![("TOKEN".to_string(), "abc".to_string())]);
    }

    #[test]
    fn maps_exit_codes_to_not_found() {
        let bin = fake_cfgy("noconf", "", EXIT_CONFIGURATION_NOT_FOUND);
        assert!(matches!(
            provider(bin).fetch("teka", "ghost"),
            Err(SecretsError::ConfigurationNotFound(c)) if c == "ghost"
        ));

        let bin = fake_cfgy("noproj", "", EXIT_PROJECT_NOT_FOUND);
        assert!(matches!(
            provider(bin).fetch("ghost", "prod"),
            Err(SecretsError::ProjectNotFound(p)) if p == "ghost"
        ));
    }

    #[test]
    fn missing_binary_is_reported() {
        let p = CfgyProvider::new(Some("/nonexistent/cfgy".to_string()), None, None);
        assert!(matches!(
            p.fetch("teka", "prod"),
            Err(SecretsError::ProviderUnavailable { .. })
        ));
    }

    #[test]
    fn ignores_short_values() {
        let mut m = Masker::default();
        m.add("ok");
        assert_eq!(m.mask("ok then".to_string()), "ok then");
    }
}
