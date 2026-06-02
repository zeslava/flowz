# flowz — Deploy & Run Specification

> Часть спецификации flowz, описывающая только **этап deploy**: как pipeline доставляет собранный артефакт в работающий jail на том же хосте.

## 1. Контекст

К моменту deploy предполагается, что предыдущие шаги pipeline уже отработали:

- Шаг `build` собрал артефакт (бинарь, набор файлов).
- Артефакт сохранён в `zroot/flowz/artifacts/<run_id>/build/` как ZFS dataset.
- Тесты (если есть) прошли.

Задача deploy: запустить (или заменить) долгоживущий jail на этом же хосте, используя собранный артефакт.

## 2. Модель

### 2.1. Два типа шагов в pipeline

| Тип | Где выполняется | Зачем |
|-----|----------------|-------|
| `run:` | в эфемерном jail через `dail run` | сборка, тесты — всё что требует изоляции |
| `exec:` | на хосте, в окружении flowz-agent с доступом к `dail` CLI | deploy, потому что `dail run` для постоянного jail нельзя сделать изнутри другого jail |

### 2.2. Почему deploy — это `exec:`, а не `run:`

`run:`-шаги создают **эфемерные** jail, которые уничтожаются после завершения. Финальный сервис должен **жить долго** — после окончания pipeline. Создать такой jail можно только с хоста, через `dail run` снаружи.

Поэтому deploy = shell-команды на хосте, которые вызывают `dail` для управления долгоживущим jail.

### 2.3. Артефакт не копируется — монтируется

Артефакт уже лежит в ZFS dataset. Не копировать его, не упаковывать — просто **смонтировать в финальный jail как volume**. Это:

- мгновенно (ZFS clone),
- минимум диска (copy-on-write),
- атомарно (новая версия артефакта = новый clone).

## 3. Синтаксис

### 3.1. Минимальный deploy

```yaml
pipeline:
  build:
    run: ci/build.dail
    artifacts:
      - target/release/myapp

  deploy:
    exec: |
      dail run \
        --name myapp \
        --replace \
        --volume $ARTIFACTS_DIR/myapp:/usr/local/bin/myapp:ro \
        --port 8080:8080 \
        /repo/jails/myapp.dail
    needs: [build]
    use_artifacts: [build]
    only:
      branch: main
```

### 3.2. Поля шага `exec:`

| Поле | Тип | Описание |
|------|-----|----------|
| `exec:` | string | shell-скрипт, выполняется через `/bin/sh -e` |
| `needs:` | list | от каких шагов зависит |
| `use_artifacts:` | list | имена шагов, артефакты которых нужны |
| `secrets:` | list | имена секретов для инжекции в env |
| `env:` | map | переменные окружения шага |
| `only:` | object | условия выполнения (branch, tag, event) |
| `timeout:` | duration | timeout (default 10m) |

## 4. Что делает flowz-agent при выполнении `exec:`-шага

### 4.1. Подготовка окружения

1. Создаёт временную директорию `/tmp/flowz/<run_id>/<step_name>/`.
2. Для каждого `use_artifacts` шага: клонит ZFS dataset артефакта, монтирует в `/tmp/flowz/<run_id>/<step_name>/artifacts/<source_step>/`.
3. Если в репозитории есть jail-конфиги (`.dail` файлы для сервисов) — монтирует cloned repo как read-only в `/tmp/flowz/<run_id>/<step_name>/repo/`.
4. Готовит переменные окружения:
   - `$ARTIFACTS_DIR` — путь к корню артефактов
   - `$ARTIFACTS_DIR_BUILD` — путь к артефактам шага build (по имени шага)
   - `$REPO_DIR` — путь к смонтированному репо
   - `$RUN_ID` — текущий run_id
   - `$COMMIT_SHA` — git SHA
   - `$BRANCH` — git branch
   - секреты из `secrets:` как обычные env vars
   - переменные из `env:`

### 4.2. Выполнение

5. Запускает shell с подготовленным окружением и cwd = `$REPO_DIR`.
6. Стримит stdout/stderr в flowz log storage.
7. Дожидается завершения, фиксирует exit code.
8. Сканирует логи на наличие значений секретов, маскирует их.

### 4.3. Cleanup

9. Unmount всех точек монтирования из шага.
10. Удаление временной директории.
11. Уведомление scheduler-а о завершении шага.

**Важно:** артефакты, которые `dail run --volume` смонтировал в финальный долгоживущий jail, **не размонтируются**. Они остаются доступны для работающего сервиса. Это правильное поведение — иначе сервис сломается сразу после deploy.

## 5. Жизненный цикл артефактов после deploy

```
runtime артефакт                                 deploy time
   │                                                 │
   │  zroot/flowz/artifacts/<run_id>/build/myapp     │
   │                       │                         │
   │                       │ dail run --volume       │
   │                       ▼                         │
   │  jail "myapp" монтирует артефакт read-only      │
   │  бинарь доступен в /usr/local/bin/myapp         │
   │                                                 │
   ▼ artifact retention (30 дней)                    ▼
   удаление ZFS dataset → jail продолжает работать,
   так как dataset уже клонирован в namespace jail
```

**Подвох:** при удалении ZFS dataset по retention, jail который его использует, может потерять доступ к артефакту. Решения:

