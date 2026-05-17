# flowz — Technical Specification

> Blazing fast CI/CD for FreeBSD.

## 1. Overview

`flowz` is a CI/CD engine built natively for FreeBSD. It uses [`dail`](https://github.com/zeslava/dail) as its execution backend, running each pipeline step in an ephemeral, isolated FreeBSD jail. There is no Docker, no registry, no daemon — pipelines are defined as code in git, distributed via git, and executed via `dail`.

**Slogan:** *Git push → pipeline in jail → deployed.*

## 2. Positioning

### 2.1. Target audience

In order of priority:

1. **Homelab enthusiasts** running FreeBSD servers who want continuous deployment for self-hosted services (Nextcloud, Jellyfin, Gitea, etc.).
2. **Solo developers** with FreeBSD-based pet projects who need a simple CI pipeline without standing up Drone/Woodpecker/Jenkins.
3. **Small teams** building FreeBSD-native software (ports, system tools, BSD-focused products) who need reproducible builds and deploys.

Non-goals (explicitly out of scope for v1):

- Enterprise features (RBAC, audit logs, SSO, multi-tenancy).
- Linux executor support.
- Kubernetes integration.
- Cross-platform support of any kind.

### 2.2. Differentiation from Drone, Woodpecker, GitLab CI, GitHub Actions

`flowz` is opinionated about being FreeBSD-native:

- **Pipeline steps are `.dail` files.** No new container syntax to learn — if you know `dail`, you know `flowz`. The `.flowz.yaml` only describes orchestration (order, triggers, conditions).
- **ZFS time-travel debugging.** Each pipeline step produces a ZFS snapshot. Failed at step 4 of 7? Don't rerun from scratch — clone the snapshot after step 3, fix, retry step 4. Other CI systems can't do this because they don't have ZFS.
- **Identical local and CI execution.** `flowz run` on a developer laptop uses the same `dail` invocation as the server. No "works in CI, fails locally" drift.
- **Zero infrastructure.** Single binary, SQLite for state, embedded webhook receiver. No PostgreSQL, no Redis, no S3, no message broker.
- **Git is the only source of truth.** Code, pipeline definition, jail config, secrets (encrypted via age/sops) — all in one repo.

### 2.3. Technology stack

- **Language:** Rust.
- **Platform:** FreeBSD 14+ (with `dail` as required runtime dependency).
- **State storage:** SQLite for pipeline metadata, ZFS datasets for workspace and step snapshots.
- **Distribution:** binary release via GitHub, FreeBSD port (long-term goal).
- **No external services required.**

## 3. Architecture

### 3.1. Components

```
┌─────────────────────────────────────────────────────────┐
│                      flowz-server                       │
│                                                         │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌────────┐  │
│  │ webhook  │  │  HTTP    │  │ scheduler│  │ SQLite │  │
│  │ receiver │  │  API     │  │          │  │  DB    │  │
│  └──────────┘  └──────────┘  └──────────┘  └────────┘  │
│       │             │              │           │       │
└───────┼─────────────┼──────────────┼───────────┼───────┘
        │             │              │           │
        ▼             ▼              ▼           ▼
   git provider   web UI         agents      state
    (push)        (browser)    (long poll)
                                    │
                                    ▼
                          ┌─────────────────┐
                          │   flowz-agent   │
                          │                 │
                          │  ┌───────────┐  │
                          │  │   dail    │  │
                          │  │ (subprocess)│
                          │  └───────────┘  │
                          └─────────────────┘
```

#### 3.1.1. `flowz-server`

Single binary, runs as rc.d service. Responsibilities:

- HTTP server on configurable port (default `:7878`).
- Webhook endpoints for GitHub, Gitea, Forgejo, GitLab.
- REST/JSON API for agents and CLI.
- Scheduler: assigns queued jobs to available agents.
- SQLite database for pipelines, runs, logs metadata.
- Embedded web UI (static files served from binary).
- Optional: serves git over HTTP for self-hosted repos.

#### 3.1.2. `flowz-agent`

Runs on each FreeBSD host that can execute jobs. Responsibilities:

- Long-poll `flowz-server` for assigned jobs.
- Clone source repo into ephemeral ZFS workspace.
- Invoke `dail` for each pipeline step.
- Stream logs back to server.
- Manage step-level ZFS snapshots for retry/debug.
- Report status (success, failure, exit code).
- Self-cleanup on completion or termination.

Single-host deployments run server + agent on the same machine. Multi-host scales by adding agents.

#### 3.1.3. `flowz` CLI

User-facing command-line tool. Commands:

- `flowz init` — scaffold `.flowz.yaml` in current repo.
- `flowz run [pipeline]` — execute pipeline locally (same path as agent uses).
- `flowz validate` — lint pipeline file.
- `flowz status` — show recent runs (queries server).
- `flowz logs <run-id>` — stream/dump logs.
- `flowz rollback <deploy-id>` — restore previous deploy state.
- `flowz secret set/get/list` — manage secrets.

### 3.2. Communication

- **Agent ↔ Server:** HTTP long polling for job assignment, WebSocket for log streaming.
- **CLI ↔ Server:** REST/JSON API.
- **Webhook providers → Server:** standard webhook HTTP POST.
- **Server → User browser:** HTTP + Server-Sent Events for live log/status updates.

No external message broker. No gRPC in v0 — kept simple.

### 3.3. State model

SQLite schema (high level):

- `repos` — connected repositories with auth credentials.
- `pipelines` — parsed pipeline definitions per commit.
- `runs` — pipeline execution records (status, started_at, finished_at).
- `steps` — individual step records within a run.
- `logs` — log line index (actual log content on disk).
- `agents` — registered agents and their capabilities.
- `secrets` — encrypted secret blobs scoped to repo.

## 4. Pipeline format

### 4.1. File location

`.flowz.yaml` (or `.flowz.yml`) in repository root. Override with `--config` flag.

### 4.2. Schema

```yaml
# .flowz.yaml
version: 1

on:
  push:
    branches: [main, develop]
  pull_request:
    branches: [main]
  schedule:
    - cron: "0 3 * * *"

env:
  RUST_LOG: info

pipeline:
  test:
    run: ci/test.dail
    timeout: 10m

  build:
    run: ci/build.dail
    needs: [test]
    artifacts:
      - target/release/myapp

  deploy:
    run: ci/deploy.dail
    needs: [build]
    only:
      branch: main
    env:
      DEPLOY_HOST: prod.example.com
    secrets:
      - SSH_KEY
```

### 4.3. Key design decisions

- **Each step is `dail run <file>`.** No `image:` or `container:` field — that lives inside the `.dail` file.
- **`needs:` defines DAG.** Steps without dependencies run in parallel. Otherwise topological order.
- **Workspace is shared.** ZFS dataset mounted into each step's jail at `/workspace`. Files written in one step are visible to the next.
- **Artifacts are explicit.** Listed paths get copied to server-side storage after step completes.
- **Secrets are referenced by name, injected as env vars** into the jail via `dail`'s secrets API.

### 4.4. Local execution parity

```sh
flowz run                  # run entire pipeline locally
flowz run test             # run single step
flowz run --from build     # resume from a step (using ZFS snapshot)
```

Local execution must invoke `dail` with identical parameters to agent execution. The only difference is that workspace is the current directory instead of a cloned repo.

## 5. Roadmap

### 5.1. v0.1 — MVP (target: 2–3 months)

Goal: usable for a single FreeBSD homelab host running a few pet projects.

- [ ] Project skeleton, CI for flowz itself (dogfooding via dail).
- [ ] `.flowz.yaml` parser with validation.
- [ ] Single-binary server with embedded SQLite.
- [ ] Webhook receiver for GitHub (most common).
- [ ] Agent that clones repo, invokes `dail`, reports status.
- [ ] Sequential pipeline execution (no DAG yet).
- [ ] CLI: `init`, `run`, `validate`, `status`, `logs`.
- [ ] Minimal HTML UI (read-only list of runs, log viewer).
- [ ] rc.d scripts and FreeBSD port packaging.
- [ ] Documentation site (mdBook or similar).

**Success criterion:** push to a git repo with `.flowz.yaml`, see pipeline execute, view logs in browser.

### 5.2. v0.2 — DAG, secrets, artifacts (target: +2 months)

- [ ] DAG execution with `needs:` (parallel where possible).
- [ ] Secrets storage (age encryption, per-repo keys).
- [ ] Artifact upload/download between steps and per run.
- [ ] Gitea and Forgejo webhook support.
- [ ] Pull request pipelines.
- [ ] Status posting back to git provider (commit checks).

### 5.3. v0.3 — ZFS time-travel (target: +2 months)

The flagship differentiating feature.

- [ ] Per-step ZFS snapshot of workspace + jail rootfs.
- [ ] `flowz run --from <step>` resumes from snapshot.
- [ ] Web UI: "retry from step" button.
- [ ] `flowz debug <run-id>` opens shell in the failed step's jail.
- [ ] GC policy for old snapshots (configurable retention).

### 5.4. v0.4 — Multi-host (target: +2 months)

- [ ] Agent registration and capability matching (labels).
- [ ] Job distribution across multiple agents.
- [ ] Per-agent concurrency limits.
- [ ] Cross-host artifact transfer (via `zfs send`/`recv` when possible).
- [ ] Agent auto-update.

### 5.5. v1.0 — Production polish

- [ ] Stable API.
- [ ] Comprehensive docs with tutorials.
- [ ] Backup/restore tooling.
- [ ] Metrics endpoint (Prometheus format).
- [ ] Performance benchmarks vs Drone/Woodpecker.
- [ ] Showcase deployments.

### 5.6. Post-v1.0 ideas

- GitHub Actions self-hosted runner mode (flowz-agent posing as GHA runner).
- Matrix builds.
- Reusable pipeline components.
- Built-in caching primitives.
- Web UI: pipeline graph visualization.
- Pluggable executors (could enable Linux jails via Linuxulator, bhyve VMs).

## 6. Dependencies on `dail`

`flowz` requires features in `dail` that may not yet exist. Coordination needed:

| Feature | Status | Blocks flowz at |
|---------|--------|-----------------|
| `dail.lock` for reproducibility | TBD | v0.1 (reproducible CI) |
| Ephemeral jails with guaranteed cleanup | TBD | v0.1 (every CI step) |
| Secrets injection API | TBD | v0.2 |
| ZFS snapshot per step | TBD | v0.3 |
| Workspace mounts | partial (volumes exist) | v0.1 |
| Stable subprocess API | TBD | v0.1 |
| Streaming exec with exit codes | partial | v0.1 |

See `dail-ROADMAP.md` for the corresponding work plan on the `dail` side.

## 7. Open questions

- **Library vs subprocess?** Should `flowz-agent` link `dail` as a Rust library or call it via subprocess? Subprocess is simpler and more loosely coupled; library gives better error handling and streaming. Decide during v0.1 prototype.
- **Secrets storage location.** Encrypted in SQLite, or in separate per-repo files? Sops-style "decrypt on read" or in-memory after agent receives job?
- **PR pipelines and forks.** How to handle pipelines triggered by external contributors (security implications of running their code)?
- **Pipeline-as-code vs pipeline-in-UI.** Stay strictly file-based, or allow some UI configuration (env vars, manual triggers)?
- **Notification channels.** Built-in (matrix, telegram, email), webhook out, or both?

## 8. Out of scope (forever)

These are explicit non-goals to keep focus:

- Web-based pipeline editor.
- Built-in artifact registry (use external S3/MinIO if needed).
- Plugin marketplace.
- Hosted/SaaS mode.
- Windows agent.
- Multi-tenancy with isolated organizations.
- Replacing `dail` functionality (build, run, registry). `flowz` orchestrates; `dail` executes.

## 9. Repository layout (proposed)

```
flowz/
├── Cargo.toml              # workspace
├── crates/
│   ├── flowz-cli/          # user CLI binary
│   ├── flowz-server/       # server binary
│   ├── flowz-agent/        # agent binary
│   ├── flowz-core/         # shared types, pipeline parser
│   ├── flowz-dail/         # dail interface (lib or subprocess wrapper)
│   └── flowz-webhook/      # webhook parsers per provider
├── ui/                     # web UI (static, served by server)
├── docs/                   # mdBook source
├── examples/               # sample .flowz.yaml + .dail files
├── port/                   # FreeBSD port files
├── rc.d/                   # rc.d scripts
└── README.md
```

## 10. First milestone definition of done

**v0.1 ships when:**

- A user can `pkg install flowz` (or download binary) on a FreeBSD 14+ host.
- Run `flowz server init && service flowz start` and have a working server.
- Connect a GitHub repo via webhook.
- Push a commit with `.flowz.yaml` + `.dail` files.
- See the pipeline execute in the browser.
- View streamed logs.
- See success/failure status posted back to GitHub.

Everything else is post-v0.1.
