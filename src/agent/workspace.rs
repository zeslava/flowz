//! Job workspace provisioning.
//!
//! Two backends:
//! - [`WorkspaceBackend::Dir`] — plain directory under `workspace_dir`, cloned by
//!   the agent. Used for Linux/StubExecutor dev. Cleanup is `rm -rf` (with a
//!   privileged fallback for root-owned files left by a dail jail).
//! - [`WorkspaceBackend::Zfs`] — a per-repo "warm" dataset holds a full git
//!   clone (so runs only fetch); each job runs in an instant `zfs clone` of a
//!   snapshot of it. Cleanup is `zfs destroy`, so root-owned files inside the
//!   jail are never a problem and the original repo dataset is untouched.
//!
//! Caches (`cache:` in `.flowz.yaml`) are ZFS-only: each one is a dataset
//! under `<pool>/cache/<repo>/` whose `@latest` snapshot is cloned per run and
//! committed back only if the run succeeds, so a broken build never poisons it.
//!
//! In the ZFS backend every filesystem operation runs as root through the
//! privilege tool. The agent itself only ever *reads* the workspace (parse
//! `.flowz.yaml`, upload artifacts), and those files are world-readable, so the
//! agent never needs write access.

use anyhow::{Context, Result};
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model::Run;
use crate::pipeline;

#[derive(Clone)]
pub enum WorkspaceBackend {
    Dir(PathBuf),
    /// Parent dataset that holds per-repo datasets and per-run clones,
    /// e.g. `zroot/flowz/ws`.
    Zfs {
        pool: String,
    },
}

/// A provisioned workspace ready for the executor.
pub struct Workspace {
    /// Host path to run the job in (mountpoint of the ZFS clone, or the dir).
    pub path: PathBuf,
    cleanup: Cleanup,
    caches: Vec<CacheClone>,
}

enum Cleanup {
    Dir {
        priv_tool: String,
    },
    Zfs {
        priv_tool: String,
        clone_ds: String,
        snapshot: String,
        /// Parent of this repo's cache datasets, e.g. `zroot/flowz/ws/cache/owner_repo`.
        cache_root: String,
    },
}

/// A cache provisioned for a run, ready to be mounted into steps.
pub struct CacheMount {
    pub name: String,
    /// Path as declared in `.flowz.yaml` (normalized).
    pub path: String,
    pub host_path: PathBuf,
    /// False when the cache started empty (first run, or never committed).
    pub restored: bool,
}

/// Per-run writable clone of `base@latest`. The run works in `clone`; on
/// success the clone replaces `base`, on failure it is destroyed.
struct CacheClone {
    base: String,
    clone: String,
    restored: bool,
    /// Mounted inside the workspace via an explicit `mountpoint`.
    in_workspace: bool,
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
    /// Provision per-run clones of the pipeline's caches (`name -> path`).
    /// Only the ZFS backend supports caches; the dir backend returns `None`.
    pub fn attach_caches(
        &mut self,
        run_id: &str,
        caches: &IndexMap<String, String>,
    ) -> Result<Option<Vec<CacheMount>>> {
        let Cleanup::Zfs {
            priv_tool,
            cache_root,
            ..
        } = &self.cleanup
        else {
            return Ok(None);
        };

        if !caches.is_empty() {
            zfs(priv_tool, &["create", "-p", cache_root]).context("zfs create cache root")?;
        }

        let mut mounts = Vec::with_capacity(caches.len());
        for (name, path) in caches {
            let path = pipeline::normalize_cache_path(path);
            // '.' is not allowed in cache names, so these never collide with a base.
            let base = format!("{cache_root}/{name}");
            let clone = format!("{cache_root}/{name}.run-{run_id}");
            let latest = format!("{base}@latest");

            // Workspace caches are mounted by ZFS right into the workspace, so
            // exec: steps on the host see them as plain directories.
            let in_workspace = !path.starts_with('/');
            let mut opts: Vec<String> = Vec::new();
            if in_workspace {
                opts.push("-o".into());
                opts.push(format!("mountpoint={}", self.path.join(path).display()));
            }

            let restored = dataset_exists(priv_tool, &latest);
            let mut args: Vec<&str> = vec![if restored { "clone" } else { "create" }];
            args.extend(opts.iter().map(String::as_str));
            if restored {
                args.push(&latest);
            }
            args.push(&clone);
            zfs(priv_tool, &args).with_context(|| format!("cache '{name}': zfs {}", args[0]))?;
            self.caches.push(CacheClone {
                base,
                clone: clone.clone(),
                restored,
                in_workspace,
            });

            let host_path = mountpoint(priv_tool, &clone)
                .with_context(|| format!("cache '{name}': resolve mountpoint"))?;
            if in_workspace {
                let uid = uid_gid()?;
                priv_cmd(priv_tool, "chown", &[&uid, &host_path.to_string_lossy()])
                    .with_context(|| format!("cache '{name}': chown mountpoint"))?;
            }
            mounts.push(CacheMount {
                name: name.clone(),
                path: path.to_string(),
                host_path,
                restored,
            });
        }
        Ok(Some(mounts))
    }

