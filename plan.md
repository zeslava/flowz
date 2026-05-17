# flowz v0.1 MVP — Implementation Plan

## Context

The repo is empty (only `SPEC.md`, `README.md`, `LICENSE`, `.gitignore`). `SPEC.md`
is the full design doc. This plan implements **v0.1 MVP** (SPEC §5.1, definition of
done in §10): a single FreeBSD host can receive a GitHub push, run a `.flowz.yaml`
pipeline through `dail`-isolated steps, stream logs to a browser, and post status
back to GitHub.

Constraints driving the design:
- **dail is not built yet** and jails are FreeBSD-only. The dev machine is Linux.
- Confirmed decisions: dail is called as a **subprocess**; an **`Executor` port** with
  a real `DailExecutor` and a `StubExecutor` lets everything except real jail
  execution be built and tested on Linux.

## Simplifications vs SPEC for v0.1 (deferred to later versions)

- **No DAG** — steps run in declared (file) order. `needs` and cycles are still
  *validated*, just not used for parallel scheduling. (DAG → v0.2)
- **No ZFS snapshots** — workspace is a plain directory. (time-travel → v0.3)
- **Agent→server logs via plain HTTP POST chunks**, not WebSocket.
- **No secrets** (→ v0.2), **no artifacts** (→ v0.2), **no `flowz run --from`** (→ v0.3).

## Architecture

Single flat crate (`flowz`) with logical modules, three binaries. Not a multi-crate
workspace (kept simple for v0.1).

```
flowz/
├── Cargo.toml
├── rust-toolchain.toml
├── src/
│   ├── lib.rs
│   ├── model/mod.rs        # domain types
│   ├── pipeline/mod.rs     # .flowz.yaml parser + validator
│   ├── executor/mod.rs     # Executor trait, StubExecutor
│   ├── webhook/mod.rs      # GitHub HMAC verify + push parse
│   ├── store/mod.rs        # SQLite adapter (sqlx)
│   ├── server/mod.rs       # axum HTTP server, UI routes
│   ├── agent/mod.rs        # long-poll worker
│   ├── cli/mod.rs          # CLI commands (stub)
│   └── bin/
│       ├── server.rs
│       ├── agent.rs
│       └── cli.rs
├── templates/
│   ├── runs.html           # askama: run list
│   └── run.html            # askama: run detail + logs
├── migrations/
│   └── 0001_init.sql
├── examples/
│   └── .flowz.yaml
└── flowz-server.yaml / flowz-agent.yaml  # local dev configs
```

### Stack

| Concern        | Crate |
|----------------|-------|
| async runtime  | `tokio` |
| HTTP server    | `axum` |
| HTTP client    | `reqwest` |
| SQLite         | `sqlx` (runtime queries, no compile-time macros) |
| YAML           | `serde` + `serde_yaml_ng` |
| ordered steps  | `indexmap` |
| CLI args       | `clap` (derive) |
| HMAC verify    | `hmac` + `sha2` |
| HTML UI        | `askama` (compile-time templates) |
| logging        | `tracing` + `tracing-subscriber` |
| config         | YAML + `serde` (via `serde_yaml_ng`) |
| errors         | `thiserror` in lib modules, `anyhow` in binaries |

## Phases

### Phase 0 — Workspace skeleton ✅
- `Cargo.toml`, `rust-toolchain.toml`, stub modules.
- `examples/.flowz.yaml` + `examples/ci/*.dail`.
- `cargo build --workspace` succeeds.

### Phase 1 — Domain + pipeline parser ✅
Files: `src/model/mod.rs`, `src/pipeline/mod.rs`, `src/executor/mod.rs`
- `PipelineFile`, `Step`, `Trigger`, `Run`, `StepRecord`, `RunStatus`/`StepStatus`.
- `parse` + `validate`: YAML, version check, path validation, `needs` refs, cycle detection.
- `Executor` trait + `StepContext`/`StepOutcome`.
- Unit tests in `src/pipeline/mod.rs`.

### Phase 2 — Executor adapters ✅ (partial)
Files: `src/executor/mod.rs`
- `StubExecutor`: runs `sh <run_file>` in workspace — Linux dev/testing. ✅
- `DailExecutor`: deferred — requires FreeBSD 14+ and dail runtime.

