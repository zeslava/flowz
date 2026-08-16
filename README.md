# flowz

Blazing fast CI/CD for FreeBSD. Git push → pipeline in jail → deployed.

Built on [`dail`](https://github.com/zeslava/dail): each pipeline step runs in an ephemeral FreeBSD jail. No Docker, no registry, no daemon.

## Status

**v0.1 MVP — in development.** Core loop works on Linux with `StubExecutor`:
webhook → queue → agent → logs → UI. Real jail execution requires FreeBSD 14+ and dail.

flowz builds itself: see [`.flowz.yaml`](.flowz.yaml) and [`ci/`](ci/).

## Quick start (Linux / dev)

```sh
# 1. Build
cargo build --workspace

# 2. Start server (reads flowz-server.yaml)
task server

# 3. Start agent in another terminal (reads flowz-agent.yaml)
task agent

# 4. Send a test webhook
task webhook REPO=owner/repo BRANCH=main SHA=abc1234567890abc1234567890abc1234567890ab

# 5. Open http://localhost:7878
```

## Pipeline format

`.flowz.yaml` in your repository root:

```yaml
version: 1

triggers:
  - on: push
    branches: [main]

steps:
  build:
    run: ci/build.sh

  test:
    run: ci/test.sh
    needs: [build]

  deploy:
    run: ci/deploy.sh
    needs: [test]
    only:
      branch: main
```

Each `run:` points to a shell script executed by `sh` via `StubExecutor` (or `dail` inside a jail on FreeBSD).

## Secrets

Secrets come from [cfgy](https://github.com/zeslava/cfgy); flowz stores none of its own.

```yaml
secrets:
  provider: cfgy
  project: teka
  configurations:
    main: prod
    develop: staging

steps:
  test:
    run: ci/test.sh
    secrets: dev          # step-level override

  deploy:
    exec: ci/deploy.sh
    only: { branch: main }
    # no override -> mapping: main -> prod
```

- Resolution happens on the agent host before the step starts, via
  `cfgy list --project <p> -c <conf> -f json`. The server never sees a value —
  not in the job payload, not in the database.
- **Fail-closed:** a branch with no entry in `configurations` and no step-level
  `secrets:` gets no secrets at all.
- All configurations a run needs are resolved before the first step, so a typo
  fails in seconds (`✗ secrets: configuration 'prod' not found`).
- Key priority: `pipeline.env` → cfgy → `step.env`; what is in the YAML wins.
- Values are masked (`***`) in logs on the agent, before they are sent.
- Cron triggers have no branch, so the repo's configured branch is used.

The agent needs a cfgy token — set `cfgy_token`/`cfgy_server_url` in
`flowz-agent.yaml`, or let cfgy pick up `CFGY_TOKEN` / `CFGY_SERVER_URL` from
the agent's environment.

## Configuration

**flowz-server.yaml**
```yaml
listen: "0.0.0.0:7878"
db: flowz.db
webhook_secret: your-secret
github_token: ghp_...   # optional, for commit status
```

**flowz-agent.yaml**
```yaml
server_url: http://localhost:7878
workspace_dir: /tmp/flowz-workspaces
cfgy_bin: /usr/local/bin/cfgy   # optional, default: cfgy in PATH
cfgy_server_url: https://cfgy.example.com
cfgy_token: ...
```

Or use env vars: `FLOWZ_WEBHOOK_SECRET`, `FLOWZ_SERVER_URL`.

## Architecture

Single Rust binary (`flowz`) with three executables: `flowz-server`, `flowz-agent`, `flowz` (CLI).

```
GitHub push → POST /webhook/github
                    │
              flowz-server (SQLite, :7878)
                    │ long-poll
              flowz-agent
                    │
              git clone → parse .flowz.yaml → run steps (dail / stub)
                    │
              POST logs + status → flowz-server
                    │
              browser ← GET /runs/{id}
```

See [SPEC.md](SPEC.md) for full design and [plan.md](plan.md) for implementation status.