    /// Tear down the workspace; on success, commit caches as the new
    /// `@latest`, otherwise discard them. Best-effort: failures are logged,
    /// not returned, since the job result has already been reported.
    pub fn finish(self, run_id: &str, success: bool) {
        match self.cleanup {
            Cleanup::Dir { priv_tool } => cleanup_dir(run_id, &self.path, &priv_tool),
            Cleanup::Zfs {
                priv_tool,
                clone_ds,
                snapshot,
                ..
            } => {
                for cache in &self.caches {
                    if success {
                        if let Err(e) = commit_cache(&priv_tool, run_id, cache) {
                            tracing::warn!(run_id, "cache commit {}: {e:#}", cache.clone);
                        }
                    } else if let Err(e) = zfs(&priv_tool, &["destroy", "-r", &cache.clone]) {
                        tracing::warn!(run_id, "cache discard {}: {e:#}", cache.clone);
                    }
                }
                cleanup_zfs(run_id, &priv_tool, &clone_ds, &snapshot);
            }
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
        caches: Vec::new(),
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
    match Command::new(priv_tool)
        .arg("rm")
        .arg("-rf")
        .arg(path)
        .status()
    {
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
    // Not under repo_ds: a child dataset would show up inside the checkout.
    let cache_root = format!("{pool}/cache/{slug}");
    let snapshot = format!("{repo_ds}@run-{}", run.id);

    ensure_repo_dataset(priv_tool, &repo_ds, run)?;

    zfs(priv_tool, &["snapshot", &snapshot]).context("zfs snapshot")?;
    if let Err(e) = zfs(priv_tool, &["clone", &snapshot, &clone_ds]) {
        // snapshot succeeded but clone failed — don't leak the snapshot
        let _ = zfs(priv_tool, &["destroy", &snapshot]);
        return Err(e).context("zfs clone");
    }

    let path = mountpoint(priv_tool, &clone_ds).context("resolve clone mountpoint")?;

    // The ZFS clone is created by root via priv_tool; chown the root of the
    // mountpoint to the current (agent) user so exec: steps can write to it.
    let uid = uid_gid()?;
    priv_cmd(priv_tool, "chown", &[&uid, &path.to_string_lossy()])
        .context("chown clone mountpoint")?;

    Ok(Workspace {
        path,
        cleanup: Cleanup::Zfs {
            priv_tool: priv_tool.to_string(),
            clone_ds,
            snapshot,
            cache_root,
        },
        caches: Vec::new(),
    })
}

/// Create the per-repo dataset and clone the repo on first use; otherwise fetch
/// and check out the target commit.
fn ensure_repo_dataset(priv_tool: &str, repo_ds: &str, run: &Run) -> Result<()> {
    if !dataset_exists(priv_tool, repo_ds) {
        zfs(priv_tool, &["create", "-p", repo_ds]).context("zfs create repo dataset")?;
        let mp = mountpoint(priv_tool, repo_ds).context("resolve repo mountpoint")?;
        // Dataset created by root; chown mountpoint so git runs as agent user.
        let uid = uid_gid()?;
        priv_cmd(priv_tool, "chown", &[&uid, &mp.to_string_lossy()])
            .context("chown repo mountpoint")?;
        let mp = mp.to_string_lossy();
        git(&["clone", &run.clone_url, &mp]).context("git clone")?;
        git(&["-C", &mp, "checkout", "-f", &run.commit]).context("git checkout")?;
        return Ok(());
    }

    let mp = mountpoint(priv_tool, repo_ds).context("resolve repo mountpoint")?;
    let mp = mp.to_string_lossy();
    git(&["-C", &mp, "fetch", "--all", "--prune"]).context("git fetch")?;
    // If commit is a real SHA use it directly; otherwise (e.g. "HEAD" from the
    // build button) reset to the remote tracking branch so we get latest code.
    let checkout_ref =
        if run.commit.len() == 40 && run.commit.chars().all(|c| c.is_ascii_hexdigit()) {
            run.commit.clone()
        } else {
            format!("origin/{}", run.branch)
        };
    git(&["-C", &mp, "checkout", "-f", &checkout_ref]).context("git checkout")?;
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

/// Make the run's clone the new cache base, snapshotted as `@latest`.
///
/// After `promote` the clone owns `@latest` and the old base becomes its
/// dependent, so the old base is renamed aside and destroyed rather than the
/// other way round. Between the two renames no base exists; a crash there
/// costs one cold run, nothing more.
fn commit_cache(priv_tool: &str, run_id: &str, cache: &CacheClone) -> Result<()> {
    let CacheClone {
        base,
        clone,
        restored,
        in_workspace,
    } = cache;
    if *in_workspace {
        // Back to the inherited mountpoint: the workspace is about to go away.
        zfs(priv_tool, &["inherit", "mountpoint", clone]).context("zfs inherit mountpoint")?;
    }
    if *restored {
        zfs(priv_tool, &["promote", clone]).context("zfs promote")?;
    }
    let old = format!("{base}.old-{run_id}");
    let had_base = dataset_exists(priv_tool, base);
    if had_base {
        zfs(priv_tool, &["rename", base, &old]).context("zfs rename base aside")?;
    }
    zfs(priv_tool, &["rename", clone, base]).context("zfs rename clone to base")?;
    if had_base {
        zfs(priv_tool, &["destroy", "-r", &old]).context("zfs destroy old base")?;
    }
    let latest = format!("{base}@latest");
    if *restored {
        // The previous @latest moved here with the promote.
        zfs(priv_tool, &["destroy", &latest]).context("zfs destroy previous @latest")?;
    }
    zfs(priv_tool, &["snapshot", &latest]).context("zfs snapshot @latest")?;
    Ok(())
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

/// Returns "uid:gid" for the current process, used for chown.
fn uid_gid() -> Result<String> {
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .context("spawn id -u")?;
    let gid = std::process::Command::new("id")
        .arg("-g")
        .output()
        .context("spawn id -g")?;
    Ok(format!(
        "{}:{}",
        String::from_utf8_lossy(&uid.stdout).trim(),
        String::from_utf8_lossy(&gid.stdout).trim()
    ))
}

fn zfs(priv_tool: &str, args: &[&str]) -> Result<()> {
    priv_cmd(priv_tool, "zfs", args)
}

fn git(args: &[&str]) -> Result<()> {
    run_cmd("git", args)
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

fn run_cmd(bin: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("spawn {bin} {}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "{bin} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}
