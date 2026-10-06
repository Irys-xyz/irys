# irys-sm-bench

Phase 1 storage-module load bench. One `StorageModule`, one submodule, one disk-lane thread, the real index drain, and the real submodule MDBX. No network, consensus, VDF, or actor runtime.

Recall reads use a read-only entropy file. Ingest, migration, and serve reads of written chunks use a write file. Both descriptors hang off the same lane, so one spindle schedules both.

The `bench` feature on `irys-domain` is required. Production builds leave it off. A split is installed only when this binary calls `StorageModule::open_split`.

## Safety guard

The bench refuses to start unless the canonical `--dir`, `--recall-file`, and `--write-file` paths each contain a path component named `loadtest`. The same check runs on the path text before any file is created. There is no override.

The bench does not truncate, resize, or `fallocate` either file. A missing file is an error. It does not create `chunks.dat`. If that name already exists under `--dir`, the bench warns and leaves it closed.

The recall file is opened `O_RDONLY`. After open, `fcntl(F_GETFL)` must show `O_ACCMODE == O_RDONLY`. The bench never writes that descriptor.

Before the run, the write file is checked with `FIEMAP`. An unwritten extent, a hole, `UNKNOWN`, or `DELALLOC` is a refusal. A filesystem with no `FIEMAP` is allowed only when `--create-small` wrote that file in this process.

`--out` is not under the guard.

## Build

From the repo root. Do not set `RUSTFLAGS` (that replaces `.cargo/config.toml`).

```sh
cargo build --release -p irys-sm-bench
```

`rust-toolchain.toml` pins the compiler for this repo. The test host may already have an older rustc. `rustup` follows the pin when you build in the repo.

The binary is `target/release/irys-sm-bench`.

## Host run

Record the queue settings before you change them:

```sh
queue=/sys/block/sdc/queue
{
    echo "max_sectors_kb=$(cat $queue/max_sectors_kb)"
    echo "read_ahead_kb=$(cat $queue/read_ahead_kb)"
    echo "nr_requests=$(cat $queue/nr_requests)"
    echo "scheduler=$(awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\[/) print $i }' $queue/scheduler)"
} > /mnt/sm-2/loadtest/sysfs-orig.txt
```

The stock values seen on the lab host were `max_sectors_kb=1280`, `read_ahead_kb=128`, `nr_requests=64`, scheduler `mq-deadline`. Use the file you just wrote, not those numbers, if the host differs.

Recall at 1/s, then ingest at 2, 4, 8, 16, and 32 MiB/s. Each step is 600 s. A hard failure stops the run (`--stop-on-fail`, the default). A recall-margin miss or an enqueue shortfall is recorded and the ladder continues. `--no-stop-on-fail` continues after a hard scored failure too. A drain timeout or a driver abort always stops the plan.

```sh
./target/release/irys-sm-bench \
    --dir /mnt/sm-2/loadtest \
    --recall-file /mnt/sm-2/loadtest/entropy-chunks.dat \
    --write-file /mnt/sm-2/loadtest/write-region.dat \
    --out /mnt/sm-2/loadtest/out-recall-1 \
    --preset recall-1
```

The same profile at 1.8/s (period 555_555_555 ns):

```sh
./target/release/irys-sm-bench \
    --dir /mnt/sm-2/loadtest \
    --recall-file /mnt/sm-2/loadtest/entropy-chunks.dat \
    --write-file /mnt/sm-2/loadtest/write-region.dat \
    --out /mnt/sm-2/loadtest/out-recall-1.8 \
    --preset recall-1.8
```

In another shell, for the same out directory:

```sh
crates/tooling/sm-bench/scripts/sample.sh sdc /mnt/sm-2/loadtest/out-recall-1
```

After the run:

```sh
crates/tooling/sm-bench/scripts/teardown.sh sdc /mnt/sm-2/loadtest/sysfs-orig.txt
```

`teardown.sh` stops `irys-sm-bench` and restores the four queue keys. It does not delete files.

