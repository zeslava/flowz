//! Job workspace provisioning.
//!
//! Two backends:
//! - [`WorkspaceBackend::Dir`] — plain directory under `workspace_dir`, cloned by
//!   the agent. Used for Linux/StubExecutor dev. Cleanup is `rm -rf` (with a
//!   privileged fallback for root-owned files left by a dail jail).
//! - [`WorkspaceBackend::Zfs`] — a per-repo "warm" dataset holds a full clone
//!   plus build cache; each job runs in an instant `zfs clone` of a
//!   snapshot of it. Cleanup is `zfs destroy`, so root-owned files inside the
//!   jail are never a problem and the original repo dataset is untouched.
//!
//! In the ZFS backend every filesystem operation runs as root through the
//! privilege tool. The agent itself only ever *reads* the workspace (parse
//! `.flowz.yaml`, upload artifacts), and those files are world-readable, so the
//! agent never needs write access.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model::Run;

#[derive(Clone)]
pub enum WorkspaceBackend {
    Dir(PathBuf),
    /// Parent dataset that holds per-repo datasets and per-run clones,
    /// e.g. `zroot/flowz/ws`.
    Zfs { pool: String },
}

/// A provisioned workspace ready for the executor.
pub struct Workspace {
    /// Host path to run the job in (mountpoint of the ZFS clone, or the dir).
    pub path: PathBuf,
    cleanup: Cleanup,
}

enum Cleanup {
    Dir { priv_tool: String },
    Zfs {
        priv_tool: String,
        clone_ds: String,
        snapshot: String,
    },
}

/// Provision a workspace for `run`. `priv_tool` is the privilege escalation
/// command (e.g. `doas`/`sudo`) used by the ZFS backend.
pub fn prepare(backend: &WorkspaceBackend, run: &Run, priv_tool: &str) -> Result<Workspace> {
    match backend {
        WorkspaceBackend::Dir(dir) => prepare_dir(dir, run, priv_tool),
        WorkspaceBackend::Zfs { pool } => prepare_zfs(pool, priv_tool, run),
    }
}

impl Workspace {
    /// Tear down the workspace. Best-effort: failures are logged, not returned,
    /// since the job result has already been reported.
    pub fn cleanup(self, run_id: &str) {
        match self.cleanup {
            Cleanup::Dir { priv_tool } => cleanup_dir(run_id, &self.path, &priv_tool),
            Cleanup::Zfs {
                priv_tool,
                clone_ds,
                snapshot,
            } => cleanup_zfs(run_id, &priv_tool, &clone_ds, &snapshot),
        }
    }
}

// ---- Dir backend ----

fn prepare_dir(dir: &Path, run: &Run, priv_tool: &str) -> Result<Workspace> {
    let path = dir.join(&run.id);
    std::fs::create_dir_all(&path)
        .with_context(|| format!("create workspace {}", path.display()))?;

    let status = Command::new("git")
        .args(["clone", "--depth=1", &run.clone_url, "."])
        .current_dir(&path)
        .status()
        .context("spawn git clone")?;
    if !status.success() {
        anyhow::bail!("git clone exited with {status}");
    }

    Ok(Workspace {
        path,
        cleanup: Cleanup::Dir {
            priv_tool: priv_tool.to_string(),
        },
    })
}

/// Remove a directory workspace. Files built inside a dail jail are owned by
/// root, so a plain remove hits EACCES — fall back to a privileged `rm -rf`.
fn cleanup_dir(run_id: &str, path: &Path, priv_tool: &str) {
    if !path.exists() {
        return;
    }
    match remove_dir_all_force(path) {
        Ok(()) => return,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => tracing::warn!(run_id, "cleanup workspace: {e}"),
    }
    // Privileged fallback for root-owned files left by a dail jail.
    match Command::new(priv_tool).arg("rm").arg("-rf").arg(path).status() {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(run_id, "cleanup workspace via {priv_tool}: exit {s}"),
        Err(e) => tracing::warn!(run_id, "cleanup workspace via {priv_tool}: {e}"),
    }
}

