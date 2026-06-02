## flowz

- **Workspace** — общий ZFS dataset, монтируется по очереди во все шаги pipeline как `/workspace`. Создаётся в начале, удаляется в конце. Файлы передаются между шагами автоматически.
- **`artifacts:`** — список путей в шаге. После успешного шага flowz-agent копирует их с workspace на flowz-server, хранит в `/var/db/flowz/artifacts/<run_id>/`, отдаёт через UI/CLI/API. Retention policy: 30 дней или 10 последних run.
- **`cache:`** — путь + ключ. Перед шагом flowz клонит ZFS snapshot `flowz/cache/<repo>/<key>` в jail. После шага делает новый snapshot. Поддержка `fallback_keys` для частичного попадания.
- **Failure debug** — при упавшем pipeline workspace сохраняется как ZFS snapshot на 24ч. `flowz debug <run_id>` запускает интерактивный jail из этого снапшота.
- **GC** — `flowz gc` плюс cron, чистит старые artifacts, evict cache по LRU.

## dail

- **`--workspace <host_path>:<jail_path>`** — монтирует ZFS dataset в jail с заданными permissions/UID mapping. Корректный unmount при завершении даже на crash.
- **`--cache <host_path>:<jail_path>`** — то же что workspace, но семантически отдельный флаг (для clarity и возможной разной обработки в будущем).
- **Параллельные mount одного dataset** — запретить или сериализовать, чтобы два jail не писали в один кэш одновременно.
- **Cleanup invariants** — гарантировать unmount всех точек при `dail prune` для зомби-jail.

То есть в dail добавляется по сути один механизм (mount внешних путей с правильной семантикой), а вся логика артефактов и кэша живёт во flowz.
