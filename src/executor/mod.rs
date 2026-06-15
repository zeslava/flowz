use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

pub struct StepContext<'a> {
    pub run_id: &'a str,
    pub step_name: &'a str,
    pub run_file: &'a str,
    pub workspace: &'a Path,
    pub env: Vec<(String, String)>,
}

pub struct StepOutcome {
    pub exit_code: i32,
    pub success: bool,
}

pub trait Executor: Send + Sync {
    fn run_step(&self, ctx: &StepContext, on_line: &mut dyn FnMut(String)) -> Result<StepOutcome>;
}

#[derive(Deserialize, Clone, Copy, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExecutorKind {
    Stub,
    #[default]
    Dail,
}

impl ExecutorKind {
    pub fn build(self, dail_bin: Option<String>, priv_tool: Option<String>) -> Box<dyn Executor> {
        match self {
            ExecutorKind::Stub => Box::new(StubExecutor),
            ExecutorKind::Dail => {
                let mut exec = DailExecutor::default();
                if let Some(bin) = dail_bin {
                    exec.dail_bin = bin;
                }
                if let Some(tool) = priv_tool {
                    exec.priv_tool = tool;
                }
                Box::new(exec)
            }
        }
    }
}

// HostExecutor: runs `sh <script>` directly on the agent host — used for exec: steps.
pub struct HostExecutor;