**Вариант A (рекомендуется):** при deploy агент создаёт «pinned» clone артефакта в `zroot/flowz/deployed/<service_name>/current` и монтирует его. При следующем deploy старый clone сохраняется как `previous` (для отката), позапрошлый удаляется. Retention артефактов не трогает pinned clones.

**Вариант B:** retention учитывает «используются ли артефакты в активном deploy» и не удаляет такие.

В MVP — реализовать Вариант A. Чище и явнее.

## 6. Атомарная замена jail

Команда `dail run --replace --name X` должна:

1. Если jail с именем `X` уже работает — остановить и удалить **после** запуска нового.
2. Старый jail не удаляется пока новый не подтвердил готовность (healthcheck или короткий timeout).
3. Порты переключаются атомарно (через PF rdr-anchor swap).
4. При неудаче запуска нового jail — старый продолжает работать, deploy фейлится.

**Это требование к `dail`, не к flowz.** flowz просто вызывает `dail run --replace`, ожидая такого поведения.

Если `dail` пока не поддерживает атомарную замену — для MVP допустимо short downtime (stop старого → start нового). Но в roadmap это надо доделать.

## 7. Rollback

После каждого успешного deploy flowz фиксирует:

- run_id, commit_sha, branch, время
- pinned ZFS dataset с артефактом этого deploy
- параметры запуска (имя jail, volume mounts, ports, .dail файл)

В таблице SQLite:
```
deployments(id, service_name, run_id, commit_sha, deployed_at,
            artifact_dataset, dail_command, status)
```

Команда `flowz rollback <service>` или `flowz rollback <service> <deploy_id>`:

1. Находит предыдущий успешный deploy в таблице.
2. Выполняет тот же `dail run --replace` с pinned артефактом этого deploy.
3. Фиксирует новый deployment record (помеченный как rollback).

Rollback — это просто «deploy предыдущей версии», не магия.

## 8. Безопасность `exec:`

`exec:` потенциально опасен: запускается на хосте, имеет полный доступ к `dail` и системным ресурсам. Меры:

1. **`exec:`-шаги отключены для pull request триггеров по умолчанию.** Только push в защищённые ветки.
2. **flowz-agent работает под отдельным пользователем** (например `flowz`), который входит в группу с правом вызывать `dail` (через `sudo` с ограниченным списком команд или прямой capabilities).
3. **Логирование всех `exec:`-команд** в отдельный аудит-лог, неудаляемый из UI.
4. **В будущем (post-MVP):** декларативный `deploy:`-блок без shell, который flowz транслирует в безопасные вызовы `dail`. Тогда `exec:` останется как escape hatch.

## 9. Пример полного pipeline

```yaml
# .flowz.yaml
version: 1

on:
  push:
    branches: [main]

pipeline:
  build:
    run: ci/build.dail
    cache:
      - path: ~/.cargo/registry
        key: cargo-${hash:Cargo.lock}
    artifacts:
      - target/release/myapp

  test:
    run: ci/test.dail
    needs: [build]
    use_artifacts: [build]

  deploy:
    exec: |
      dail run \
        --name myapp \
        --replace \
        --volume $ARTIFACTS_DIR_BUILD/myapp:/usr/local/bin/myapp:ro \
        --port 8080:8080 \
        --env DATABASE_URL="$DATABASE_URL" \
        $REPO_DIR/jails/myapp.dail
    needs: [test]
    use_artifacts: [build]
    only:
      branch: main
    secrets:
      - DATABASE_URL
    timeout: 5m
```

И сам `jails/myapp.dail` в репозитории:

```dockerfile
FROM 15.0-RELEASE
RUN pkg install -y ca_root_nss
# бинарь не COPY-ится — он будет смонтирован через --volume на этапе run
EXPOSE 8080
CMD ["/usr/local/bin/myapp"]
```

## 10. Open questions

- **Healthcheck при deploy.** Откуда flowz узнаёт что новая версия успешно стартовала? Опции: tail-ить stdout jail на success-маркер; ждать N секунд и проверять что jail running; полагаться на `dail` healthcheck (когда появится). MVP — простой timeout-check.
- **Какие `dail`-команды разрешены в `exec:`?** На MVP — все. В будущем whitelist (`run`, `stop`, `ps`, `logs`, `inspect`), запрет на `destroy` без подтверждения.
- **Concurrent deploys одного сервиса.** Что если два pipeline пытаются деплоить `myapp` одновременно? Lock per service_name на уровне flowz scheduler.
- **Логи долгоживущего jail.** После deploy jail продолжает работать, но flowz уже не следит за его stdout. Возможно интеграция с syslog или просто документация «смотри `dail logs myapp`».

## 11. Минимальный набор требований к dail

Для работы этого ТЗ в `dail` нужно:

- `dail run --replace --name X` — атомарная замена работающего jail с этим именем (или допустимая short downtime в MVP).
- `dail run --volume host_path:jail_path:ro` — read-only mount файла из хоста в jail. Должен корректно работать с файлами внутри ZFS datasets.
- `dail run --port host:jail` — port forwarding (уже есть).
- `dail run --env KEY=VALUE` — env vars (уже есть).
- Стабильные exit codes у `dail` CLI (0 = success, конкретные коды для конкретных ошибок).

Никаких новых концепций в `dail` для deploy не требуется — только то, что уже есть или планируется.
