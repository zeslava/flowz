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