`--chunks` defaults to the recall file length / 256 KiB. The host recall file is 19_800_889_753_600 bytes (75_534_400 chunks). The write file is 274_877_906_944 bytes (1_048_576 chunks). Do not pass `--create-small` on that host.

## Presets and schedules

| Preset | Steps |
| --- | --- |
| `recall-1` | 600 s recall 1/s idle, then ingest 2, 4, 8, 16, 32 MiB/s with recall 1/s |
| `recall-1.8` | Same steps with recall 9/5 |
| `smoke` | One 8 s step: recall 1/s, ingest 2 MiB/s, migration 1 MiB/s, serve 4/s |

`--step-secs N` replaces every step duration. Default preset is `recall-1` when neither `--preset` nor `--schedule` is set.

A schedule file:

```toml
stop_on_fail = true

[[step]]
name = "idle"
secs = 600
recall_num = 1
recall_den = 1

[[step]]
name = "ingest-2"
secs = 600
recall_num = 1
recall_den = 1
ingest_mib_s = 2
migration_mib_s = 0
serve_per_sec = 0
serve_seq_pct = 50
```

Rates are integers. `recall_num = 0` turns recall off. There is no float parser.

## Other flags

| Flag | Default | Role |
| --- | --- | --- |
| `--recall-chunks` | 400 | Chunks in one recall (100 MiB at 256 KiB) |
| `--write-origin` | 0 | Partition offset that maps to byte 0 of the write file |
| `--migrate-origin` | midpoint of the write window | First migration offset |
| `--prepopulate-index N` | 0 | Synthetic index rows. Resumable via `--dir/bench-prepop.json` |
| `--seed` | 1 | Recall, ingest, and serve RNGs |
| `--create-small BYTES` | unset | Create both files at this size for a local smoke run. Multiple of 262144. The recall file is mode `0444` |
| `--num-writes-before-sync` | 100 | Production sync cadence |
| `--entropy-sweep-max-bytes` | 262144 | One entropy pread is one chunk |
| `--keep-recall-cache` | off | Bench default drops recall pages |
| `--ingest-mix` | `1:40,4:25,16:20,64:10,256:5` | `chunks:weight` list |
| `--serve-seq-chunks` | 8 | Longest sequential serve read |
| `--ingest-concurrency` | 1024 | In-flight ingest chunks, held until durable. Not a production admission cap. Migration has no bench cap. Serve is one read at a time |
| `--ingest-index` | `serial` | `serial` commits `index_transaction_data` on the generator before that tx is paced. `overlap` starts the next commit while the current tx's chunks are paced. The MDBX writer is still one at a time |
| `--recall-delay-margin-ms` | 200 | Start-delay p99 may exceed the `idle` step by this much |
| `--recall-latency-margin-ms` | 500 | Recall latency p99 may exceed the `idle` step by this much |
| `--stop-on-fail` | on, from the schedule | Stop after a hard step failure. Overrides `stop_on_fail` in a schedule file |
| `--no-stop-on-fail` | off | Run the remaining steps after a hard scored failure |

## Output

`--out` receives:

- `header.json` — git commit, dirty flag, rustc version, and the resolved settings. Written before the drivers start. Git data comes from `git` in the current directory. A failed git command records commit `unknown`.
- `samples.jsonl` — one JSON object per second per driver (`recall`, `ingest`, `migration`, `serve`), including idle drivers. Latency fields are integer microseconds. The line is flushed as it is written.
- `summary.json` — one object per step, plus `passed`, `exit_code`, and driver notes.
- `table.txt` — the same table the process prints.

Exit `0` when every scored step passed. Exit `1` for usage, a safety refusal, a runtime error, or Ctrl-C. Exit `2` when a scored step failed. A scored failure plus Ctrl-C exits `2`. The summary is still written when the run had started.

Each step summary has `offered_bytes`, `enqueued_bytes`, `durable_bytes`, and `enqueue_blocked_us` / `enqueue_blocked_secs` for ingest and for migration. `enqueue_blocked_us` is wall time the single generator waited before it could hand bytes to the module. For ingest that wait is the bench semaphore, and the same waits are `permit_wait_us`. For migration it is the pending-write ceiling. It does not include the index-ACK wait inside `write_data_chunk_queued`. The table shows recall `lat_p50_us`, `lat_p99_us`, and `start_p99_us`, and ingest and migration as offered/enqueued/durable MiB.

