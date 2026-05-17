# DailExecutor — MVP ✓ ВЫПОЛНЕНО

**Статус:** реализовано и проверено end-to-end на FreeBSD.
Run `02f9a715` — success (2026-05-17).

## Что сделано

- `DailExecutor` (`src/executor/mod.rs:89`) — jail-per-job через `dail run --rm --wait`.
  Построчный стрим stdout, дренаж stderr, exit code → `StepOutcome`.
  RAII cleanup-guard через `--rm`.
- `build_dail_command` helper (`src/executor/mod.rs:115`) — единственное место,
  где собирается строка `dail run <jail> <file> --mount ... --rm --wait`.
- `ExecutorKind { Stub, Dail }` (`src/executor/mod.rs:28`) — фабрика `build()`.
  Дефолт: `Dail` на FreeBSD, `Stub` иначе.
- `AgentConfig.executor: ExecutorKind` (`src/agent/mod.rs:16`) — поле в конфиге,
  читается из `flowz-agent.yaml`, перекрывает платформенный дефолт.

## Что осталось за рамками (отдельные задачи)

- CLI-команды (`flowz validate`, `flowz status`, `flowz logs`)
- rc.d/port-упаковка
- DAG-исполнение, секреты, ZFS-снапшоты