### Phase 3 — `flowz-server` ✅
Files: `src/store/mod.rs`, `src/server/mod.rs`, `src/webhook/mod.rs`, `src/bin/server.rs`,
`migrations/0001_init.sql`
- SQLite schema: `runs`, `steps`, `log_lines`. Migrations via `sqlx::migrate!`.
  - Note: `"commit"` column is quoted (reserved SQLite keyword).
- `Store`: sqlx adapter — create/get/list runs, steps, logs; `claim_next_job`.
- GitHub webhook: HMAC-SHA256 verify + push event parsing.
- HTTP API (axum, v0.8 `{param}` route syntax):
  - `POST /webhook/github`
  - `GET /api/jobs/poll` — long-poll (25s timeout, 2s interval)
  - `POST /api/runs/{id}/steps/{step}/logs`
  - `POST /api/runs/{id}/status`
  - `GET /api/runs`, `GET /api/runs/{id}`, `GET /api/runs/{id}/logs`
- Config: YAML (`flowz-server.yaml`) or `FLOWZ_WEBHOOK_SECRET` env var.
  - SQLite file created automatically (`create_if_missing(true)`).

### Phase 4 — `flowz-agent` ✅
Files: `src/agent/mod.rs`, `src/bin/agent.rs`
- Config: YAML (`flowz-agent.yaml`) or `FLOWZ_SERVER_URL` env var.
- Long-poll loop: `GET /api/jobs/poll` → claim → `git clone --depth=1` →
  parse `.flowz.yaml` → run steps via `StubExecutor` →
  POST log chunks + per-step status → POST final run status → cleanup.
- Honors `only.branch` (skipped steps → `StepStatus::Skipped`).

### Phase 5 — GitHub commit status ✅
Files: `src/agent/mod.rs`
- After run completes: `POST /repos/{owner}/{repo}/statuses/{sha}` with `state=success|failure`.
- GitHub token flows from server config → job payload → agent.

### Phase 6 — Basic UI ✅
Files: `templates/runs.html`, `templates/run.html`, `src/server/mod.rs`
- `GET /` — run list (askama).
- `GET /runs/{id}` — run detail with steps and logs (static, no SSE yet).

---

## SLC — Simple, Lovable, Complete

### Phase 7 — `flowz-cli` ❌ (stub only)
Files: `src/cli/mod.rs`, `src/bin/cli.rs`
- `clap` derive. Commands: `validate`, `status`, `logs`, `run`, `init`.
- Currently a stub — no commands implemented.

### Phase 8 — SSE live logs ❌
Files: `src/server/mod.rs`, `templates/run.html`
- `GET /runs/{id}/events` — SSE stream: new log lines + step status changes.
- Browser receives live updates without page reload.

---

## Post-SLC

### Phase 9 — Packaging & docs
- `rc.d/flowz` service script; FreeBSD port skeleton.
- `docs/` mdBook scaffold.
- Dogfood `.flowz.yaml` + `ci/*.dail` for flowz's own CI.

### Phase 10 — `DailExecutor` (FreeBSD only)
- Spawns `dail run <file>`, streams stdout/stderr, captures exit code.
- Requires FreeBSD 14+ and dail runtime.

## Risks / open items

- **dail does not exist yet.** All v0.1 development uses `StubExecutor`.
- **No FreeBSD here** — jail execution, rc.d, and the port are not testable on Linux.
- `sqlx` runtime queries used — build needs no live DB.

## Verification

On Linux with `StubExecutor`:
1. `cargo build --workspace && cargo test -p flowz-core` (use `cargo test` for flat crate)
2. `flowz validate examples/.flowz.yaml`
3. `task server` + `task agent`, then `task webhook` to send a signed push payload.
4. Confirm: run in `GET /api/runs` and at `http://localhost:7878/`; logs visible in browser.
5. `flowz status` and `flowz logs <run-id>` — pending Phase 7.

On FreeBSD (post-dail): swap backend to `dail`, repeat 3–4, verify GitHub commit status posted.