Each `samples.jsonl` line has a `phases` object. Recall and serve leave it zero. `summary.json` repeats the step totals as `ingest_phases` and `migration_phases`. The table adds one line per write driver:

```text
ingest phases: limit=txreg behind=420.000s sleep=90.000s rel/sched=6692/19200MiB txreg n=1100 p99=250.0ms wait=400.000s settle n=1100 p99=5.0ms enqueue n=26000 p99=2.0ms durable p99=700.0ms permit_wait=0.000s permits=31/12.000 gen=1.200s
```

`limit` is why the driver missed `rate * step`. `off` means the rate is 0. `rate` means released and enqueued bytes are within one chunk of the schedule. `txreg`, `gen`, `permits`, `backpressure`, or `enqueue` is the largest generator stall and at least 5% of the step. `pacing` means the bytes were short and none of those stalls explain it. `enqueue` counts only for migration, because `deposit_data_chunks` is awaited on the generator. Ingest `write_data_chunk_queued` runs on the chunk task.

| Field | Meaning |
| --- | --- |
| `pace_behind_us` | Wall time the generator was past its deadline and not asleep. This is part of the step, not a sum of worker lateness |
| `pace_sleep_us` | Wall time asleep because the next release was not due yet |
| `pace_released_bytes` | Bytes the pacer released |
| `pace_scheduled_bytes` | `rate * seconds`. Drain can release more than this |
| `txreg_*` | `index_transaction_data`, including the wait for the bench index lock. One durable MDBX commit per tx. `p50` / `p99` / `max` are microseconds |
| `txreg_wait_us` | Time the generator sat waiting for that result. In `serial` mode this is the commit. In `overlap` mode the next commit runs during the previous tx, so this is only the leftover |
| `settle_*` | `settle_pending_body_migration` after the tx's chunks are durable. Another per-tx MDBX commit. It does not block the generator |
| `enqueue_*` | Ingest: one sample per `write_data_chunk_queued`, until index ACK. Migration: one sample per `deposit_data_chunks` call. Ingest calls overlap, so the total can exceed the step |
| `durable_*` | From ACK or deposit return until `is_data_chunk_durable_at`. One sample per chunk. Overlapping |
| `permit_wait_us` | Generator time waiting for `--ingest-concurrency` |
| `permit_max_in_use` | Highest semaphore occupancy in the window |
| `permit_avg_in_use_milli` | Time-weighted average occupancy times 1000. `12000` is 12 permits |
| `gen_us` | Generator time building the synthetic header, data path, and `0x5A` body |

`behind` and `sleep` are the generator's pacer clock. `txreg_wait_us`, `gen_us`, `permit_wait_us`, migration `enqueue_total_us`, and migration ceiling time are where the non-sleep time went. Do not add them to `behind`. Ingest enqueue and durable totals are overlapping worker time. The step object includes the drain. The per-second lines do not, so step `pace_sleep_us` can exceed the scored seconds while the open transaction is still paced.

A step fails when any of these hold:

- enqueued bytes are short of the offered bytes by more than one chunk (soft: generator cap or enqueue backpressure)
- durable bytes are short of the enqueued bytes by more than one chunk (hard: backlog)
- any recall was skipped (start lag of 3 steps or more, the mining rule)
- recall start-delay p99 or recall latency p99 is over the bar below (soft)
- more than 20% of seconds at or above 5 s were at the pending-write ceiling, or pending bytes are still above the ceiling after the drain
- the pending-write backlog grew by more than one flush batch and is still high after the drain (needs at least 30 samples)
- a body-migration row remains after the drain
- a driver or the lane aborts

The recall bar depends on the schedule. A step named `idle` is the baseline: skips fail it, and its own p99 does not. A later step fails when its start-delay p99 exceeds that baseline by more than `--recall-delay-margin-ms` (default 200), or its latency p99 exceeds the baseline by more than `--recall-latency-margin-ms` (default 500). The summary and the table still show the absolute microseconds. If no `idle` step has completed, the absolute ceilings apply: start-delay p99 above 200 ms, or latency p99 above 500 ms.

