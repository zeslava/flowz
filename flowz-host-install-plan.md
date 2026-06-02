# План: подготовка flowz к установке на хост (FreeBSD)

## Context

Обсудили архитектурный вопрос «запускать flowz через dail или на хосте». Вывод:

- **flowz-agent должен жить на хосте**, не внутри jail. Он вызывает `dail run`
  (создание jail, ZFS clone, PF rdr, volume-mount) — для этого нужны хостовые
  привилегии, которых изнутри обычного jail нет. Запускать управляющий dail-процесс
  внутри dail — инверсия зависимости.
- **flowz-server** (HTTP + SQLite + UI) dail не нужен — его можно держать в jail
  (это attack surface: webhook'и из интернета, GitHub-токены), но для MVP допустимо
  и на хосте. Сервер и агент — два независимых сервиса.
- Изоляцию даёт jail-per-job (`dail run`), а не jail вокруг агента.

Сейчас в репозитории нет `deploy/`. Цель — добавить FreeBSD deploy-обвязку по
образцу соседнего проекта `../filest/deploy/` (rc.d + env/конфиг + install-скрипт),
адаптированную под специфику flowz (два сервиса, отдельный юзер, доступ к `dail`).

Эталон, с которого копируем паттерн: `/home/slava/dev/github.com/zeslava/filest/deploy/`
(`install-freebsd.sh`, `filest.rc`, `filest.env`).

## Факты по текущему коду

Бинарь один (`Cargo.toml`): `flowz` (`src/main.rs`) с сабкомандами
`flowz server`, `flowz agent`, плюс заглушки `validate`/`status`/`logs`.

Конфиг читается из YAML по пути из env-переменной, с фолбэком на env-переменные:
- server: `FLOWZ_CONFIG` → `flowz-server.yaml`. Поля: `listen` (default
  `0.0.0.0:7878`), `db` (default `flowz.db`), `artifacts_dir`
  (default `/var/db/flowz/artifacts`), `webhook_secret` (required),
  `github_token` (optional). Фолбэк-env: `FLOWZ_WEBHOOK_SECRET`, `FLOWZ_GITHUB_TOKEN`.
- agent: `FLOWZ_AGENT_CONFIG` → `flowz-agent.yaml`. Поля: `server_url` (required),
  `agent_name` (default hostname), `workspace_dir` (default `/tmp/flowz-workspace`),
  `executor` (`stub`|`dail`). Фолбэк-env: `FLOWZ_SERVER_URL`.

Это значит: rc.d-скрипты просто экспортируют `FLOWZ_CONFIG` / `FLOWZ_AGENT_CONFIG`
перед запуском бинаря — менять Rust-код не нужно.

## Файлы (новые, всё в `deploy/`)

```
deploy/
  install-freebsd.sh        # юзер, директории, sudoers, один бинарь flowz, оба rc, rc.conf
  flowz-server.rc           # rc.d сервис сервера
  flowz-agent.rc            # rc.d сервис агента
  flowz-server.yaml         # пример конфига сервера (keep-existing при апдейте)
  flowz-agent.yaml          # пример конфига агента
  flowz-agent.sudoers       # whitelist команд dail для юзера flowz
```

### `flowz-server.rc`
По скелету `filest.rc` (daemon + pidfile + log), но:
- `name="flowz_server"`, `rcvar="flowz_server_enable"`, юзер `: ${flowz_server_user:="flowz"}`.
- В `*_start` экспортировать `FLOWZ_CONFIG=/usr/local/etc/flowz/server.yaml` перед `exec`:
  ```sh
  /usr/sbin/daemon -f -P ${pidfile} -o ${flowz_server_log} \
    /usr/bin/su -m ${flowz_server_user} -c \
    "export FLOWZ_CONFIG=${flowz_server_config}; exec ${command}"
  ```
- `command="/usr/local/bin/flowz"`, `command_args="server"`, pidfile `/var/run/flowz/server.pid`,
  log `/var/log/flowz/server.log`.
- `REQUIRE: NETWORKING`.

### `flowz-agent.rc`
Аналогично, но:
- `name="flowz_agent"`, `rcvar="flowz_agent_enable"`.
- `command="/usr/local/bin/flowz"`, `command_args="agent"`, экспорт `FLOWZ_AGENT_CONFIG`,
  pidfile `/var/run/flowz/agent.pid`, log `/var/log/flowz/agent.log`.
- `REQUIRE: NETWORKING flowz_server` (агент long-poll'ит сервер; если сервер
  на другой машине — убрать зависимость, оставить только NETWORKING).

### `flowz-server.yaml` (пример)
```yaml
listen: "0.0.0.0:7878"
db: "/var/db/flowz/flowz.db"
artifacts_dir: "/var/db/flowz/artifacts"
webhook_secret: "CHANGE_ME"
# github_token: "ghp_..."
```

### `flowz-agent.yaml` (пример)
```yaml
server_url: "http://127.0.0.1:7878"
# agent_name: "host1"
workspace_dir: "/var/db/flowz/work"
executor: "dail"     # на хосте FreeBSD; stub — для Linux dev
```

### `flowz-agent.sudoers`
Whitelist из §8 deploy-spec (`dail destroy` намеренно отсутствует):
```
flowz ALL=(root) NOPASSWD: /usr/local/bin/dail run *, /usr/local/bin/dail stop *, /usr/local/bin/dail ps, /usr/local/bin/dail logs *
```
Устанавливается в `/usr/local/etc/sudoers.d/flowz-agent`, `chmod 440`,
с проверкой `visudo -cf` перед активацией.

### `install-freebsd.sh`
По образцу `filest/deploy/install-freebsd.sh`, расширенный:
1. Проверить наличие `target/release/flowz`
   (подсказать `cargo build --release` при отсутствии).
2. Создать юзера/группу `flowz`:
   ```sh
   pw groupshow flowz >/dev/null 2>&1 || pw groupadd flowz
   pw usershow  flowz >/dev/null 2>&1 || \
     pw useradd flowz -g flowz -d /nonexistent -s /usr/sbin/nologin
   ```
3. Создать датадиры с `chown flowz:flowz`:
   `/var/db/flowz`, `/var/db/flowz/work`, `/var/db/flowz/artifacts`,
   `/var/log/flowz`, `/var/run/flowz`.
4. Установить sudoers-дроплет (с `visudo -cf` проверкой).
5. `service flowz_server stop` / `flowz_agent stop` (|| true), затем копировать
   бинарь `flowz` в `/usr/local/bin/`, `chmod 755`.
6. Копировать оба rc в `/usr/local/etc/rc.d/`, `chmod 755`.
7. `mkdir -p /usr/local/etc/flowz`; копировать `server.yaml`/`agent.yaml`
   только если их ещё нет (keep-existing, как в filest для env).
8. Добавить в `/etc/rc.conf` `flowz_server_enable="YES"` и
   `flowz_agent_enable="YES"` (с grep-проверкой, чтобы не дублировать).
9. Финальная подсказка: отредактировать `webhook_secret` в
   `/usr/local/etc/flowz/server.yaml`, затем `service flowz_server start`.

## Зависимости / предпосылки (не код, отметить в README или выводе скрипта)
- `dail` установлен в `/usr/local/bin/dail` (для `executor: dail`).
- `sudo` установлен (`pkg install sudo`) — нужен для sudoers-whitelist.
- `git` установлен (агент клонирует репозитории).

## Verification
1. `cargo build --release` (на FreeBSD-хосте) — собирает бинарь `flowz`.
2. `sh deploy/install-freebsd.sh` от root — проверить идемпотентность
   (повторный запуск не должен ломать существующие конфиги/юзера/rc.conf).
3. `pw usershow flowz`, `ls -la /var/db/flowz` — юзер и директории с правильным
   владельцем.
4. `visudo -cf /usr/local/etc/sudoers.d/flowz-agent` — sudoers валиден.
5. Отредактировать `webhook_secret`, затем
   `service flowz_server start && service flowz_agent start`.
6. `service flowz_server status`, `curl localhost:7878/api/runs` — сервер живой.
7. `tail /var/log/flowz/agent.log` — агент стартанул и long-poll'ит.
8. (опц.) `task webhook` аналог → run появляется в `/api/runs`, агент его берёт.

## Заметки на будущее (вне scope этого плана)
- Server в отдельном jail (через `flowz.dail` по образцу `filest.dail`) — отдельная
  задача; rc.d-агент на хосте остаётся.
- `exec:`-исполнитель для deploy-шагов (StubExecutor + artifact mounts) — это
  отдельная фича парсера pipeline (`run:` vs `exec:`), не deploy-обвязка.