impl Executor for HostExecutor {
    fn run_step(&self, ctx: &StepContext, on_line: &mut dyn FnMut(String)) -> Result<StepOutcome> {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        let mut cmd = Command::new("sh");
        cmd.arg(ctx.run_file)
            .current_dir(ctx.workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        for (k, v) in &ctx.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let stderr_handle = std::thread::spawn(move || {
            BufReader::new(stderr)
                .lines()
                .collect::<Result<Vec<_>, _>>()
        });

        for line in BufReader::new(stdout).lines() {
            on_line(line?);
        }

        let status = child.wait()?;

        if let Ok(Ok(lines)) = stderr_handle.join() {
            for line in lines {
                on_line(line);
            }
        }
        let exit_code = status.code().unwrap_or(-1);
        Ok(StepOutcome {
            exit_code,
            success: status.success(),
        })
    }
}

// StubExecutor: runs `sh <run_file>` in workspace — works on Linux for dev/testing
pub struct StubExecutor;

impl Executor for StubExecutor {
    fn run_step(&self, ctx: &StepContext, on_line: &mut dyn FnMut(String)) -> Result<StepOutcome> {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        let mut cmd = Command::new("sh");
        cmd.arg(ctx.run_file)
            .current_dir(ctx.workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        for (k, v) in &ctx.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        // Drain stderr in a separate thread to avoid deadlock
        let stderr_handle = std::thread::spawn(move || {
            BufReader::new(stderr)
                .lines()
                .collect::<Result<Vec<_>, _>>()
        });

        for line in BufReader::new(stdout).lines() {
            on_line(line?);
        }

        let status = child.wait()?;

        if let Ok(Ok(lines)) = stderr_handle.join() {
            for line in lines {
                on_line(line);
            }
        }
        let exit_code = status.code().unwrap_or(-1);
        Ok(StepOutcome {
            exit_code,
            success: status.success(),
        })
    }
}

pub struct DailExecutor {
    pub dail_bin: String,
    pub workspace_mount: String,
    // privilege escalation tool: "doas" (preferred on FreeBSD) or "sudo"
    pub priv_tool: String,
}

impl Default for DailExecutor {
    fn default() -> Self {
        DailExecutor {
            dail_bin: "/usr/local/bin/dail".to_string(),
            workspace_mount: "/workspace".to_string(),
            priv_tool: detect_priv_tool(),
        }
    }
}

// dail requires root: prefer doas (FreeBSD-idiomatic), fall back to sudo.
fn detect_priv_tool() -> String {
    for tool in ["doas", "sudo"] {
        if find_in_path(tool).is_some() {
            return tool.to_string();
        }
    }
    "doas".to_string()
}

fn find_in_path(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

fn sanitize_jail_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn build_dail_command(
    priv_tool: &str,
    dail_bin: &str,
    jail_name: &str,
    workspace_mount: &str,
    ctx: &StepContext,
) -> std::process::Command {
    // dail requires root — always run via the privilege escalation tool
    let mut cmd = std::process::Command::new(priv_tool);
    cmd.arg(dail_bin)
        .arg("run")
        .arg(ctx.run_file)
        .arg("--name")
        .arg(jail_name)
        .arg("--mount")
        .arg(format!("{}:{workspace_mount}", ctx.workspace.display()))
        .arg("-w")
        .arg(workspace_mount)
        .arg("--network")
        .arg("inherit")
        .arg("--rm")
        .arg("--wait")
        .current_dir(ctx.workspace);

    for (k, v) in &ctx.env {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }

    cmd
}

struct JailGuard {
    priv_tool: String,
    dail_bin: String,
    jail_name: String,
}

impl Drop for JailGuard {
    fn drop(&mut self) {
        // dail rm needs root too — go through the same privilege escalation tool
        let _ = std::process::Command::new(&self.priv_tool)
            .args([&self.dail_bin, "rm", &self.jail_name, "--force"])
            .output();
    }
}

impl Executor for DailExecutor {
    fn run_step(&self, ctx: &StepContext, on_line: &mut dyn FnMut(String)) -> Result<StepOutcome> {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;

        let run_prefix = &ctx.run_id[..ctx.run_id.len().min(8)];
        let jail_name = sanitize_jail_name(&format!("flowz-{}-{}", run_prefix, ctx.step_name));

        let _guard = JailGuard {
            priv_tool: self.priv_tool.clone(),
            dail_bin: self.dail_bin.clone(),
            jail_name: jail_name.clone(),
        };

        let mut cmd = build_dail_command(
            &self.priv_tool,
            &self.dail_bin,
            &jail_name,
            &self.workspace_mount,
            ctx,
        );
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        // Drain stderr in a separate thread to avoid deadlock
        let stderr_handle = std::thread::spawn(move || {
            BufReader::new(stderr)
                .lines()
                .collect::<Result<Vec<_>, _>>()
        });

        for line in BufReader::new(stdout).lines() {
            on_line(line?);
        }

        let status = child.wait()?;

        if let Ok(Ok(lines)) = stderr_handle.join() {
            for line in lines {
                on_line(line);
            }
        }

        let exit_code = status.code().unwrap_or(-1);
        Ok(StepOutcome {
            exit_code,
            success: status.success(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn build_dail_command_args() {
        let ctx = StepContext {
            run_id: "abc12345-def",
            step_name: "build",
            run_file: "ci/build.dail",
            workspace: Path::new("/tmp/workspace"),
            env: vec![("FOO".to_string(), "bar".to_string())],
        };
        let cmd = build_dail_command(
            "doas",
            "/usr/local/bin/dail",
            "flowz-abc12345-build",
            "/workspace",
            &ctx,
        );
        assert_eq!(cmd.get_program(), "doas");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args[0], "/usr/local/bin/dail");
        assert!(args.contains(&std::ffi::OsStr::new("run")));
        assert!(args.contains(&std::ffi::OsStr::new("ci/build.dail")));
        assert!(args.contains(&std::ffi::OsStr::new("--name")));
        assert!(args.contains(&std::ffi::OsStr::new("flowz-abc12345-build")));
        assert!(args.contains(&std::ffi::OsStr::new("--wait")));
        assert!(args.contains(&std::ffi::OsStr::new("--rm")));
        assert!(args.contains(&std::ffi::OsStr::new("-e")));
        assert!(args.contains(&std::ffi::OsStr::new("FOO=bar")));
    }

    #[test]
    fn sanitize_jail_name_replaces_special_chars() {
        assert_eq!(sanitize_jail_name("My Step_1!"), "my-step-1-");
    }
}
