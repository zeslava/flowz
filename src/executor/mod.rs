use anyhow::Result;
use std::path::Path;

pub struct StepContext<'a> {
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