// git sets .git/objects dirs to 0555; make everything writable before removal.
fn remove_dir_all_force(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
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

// ---- ZFS backend ----

fn prepare_zfs(pool: &str, priv_tool: &str, run: &Run) -> Result<Workspace> {
    let slug = sanitize(&run.repo);
    let repo_ds = format!("{pool}/{slug}");
    let clone_ds = format!("{pool}/run-{}", run.id);
    let snapshot = format!("{repo_ds}@run-{}", run.id);

    ensure_repo_dataset(priv_tool, &repo_ds, run)?;

    zfs(priv_tool, &["snapshot", &snapshot]).context("zfs snapshot")?;
    if let Err(e) = zfs(priv_tool, &["clone", &snapshot, &clone_ds]) {
        // snapshot succeeded but clone failed — don't leak the snapshot
        let _ = zfs(priv_tool, &["destroy", &snapshot]);
        return Err(e).context("zfs clone");
    }

    let path = mountpoint(priv_tool, &clone_ds).context("resolve clone mountpoint")?;

    Ok(Workspace {
        path,
        cleanup: Cleanup::Zfs {
            priv_tool: priv_tool.to_string(),
            clone_ds,
            snapshot,
        },
    })
}

/// Create the per-repo dataset and clone the repo on first use; otherwise fetch
/// and check out the target commit, preserving untracked files (the build cache).
fn ensure_repo_dataset(priv_tool: &str, repo_ds: &str, run: &Run) -> Result<()> {
    if !dataset_exists(priv_tool, repo_ds) {
        zfs(priv_tool, &["create", "-p", repo_ds]).context("zfs create repo dataset")?;
        let mp = mountpoint(priv_tool, repo_ds).context("resolve repo mountpoint")?;
        let mp = mp.to_string_lossy();
        git(priv_tool, &["clone", &run.clone_url, &mp]).context("git clone")?;
        git(priv_tool, &["-C", &mp, "checkout", "-f", &run.commit]).context("git checkout")?;
        return Ok(());
    }

    let mp = mountpoint(priv_tool, repo_ds).context("resolve repo mountpoint")?;
    let mp = mp.to_string_lossy();
    git(priv_tool, &["-C", &mp, "fetch", "--all", "--prune"]).context("git fetch")?;
    // If commit is a real SHA use it directly; otherwise (e.g. "HEAD" from the
    // build button) reset to the remote tracking branch so we get latest code.
    let checkout_ref = if run.commit.len() == 40
        && run.commit.chars().all(|c| c.is_ascii_hexdigit())
    {
        run.commit.clone()
    } else {
        format!("origin/{}", run.branch)
    };
    git(priv_tool, &["-C", &mp, "checkout", "-f", &checkout_ref]).context("git checkout")?;
    Ok(())
}

fn cleanup_zfs(run_id: &str, priv_tool: &str, clone_ds: &str, snapshot: &str) {
    // Destroy the clone first (it depends on the snapshot), then the snapshot.
    if let Err(e) = zfs(priv_tool, &["destroy", "-r", clone_ds]) {
        tracing::warn!(run_id, "cleanup: zfs destroy {clone_ds}: {e:#}");
    }
    if let Err(e) = zfs(priv_tool, &["destroy", snapshot]) {
        tracing::warn!(run_id, "cleanup: zfs destroy {snapshot}: {e:#}");
    }
}

// ---- low-level helpers ----

/// Sanitize `owner/repo` into a single ZFS dataset component.
fn sanitize(repo: &str) -> String {
    repo.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn dataset_exists(priv_tool: &str, ds: &str) -> bool {
    Command::new(priv_tool)
        .args(["zfs", "list", "-H", ds])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn mountpoint(priv_tool: &str, ds: &str) -> Result<PathBuf> {
    let out = Command::new(priv_tool)
        .args(["zfs", "get", "-H", "-o", "value", "mountpoint", ds])
        .output()
        .with_context(|| format!("spawn zfs get mountpoint {ds}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "zfs get mountpoint {ds} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mp = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if mp.is_empty() || mp == "none" || mp == "-" {
        anyhow::bail!("dataset {ds} has no mountpoint ({mp})");
    }
    Ok(PathBuf::from(mp))
}

fn zfs(priv_tool: &str, args: &[&str]) -> Result<()> {
    priv_cmd(priv_tool, "zfs", args)
}

fn git(priv_tool: &str, args: &[&str]) -> Result<()> {
    priv_cmd(priv_tool, "git", args)
}

fn priv_cmd(priv_tool: &str, bin: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(priv_tool)
        .arg(bin)
        .args(args)
        .output()
        .with_context(|| format!("spawn {priv_tool} {bin} {}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "{priv_tool} {bin} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}
