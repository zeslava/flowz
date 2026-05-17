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
    fn run_step(
        &self,
        ctx: &StepContext,
        on_line: &mut dyn FnMut(String),
    ) -> Result<StepOutcome>;
}

#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum ExecutorKind {
    Stub,
    Dail,
}

impl Default for ExecutorKind {
    fn default() -> Self {
        ExecutorKind::Dail
    }
}

impl ExecutorKind {
    pub fn build(self) -> Box<dyn Executor> {
        match self {
            ExecutorKind::Stub => Box::new(StubExecutor),
            ExecutorKind::Dail => Box::new(DailExecutor::default()),
        }
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
            BufReader::new(stderr).lines().collect::<Result<Vec<_>, _>>()
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
}

impl Default for DailExecutor {
    fn default() -> Self {
        DailExecutor {
            dail_bin: "dail".to_string(),
            workspace_mount: "/workspace".to_string(),
        }
    }
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
    dail_bin: &str,
    jail_name: &str,
    workspace_mount: &str,
    ctx: &StepContext,
) -> std::process::Command {
    // dail requires root — always run via doas
    let mut cmd = std::process::Command::new("doas");
    cmd.arg(dail_bin).arg("run")
        .arg(ctx.run_file)
        .arg("--name")
        .arg(jail_name)
        .arg("--mount")
        .arg(format!("{}:{workspace_mount}", ctx.workspace.display()))
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
    dail_bin: String,
    jail_name: String,
}

impl Drop for JailGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.dail_bin)
            .args(["rm", &self.jail_name, "--force"])
            .output();
    }
}

impl Executor for DailExecutor {
    fn run_step(&self, ctx: &StepContext, on_line: &mut dyn FnMut(String)) -> Result<StepOutcome> {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;

        let run_prefix = &ctx.run_id[..ctx.run_id.len().min(8)];
        let jail_name = sanitize_jail_name(&format!(
            "flowz-{}-{}",
            run_prefix,
            ctx.step_name
        ));

        let _guard = JailGuard {
            dail_bin: self.dail_bin.clone(),
            jail_name: jail_name.clone(),
        };

        let mut cmd = build_dail_command(&self.dail_bin, &jail_name, &self.workspace_mount, ctx);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        // Drain stderr in a separate thread to avoid deadlock
        let stderr_handle = std::thread::spawn(move || {
            BufReader::new(stderr).lines().collect::<Result<Vec<_>, _>>()
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
        let cmd = build_dail_command("dail", "flowz-abc12345-build", "/workspace", &ctx);
        assert_eq!(cmd.get_program(), "doas");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args[0], "dail");
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
