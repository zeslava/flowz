# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
# Build
cargo build --workspace

# Tests
cargo test

# Lint
cargo clippy --workspace -- -D warnings

# Format
cargo fmt --all

# Run server (reads flowz-server.yaml)
task server

# Run agent (reads flowz-agent.yaml)
task agent

# Send a test webhook
task webhook REPO=owner/repo BRANCH=main SHA=abc123...

# Check runs via API
task runs
```

End-to-end verification (Linux, StubExecutor):
1. `cargo build --workspace && cargo test`
2. `task server` in one terminal, `task agent` in another
3. `task webhook` — sends a signed GitHub push payload to `http://localhost:7878/webhook/github`
4. Confirm run appears at `GET /api/runs` and in the UI at `http://localhost:7878/`

## Architecture

Single flat crate (`flowz`) with three binaries. Not a multi-crate workspace.

| Module | Role |
|--------|------|
| `src/model/` | Domain types: `Run`, `StepRecord`, `RunStatus`, `StepStatus` |
| `src/pipeline/` | `.flowz.yaml` parser + validator, `Executor` port trait |
| `src/executor/` | `StubExecutor` (Linux dev); `DailExecutor` deferred to FreeBSD |
| `src/webhook/` | GitHub HMAC-SHA256 verification + push event parsing |
| `src/store/` | SQLite adapter via `sqlx` (runtime queries, no compile-time macros) |
| `src/server/` | axum HTTP server, UI routes (`GET /`, `GET /runs/{id}`), API |
| `src/agent/` | Long-poll worker: claim job → git clone → run steps → POST logs/status |
| `src/cli/` | CLI commands — stub, not yet implemented |

**Agent ↔ server protocol:** long-poll `GET /api/jobs/poll` for job claim;
`POST /api/runs/{id}/steps/{step}/logs` for log ingest;
`POST /api/runs/{id}/status` for completion.

**GitHub commit status:** agent posts `success|failure` to GitHub Statuses API after each run.
Token flows from `flowz-server.yaml` → job payload → agent.

## Key conventions

- Errors: `thiserror` in lib modules, `anyhow` in binaries (`src/bin/`).
- YAML parsing: `serde_yaml_ng` (maintained fork of serde-yaml).
- Step order: `indexmap` preserves `.flowz.yaml` declaration order.
- HTML templates: `askama` (compile-time), files in `templates/`.
- Logging: `tracing` + `tracing-subscriber`.
- Config: YAML (`flowz-server.yaml` / `flowz-agent.yaml`) or env vars.
- axum v0.8 route syntax: `{param}` not `:param`.
- SQLite: `"commit"` column is quoted (reserved keyword). `create_if_missing(true)` in store.

## StubExecutor behaviour

- Runs `sh <run_file>` with `current_dir` set to the cloned workspace.
- Captures **both stdout and stderr** — stderr is drained in a separate thread to avoid pipe deadlock, then emitted after stdout.
- After each step, the agent appends a synthetic `✓ exit 0` / `✗ exit N` log line.
- Infrastructure errors (clone failure, missing `.flowz.yaml`) are posted as an `agent` step log and the run is marked `failure`.

## CI

flowz builds itself. `.flowz.yaml` + `ci/` are committed. Steps: `build` → `test` + `lint` (parallel after build).
Scripts are plain `sh` — no `.dail` extension needed for `StubExecutor`.

## What is NOT implemented yet

- CLI commands (`flowz validate`, `flowz status`, `flowz logs`, etc.) — stub only.
- SSE live log streaming in the browser — UI shows static logs only.
- `DailExecutor` — requires FreeBSD 14+ and dail runtime.

## Platform note

All v0.1 development uses `StubExecutor` (`sh <run_file>` in workspace directory).
`DailExecutor` and rc.d/port packaging require a FreeBSD 14+ host.