`soft_fail` on a step means every miss was a recall-margin or enqueue-shortfall miss. Those misses do not stop the ladder. Other misses do, unless `--no-stop-on-fail` is set. The process still exits `2` when any scored step failed.

At the end of a step the bench stops recall and serve, refuses a new transaction, and keeps pacing the open transaction until it is durable. That drain waits up to 45 s so one 64 MiB transaction offered at 2 MiB/s can finish. Completions during the drain count toward durable bytes. They do not add a scored second and they are not extra sample lines.

## Local smoke

```sh
cargo build --release -p irys-sm-bench
./target/release/irys-sm-bench \
    --dir /tmp/sm-bench-local/loadtest \
    --recall-file /tmp/sm-bench-local/loadtest/recall.dat \
    --write-file /tmp/sm-bench-local/loadtest/write.dat \
    --create-small 4294967296 \
    --recall-chunks 8 \
    --preset smoke \
    --step-secs 8 \
    --out /tmp/sm-bench-out \
    --prepopulate-index 200
```

A directory with no `loadtest` component is refused and is not created.

## I/O shape

These differ from a production storage module. They are deliberate so the bench can run on the two host files.

- One lane issues both files. Recall always reads the entropy file, including a range whose interval is now `Data`. The partition offset still selects the byte offset.
- Entropy-for-XOR preads and packed `pwrite`s use the write file at `(partition_offset - write_origin) * chunk_size`. XOR uses the write file's current bytes. The bench does not check packing.
- The write file's physical placement versus the entropy file is an XFS fact. The bench does not place extents.
- `write_data_chunk_queued` completes before `fsync`. Durable latency ends at `is_data_chunk_durable_at`.
- Ingest holds a bench semaphore permit from the moment the chunk is handed off until that durable check returns. The default is 1024 permits. Production ingress does not take this semaphore. HTTP admission (`mempool.max_http_chunk_admission`, default 256) releases when `write_data_chunk_queued` returns, at index ACK. Ingress handlers default to 30 (`max_concurrent_chunk_ingress_tasks`). Gossip chunk handlers default to 50 (`max_concurrent_gossip_chunks`).
- Migration does not take a bench semaphore. It waits while `pending_write_bytes` is above the body-worker ceiling (two flush batches, and at least 256 chunks). That is the same ceiling as `pending_write_ceiling_bytes` when `max_pending_write_bytes` is unset. Serve does not overlap its own reads.
- Ingest uses the low half of the write window. Migration uses the high half. Production has one append frontier.
- The write path does not check chunk merkle proofs. Chunk bytes are `0x5A` and data paths are 64-byte synthetic values. Serve uses `read_chunks`, which also does not check proofs.
- `--prepopulate-index` grows the MDBX file. Recall does not look up paths, so those pages are touched by commits and the freelist.
- The offer deadline moves forward by one chunk interval from the previous deadline. A late release does not insert a fresh gap, and a late wake does not stretch the schedule. While the deadline is still in the past the generator releases the next chunk immediately.
- `--ingest-index serial` (the default) commits each `index_transaction_data` on the chunk generator before that tx is paced, then commits the next tx only after those chunks are handed off. Production does not do this. `process_ledger_transactions` indexes every tx in a block on the chunk migration service, and the body worker writes chunks on another task, so a later index overlaps body writes. `overlap` starts the next bench commit while the current tx is paced. A prefetched tx that the step does not write is settled, so its body-migration row does not remain. The data-root and tx-path rows from that commit stay in the bench database. The submodule MDBX still has one writer.
- Index updates run on the block path, under one mutex, on the same MDBX the lane drains. That split matches production.
- Startup deletes leftover body-migration rows in this bench database. A resumed run keeps a `Data` prefix and does not finish a transaction a crashed process indexed only in part.
- The bench does not start the mining service. Recall calls `read_recall_chunks` on this thread's blocking pool.
