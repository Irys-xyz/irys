# irys-sm-bench

Phase 1 storage-module load bench. One `StorageModule`, one submodule, one disk-lane thread, the real index drain, and the real submodule index. No network, consensus, VDF, or actor runtime.

Recall reads use a read-only entropy file. Ingest, migration, and serve reads of written chunks use a write file. Both descriptors hang off the same lane, so one spindle schedules both.

The `bench` feature on `irys-domain` is required. Production builds leave it off. A split is installed only when this binary calls `StorageModule::open_split`.

## Safety guard

The bench refuses to start unless `--dir`, `--recall-file`, and `--write-file` each contain a path component named `loadtest`. Before any file is created, the bench canonicalizes the deepest existing ancestor of each path and refuses when that resolved path has no `loadtest` component. There is no override.

The bench does not truncate, resize, or `fallocate` either file. A missing file is an error. It does not create `chunks.dat`. If that name already exists under `--dir`, the bench warns and leaves it closed.

The recall file is opened `O_RDONLY`. After open, `fcntl(F_GETFL)` must show `O_ACCMODE == O_RDONLY`. The bench never writes that descriptor.

Before the run, the recall file and the write file are checked with `FIEMAP`. An unwritten extent, a hole, `UNKNOWN`, or `DELALLOC` is a refusal. A filesystem with no `FIEMAP` is allowed only for a file `--create-small` wrote in this process.

`--out` is not under the guard.

## Build

From the repo root. Do not set `RUSTFLAGS` (that replaces `.cargo/config.toml`).

```sh
cargo build --release -p irys-sm-bench
```

`rust-toolchain.toml` pins the compiler for this repo. The test host may already have an older rustc. `rustup` follows the pin when you build in the repo.

The binary is `target/release/irys-sm-bench`.

## Host setup

Use a dedicated HDD, or a spare storage-module disk. Stop the node before you touch the disk. Do not open a live node database. Do not open or replace `chunks.dat`.

Create a directory whose path contains a component named `loadtest`. The bench refuses `--dir`, `--recall-file`, and `--write-file` without that component.

Move a real entropy file in from an entropy-only module. `mv` keeps the inode. Set the mode to `0444`. The bench opens that file `O_RDONLY`.

Pre-write the write region before the first measured run. An unwritten XFS extent reads back as zeros and the disk does no I/O, so a sparse file does not load the spindle. `scripts/fill.sh` writes an existing file with `O_DIRECT`, one segment at a time, and appends one rate line per segment. Segment duration comes from `/proc/uptime`. Create the stop file (default `./STOP`) to finish after the current segment. `--from INDEX` resumes.

```sh
crates/tooling/sm-bench/scripts/fill.sh \
    --file /mnt/sm-2/loadtest/write-region.dat \
    --log /home/ubuntu/loadtest/fill-write-region.log \
    --segment-bytes $((16 * 1024 * 1024 * 1024))
```

The file must already exist. `fill.sh` does not allocate it.

`scripts/run.sh` refuses the run when the loadtest disk has less than 500 GiB free, or when `--mount` is not that mount. `--min-free-gib` changes the floor. The value must be an integer from 0 through 8589934591. The check reads `df -B1` of the loadtest path and compares bytes, so a value just under the floor does not round up to a pass.

Record the queue before a ladder changes it. On the lab host the stock values were `max_sectors_kb=1280` (hardware max 32767), `read_ahead_kb=128`, `nr_requests=64`, scheduler `mq-deadline`. Use the file you record when the host differs. `examples/sysfs-orig.example.txt` is a host log of that record. It also has `rotational` and `queue_depth`, which `teardown.sh` does not restore. `run.sh` writes the four keys `teardown.sh` accepts to `<out>/sysfs.txt`. The scheduler value is the active name in brackets. A `# scheduler_raw=` line keeps the full queue string. `ladder.sh` sets `max_sectors_kb` for the ladder and restores it on EXIT, INT, and TERM. The restored value is the value read before the set, unless `restore_sectors_kb` names another value.

Drop the write-region page cache before a measured run so those reads miss the host cache:

```sh
dd if=/mnt/sm-2/loadtest/write-region.dat iflag=nocache count=0
```

`ladder.sh --drop-cache` runs that before each run. A cold host cache still leaves the drive cache. The drive can complete a later read of the same LBAs from its own cache. The first pass over an LBA after a long idle is the cold read.

### Detached ladder

Build into a target directory outside the checkout:

```sh
CARGO_TARGET_DIR=/home/ubuntu/loadtest/target \
    crates/tooling/sm-bench/scripts/build.sh /path/to/repo
```

`JOBS` (default 8) and `NICE` (default 15) change the build. Do not set `RUSTFLAGS`.

A ladder config is `key=value` lines plus one `run` line per ladder. A flag on the command line overrides the same key. `common` is extra bench arguments for every run. Arguments are split on whitespace, so a value cannot contain a space.

```
device=sdc
mount=/mnt/sm-2
loadtest=/mnt/sm-2/loadtest
out_root=/home/ubuntu/loadtest/runs
bench=/home/ubuntu/loadtest/target/release/irys-sm-bench
max_sectors_kb=32767
restore_sectors_kb=1280
drop_cache=true
common=--index-engine rocks --rocks-preset baseline --index-group-commit 8 --ingest-concurrency 4096 --num-writes-before-sync 10000 --entropy-sweep-max-bytes 10485760 --recall-direct-io
run fine-16-32 --schedule crates/tooling/sm-bench/presets/fine-16-32.toml
run recall-5x80 --schedule crates/tooling/sm-bench/presets/recall-5x80.toml --recall-chunks 80
```

`trace=true`, or `--trace`, starts `scripts/blk-run.sh` around each run and then runs `scripts/blk-report.py`. That needs `bpftrace` and a passwordless `sudo`.

Start the ladder with no controlling terminal:

```sh
setsid nohup crates/tooling/sm-bench/scripts/ladder.sh \
    --config /home/ubuntu/loadtest/ladder.conf \
    > /home/ubuntu/loadtest/ladder.log 2>&1 < /dev/null &
```

The script writes `<out_root>/ladder.done` when it exits, including after a failure. Before each run it renames `db`, `rocks`, `intervals.json`, `packing_params.toml`, and `bench-prepop.json` into `loadtest/state-<name>/`. A directory left between renames is put back: a name that is only there moves to the module root, and an empty directory is removed. A name that is in both places is an error, and the error names any index file that is only in that directory. Nothing is deleted. The last run's state stays in `loadtest/`. `prior_name` (default `prior`) is the directory name for state that is already there at the start. The loadtest path, the mount disk, pm2, and the free-space floor are checked before that rename and before `max_sectors_kb` is written. If `run.sh` still refuses after the rename, the files move back and that state directory is removed. If any of those names is already in the module root, the restore moves nothing and names each one. A move that fails puts the names that restore had already moved back into the state directory. A state directory this run did not create stays.

`run.sh` exits 0 when the bench exits 0, and when the bench exits 2 after writing `header.json`. That file is written after a successful open, so a scored miss has it: the ladder continues and the new index stays. A usage error also exits 2 and does not write `header.json`. An older `header.json` in the run directory does not count. `run.sh` then exits 2, the ladder stops, and the previous index moves back. Any other bench status, including a panic, makes `run.sh` exit with that status and restores the same way. A guard failure exits 1 and stops the ladder. Guards: the pm2 app `Irys Node` is online, `pm2 jlist` cannot be read, free space is below the floor, or a path has no `loadtest` component. A missing `pm2` binary is allowed.

One run, without the ladder:

```sh
crates/tooling/sm-bench/scripts/run.sh \
    --name fine-16-32 --device sdc --mount /mnt/sm-2 \
    --loadtest /mnt/sm-2/loadtest \
    --out-root /home/ubuntu/loadtest/runs \
    --bench /home/ubuntu/loadtest/target/release/irys-sm-bench \
    -- --schedule crates/tooling/sm-bench/presets/fine-16-32.toml \
    --recall-direct-io
```

Schedules in `presets/`, named by the question they ask:

| File | Shape |
| --- | --- |
| `fine-16-32.toml` | Recall 1/s. Idle 600 s. Ingest 16, 20, 24, 28, 32 MiB/s |
| `recall-5x80.toml` | Recall 5/s. Pass `--recall-chunks 80` (same bytes as one 400-chunk recall). Idle 600 s. Ingest 8, 12, 16, 20, 24 MiB/s |
| `recall-5x80-traced.toml` | Same recall. Idle 180 s. Ingest 8, 12, 16 MiB/s. Pair with `--trace` |
| `recall-1-traced.toml` | Recall 1/s. Idle 180 s. Ingest 16 and 20 MiB/s. Pair with `--trace` |

All four set `stop_on_fail = false` so a hard step still leaves a full record.

### Reading a run

The table's main row is the whole step, in integer MiB. The `tail Ns:` line is the last N scored seconds (`--judge-tail-secs`, default 240). Both are judged. A step whose mean holds can still fail when the last minutes collapse. `summary.json` stores that window on `tail`. The field is `null` when the window is off or the step was interrupted. `header.json` stores `judge_tail_secs`.

`scripts/analysis/minutes.py` prints each minute of a `samples.jsonl`. Use it when the tail fails and you need the shape of the drop.

```sh
python3 crates/tooling/sm-bench/scripts/analysis/minutes.py \
    /home/ubuntu/loadtest/runs/fine-16-32/samples.jsonl ingest-32
```

`scripts/analysis/disktime.py` reads an `iostat -dmx` log. Busy time is `%util` times 10, in milliseconds. Transfer time is bytes divided by a zone rate. The remainder is positioning. `--recall-rate` defaults to 200 MiB/s. `--region-rate` defaults to 146 MiB/s. `--device` is required. The first device line is the since-boot average and is skipped.

```sh
python3 crates/tooling/sm-bench/scripts/analysis/disktime.py \
    /home/ubuntu/loadtest/runs/fine-16-32/iostat.log \
    --device sdc --window ingest-32:0:600
```

`scripts/analysis/ladder.py` prints one block per step from `samples.jsonl`. Its percentile is the floor rank of the per-second maxima. The bench table uses nearest-rank over every recall sample in the step. Those two numbers differ.

`blk-report.py` writes `blk-transitions.csv` next to the trace. A row is one request class followed by another, with the service time of the second request. `packed_write` followed by `flush` is the drive writing its cache back to the media. See [Disk-time split](#disk-time-split).

### Teardown

Put the entropy file back on the module it came from, and restore mode `0664`.

If the file stays off that module, set the module's `intervals.json` to one inclusive `Uninitialized` range:

```json
[[{"start": 0, "end": LAST}, "Uninitialized"]]
```

`LAST` is the last chunk offset (chunk count minus one). The next start then packs the range again. A module that still says `Entropy` will treat a new sparse file as packed entropy. Unwritten extents read as zeros and issue no I/O.

Remove the write region, the run directories under `out_root`, and each `loadtest/state-<name>/` directory. `ladder.sh` never deletes them. The bench's own `intervals.json` under `loadtest/` is bench state. Remove it with the other state files.

Restore the queue from the file you recorded:

```sh
crates/tooling/sm-bench/scripts/teardown.sh sdc /mnt/sm-2/loadtest/sysfs-orig.txt
```

`teardown.sh` also stops a running `irys-sm-bench`. It does not delete files.

The ops repo's sm-loadtest playbooks cover the same provisioning and teardown.

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
    --preset recall-1 \
    --recall-direct-io
```

The same profile at 1.8/s (period 555_555_555 ns):

```sh
./target/release/irys-sm-bench \
    --dir /mnt/sm-2/loadtest \
    --recall-file /mnt/sm-2/loadtest/entropy-chunks.dat \
    --write-file /mnt/sm-2/loadtest/write-region.dat \
    --out /mnt/sm-2/loadtest/out-recall-1.8 \
    --preset recall-1.8 \
    --recall-direct-io
```

In another shell, for the same out directory:

```sh
crates/tooling/sm-bench/scripts/sample.sh sdc /mnt/sm-2/loadtest/out-recall-1
```

Per-file disk time uses `scripts/blk-run.sh` next to the bench. See [Disk-time split](#disk-time-split).

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
| `--recall-direct-io` | off | Read the entropy file with `O_DIRECT` for recall. The bench default is off. The node default is on. A run that omits this flag measures buffered recall |
| `--write-origin` | 0 | Partition offset that maps to byte 0 of the write file |
| `--migrate-origin` | midpoint of the write window | First migration offset |
| `--prepopulate-index N` | 0 | Synthetic index rows. Resumable via `--dir/bench-prepop.json`. Works for both index engines |
| `--index-engine` | `mdbx` | `mdbx` opens `--dir/db`. `rocks` opens `--dir/rocks`. A directory that already belongs to the other engine is refused |
| `--rocks-preset` | `baseline` when the engine is `rocks` | Same names as `RocksTuning::presets`. Refused unless `--index-engine rocks` |
| `--rocks-block-cache BYTES` | preset size | Replaces the preset block cache. Refused unless `--index-engine rocks`. `0` is an error |
| `--rocks-small-file-compact-threshold N` | preset value | Replaces the preset per-CF small-file compact threshold. `0` turns the hook off. Refused unless `--index-engine rocks`. The resolved count is `small_file_compact_threshold` in the `rocks_tuning` text |
| `--seed` | 1 | Recall, ingest, and serve RNGs |
| `--create-small BYTES` | unset | Create both files at this size for a local smoke run. Multiple of 262144. The recall file is mode `0444` |
| `--num-writes-before-sync` | 100 | Production sync cadence |
| `--entropy-sweep-max-bytes` | 4194304 | Cap on one entropy pread. The node cap is 4 MiB, about 16 chunks of 256 KiB |
| `--ingest-mix` | `1:40,4:25,16:20,64:10,256:5` | `chunks:weight` list |
| `--serve-seq-chunks` | 8 | Longest sequential serve read |
| `--ingest-concurrency` | 1024 | In-flight ingest chunks, held until durable. Not a production admission cap. Migration has no bench cap. Serve is one read at a time |
| `--ingest-index` | `serial` | `serial` commits `index_transaction_data` on the generator before that tx is paced. `overlap` starts the next commit while the current tx's chunks are paced. The MDBX writer is still one at a time |
| `--recall-delay-margin-ms` | 200 | Start-delay p99 may exceed the `idle` step by this much |
| `--recall-latency-margin-ms` | 500 | Recall latency p99 may exceed the `idle` step by this much |
| `--stop-on-fail` | on, from the schedule | Stop after a hard step failure. Overrides `stop_on_fail` in a schedule file |
| `--no-stop-on-fail` | off | Run the remaining steps after a hard scored failure |
| `--judge-tail-secs` | 240 | Also judge the last N scored seconds. `0` turns that judge off. The drain is outside the window |

## Output

`--out` receives:

- `header.json` — git commit, dirty flag, rustc version, and the resolved settings, including `index_engine`, `index_dir`, `rocks_preset`, `rocks_tuning`, `rocks_block_cache_bytes`, and `judge_tail_secs`. `rocks_tuning` includes `small_file_compact_threshold` after any `--rocks-small-file-compact-threshold` override. The Rocks fields are null for MDBX. Written before the drivers start. Git data comes from `git` in the current directory. A failed git command records commit `unknown`.
- `samples.jsonl` — one JSON object per second per driver (`recall`, `ingest`, `migration`, `serve`), including idle drivers. Latency fields are integer microseconds. The line is flushed as it is written.
- `steps.jsonl` — one JSON object per line for the disk-time report. The first line is the run start (`kind` = `run`) with `realtime_ns` (`CLOCK_REALTIME`) and `monotonic_ns` (`CLOCK_MONOTONIC`), both nanoseconds. Each step then has `kind` = `step`, `index`, `name`, and `phase` of `start`, `drain`, or `end`, with the same two clocks. `start` is just before the step rate is published. `drain` is the start of the drain. `end` is after the drain, including the drain's index read. The scored sample lines do not include the drain. This file does.
- `summary.json` — one object per step, plus `passed`, `exit_code`, and driver notes.
- `table.txt` — the same table the process prints. The first line is the index engine and directory. A RocksDB step adds a `rocks:` line with stall time, pending compaction bytes, flushes, running flushes, WAL syncs, and small-file compact counts (runs and files before/after). A step with a tail window adds a `tail Ns:` line after that: ingest and migration enqueued/durable MiB/s with one truncated decimal, recall skips, start-delay p99, start-delay max, and `PASS` or `FAIL`.

Exit `0` when every scored step passed. Exit `1` for usage, a safety refusal, a runtime error, or Ctrl-C. Exit `2` when a scored step failed. A scored failure plus Ctrl-C exits `2`. The summary is still written when the run had started.

Each step summary has `offered_bytes`, `enqueued_bytes`, `durable_bytes`, and `enqueue_blocked_us` / `enqueue_blocked_secs` for ingest and for migration. It also has `tail`. `tail` is `null` when `--judge-tail-secs` is 0 or the step was interrupted. `tail.reasons` are the window's own messages. The same miss on the step `reasons` list has a `tail: ` prefix when the window is shorter than the step. A window that covers every scored second does not repeat those reasons. The tail line is still printed, and `tail.passed` is its own verdict: a drain failure can be a step failure with a passing tail. `enqueue_blocked_us` is wall time the single generator waited before it could hand bytes to the module. For ingest that wait is the bench semaphore, and the same waits are `permit_wait_us`. For migration it is the pending-write ceiling. It does not include the index-ACK wait inside `write_data_chunk_queued`. The table shows recall `lat_p50_us`, `lat_p99_us`, and `start_p99_us`, and ingest and migration as offered/enqueued/durable MiB.

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
| `txreg_*` | `index_transaction_data`, including the wait for the bench index lock. One durable index commit per tx on this one submodule. `p50` / `p99` / `max` are microseconds |
| `txreg_wait_us` | Time the generator sat waiting for that result. In `serial` mode this is the commit. In `overlap` mode the next commit runs during the previous tx, so this is only the leftover |
| `settle_*` | `settle_pending_body_migration` after the tx's chunks are durable. Another per-tx index commit. It does not block the generator |
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
- serve ops are short of the offered ops by more than one op (soft). Offered ops are `serve_per_sec` times the scored time after the first chunk this process made durable. Time before that chunk, and a step that never made a chunk durable, are not part of the offer. The result column names a miss, for example `FAIL(serve 9.6k/18k)`. `serve_per_sec = 0` is not a target, so a recall-only step does not fail for serve
- more than 20% of seconds at or above 5 s were at the pending-write ceiling, or pending bytes are still above the ceiling after the drain
- the pending-write backlog grew by more than one flush batch and is still high after the drain (needs at least 30 samples)
- a body-migration row remains after the drain
- a driver or the lane aborts

The last `--judge-tail-secs` scored seconds (default 240) get a second verdict. Recall skips, recall p99, an enqueue shortfall, a serve-rate miss, and the overloaded-second rule apply to that window. Durable bytes may lag the enqueue inside the window, because the drain is outside it. That gap is printed on the tail line and does not fail the tail. Bytes still pending after the drain, leftover migration rows, backlog growth, and an abort stay on the whole-step judge. `0` turns the window off. An interrupted step has no tail. A tail miss is part of the step verdict. A soft tail miss does not stop the ladder.

The recall bar depends on the schedule. A step named `idle` or `warm`, or the first recall-only step when no baseline exists yet, is the baseline: skips fail it, and its own p99 does not. A later step fails when its start-delay p99 exceeds that baseline by more than `--recall-delay-margin-ms` (default 200), or its latency p99 exceeds the baseline by more than `--recall-latency-margin-ms` (default 500). The summary and the table still show the absolute microseconds. If no baseline step has completed, the absolute ceilings apply: start-delay p99 above 200 ms, or latency p99 above 500 ms.

`soft_fail` on a step means every miss was a recall-margin miss, an enqueue shortfall, or a serve-rate miss. Those misses do not stop the ladder. Other misses do, unless `--no-stop-on-fail` is set. The process still exits `2` when any scored step failed.

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
    --prepopulate-index 200 \
    --recall-direct-io
```

A directory with no `loadtest` component is refused and is not created.

## I/O shape

These differ from a production storage module. They are deliberate so the bench can run on the two host files.

- One lane issues both files. Recall always reads the entropy file, including a range whose interval is now `Data`. The partition offset still selects the byte offset.
- Entropy-for-XOR preads and packed `pwrite`s use the write file at `(partition_offset - write_origin) * chunk_size`. XOR uses the write file's current bytes. The bench does not check packing.
- The write file's physical placement versus the entropy file is an XFS fact. The bench does not place extents.
- `write_data_chunk_queued` completes before `fsync`. Durable latency ends at `is_data_chunk_durable_at`.
- Ingest holds a bench semaphore permit from the moment the chunk is handed off until that durable check returns. The default is 1024 permits. Production ingress does not take this semaphore. HTTP admission (`mempool.max_http_chunk_admission`, default 256) releases when `write_data_chunk_queued` returns, at index ACK. Ingress handlers default to 30 (`max_concurrent_chunk_ingress_tasks`). Gossip chunk handlers default to 50 (`max_concurrent_gossip_chunks`).
- Migration does not take a bench semaphore. It waits while `pending_write_bytes` is above the body-worker ceiling (two flush batches, and at least 256 chunks). That ceiling is `StorageModule::pending_memory_ceiling_bytes`. Serve does not overlap its own reads.
- Ingest uses the low half of the write window. Migration uses the high half. Production has one append frontier.
- The write path does not check chunk merkle proofs. Chunk bytes are `0x5A` and data paths are 64-byte synthetic values. Serve uses `read_chunks`, which also does not check proofs.
- `--prepopulate-index` grows the MDBX file. Recall does not look up paths, so those pages are touched by commits and the freelist.
- The offer deadline moves forward by one chunk interval from the previous deadline. A late release does not insert a fresh gap, and a late wake does not stretch the schedule. While the deadline is still in the past the generator releases the next chunk immediately.
- `--ingest-index serial` (the default) commits each `index_transaction_data` on the chunk generator before that tx is paced, then commits the next tx only after those chunks are handed off. Production does not do this. `process_ledger_transactions` indexes every tx in a block on the chunk migration service, and the body worker writes chunks on another task, so a later index overlaps body writes. `overlap` starts the next bench commit while the current tx is paced. A prefetched tx that the step does not write is settled, so its body-migration row does not remain. The data-root and tx-path rows from that commit stay in the bench database. The submodule MDBX still has one writer.
- Index updates run on the block path, under one mutex, on the same MDBX the lane drains. That split matches production.
- Startup deletes leftover body-migration rows in this bench database. A resumed run keeps a `Data` prefix and does not finish a transaction a crashed process indexed only in part.
- The bench does not start the mining service. Recall calls `read_recall_chunks` on this thread's blocking pool.

## Disk-time split

Use this when iostat shows a positioning remainder and you need the remainder split by file. The bench writes `steps.jsonl`. `blk-run.sh` records the block trace, the extent maps, and `io_ticks`. `blk-report.py` joins them.

Put the trace directory on a different filesystem from the disk under test. A trace on that disk shows up as `metadata/other`.

`blk-run.sh` blocks until INT or TERM. Start it first, in its own shell. Start the bench after it has attached (it checks bpftrace after one second). Use the same directory for `--out` and the trace so `steps.jsonl` sits next to `trace.log`. If the host starts the bench from a `run.sh`, use that script in place of the binary below and stop the trace after `run.sh` exits.

```sh
# Shell B. Blocks until Ctrl-C or SIGTERM.
crates/tooling/sm-bench/scripts/blk-run.sh \
    sdc \
    /mnt/sm-2/loadtest \
    /home/ubuntu/loadtest/runs/blk-1

# Shell A. After shell B is attached.
./target/release/irys-sm-bench \
    --dir /mnt/sm-2/loadtest \
    --recall-file /mnt/sm-2/loadtest/entropy-chunks.dat \
    --write-file /mnt/sm-2/loadtest/write-region.dat \
    --out /home/ubuntu/loadtest/runs/blk-1 \
    --preset recall-1 \
    --recall-direct-io

# After the bench exits, Ctrl-C shell B.
# Shell B stops bpftrace, takes one last extent snapshot, and exits.
```

`blk-run.sh` refuses a directory whose path has no `loadtest` component, on the path you pass and again after symlink resolution. It reads extent metadata only. It does not read file bytes.

Then:

```sh
python3 crates/tooling/sm-bench/scripts/blk-report.py \
    /home/ubuntu/loadtest/runs/blk-1
```

If `steps.jsonl` is in another directory, pass `--steps`. The report writes `blk-report.txt`, `blk-report.json`, `blk-report.csv`, and `blk-transitions.csv` in the trace directory.

`<dev>` is the whole disk (`sdc`), the same name iostat uses. Do not pass a partition. The lab disk is XFS on `/dev/sdc` with no partition table. `extent-snap.sh` still adds the sysfs `start` offset (`/sys/dev/block/<major>:<minor>/start`, the same value as `/sys/class/block/<dev>/start`) so a filesystem on a partition lines up with the parent disk's sectors.

Categories:

| Name | Requests |
| --- | --- |
| `recall` | reads of `entropy-chunks.dat` |
| `entropy_read` | reads of `write-region.dat` (packing entropy and serve reads of the write file) |
| `packed_write` | writes of `write-region.dat` |
| `index_wal` | `*.log` WAL files, not RocksDB `LOG` |
| `index_sst` | `*.sst` |
| `index_blob` | `*.blob` |
| `index_other` | `MANIFEST-*`, `OPTIONS-*`, `LOG`, `CURRENT`, `IDENTITY`, `mdbx.dat`, `mdbx.lck` |
| `flush` | `rwbs` contains `F` and `nr_sector` is 0 |
| `metadata` | XFS log, inodes, discards, and sectors that match no file. The text table prints this as `metadata/other` |

A request that covers two files takes the file with more of its sectors. A file that disappears from a later snapshot keeps its last extents until a newer snapshot gives those sectors to another file. The snapshot used for a request is the one nearest its issue time.

Service time is `complete - max(issue, previous completion)` in completion order. The disk has one actuator, so this sum is the busy time, not the sum of request latencies. Positioning is that service time minus `bytes / zone_rate(start LBA)`. Seek distance is `|start sector - previous data request's end sector|` and does not move on a zero-length flush.

The zone rate is a straight line by LBA fraction. The default is 266 MiB/s at the first LBA and 130 MiB/s at the last LBA. Outer-track sequential fills on the lab HDD measured about 266 MiB/s. The write region in allocation group 18 (about nine tenths of the way across groups 0–20) filled at about 146 MiB/s, which is this line at that fraction. `--zone-outer` and `--zone-inner` replace the anchors. `device.txt` supplies the sector count (`/sys/class/block/<dev>/size`).

`busy.log` samples field 10 of `/sys/block/<dev>/stat` (`io_ticks`, the counter behind iostat `%util`). For each step the report prints `service ms - io_ticks ms`.

Caveats:

- NCQ and the scheduler can complete requests in a different order than the head moved. Per-request service and the transition matrix are approximate. The step total is the figure that should match iostat.
- A flush with `nr_sector` 0 is the drive writing back its cache, not a geometric seek. Its positioning time is the whole service time. A write that completes into the cache looks cheap. The following flush carries the media time. `packed_write -> flush` in the transition matrix is the flush-coalescing cost.
- Flush pairing follows issue and completion shape, not sector. A `blk_kick_flush` request is issued as `rwbs` `FF`, sector 0, `nr_sector` 0. `block_rq_complete` records `blk_rq_pos`, and a flush has no bio, so the completion sector is `(sector_t)-1` with `nr_sector` 0. Each completion closes a batch of one or more starts. The extra starts are requeues: `block_rq_issue` runs from `blk_mq_start_request` on every start, and a requeue has no completion. The last start in a batch is the one that reached the device. The report still pairs each zero-length completion FIFO with one outstanding zero-length flush issue and ignores sector and `rwbs`. Further starts stay unpaired and are listed by `rwbs` in the header.
- `REQ_PREFLUSH|REQ_FUA` writes are split by `blk-flush.c` into pre-flush, data, and post-flush when the drive has a write cache and no native FUA. The data phase is issued with those flags cleared, so the trace shows `WSM` (sync metadata write) or `WS`, not `F`. The data completion has the same sector and length. A zero-length `FF` post-flush follows, then a second completion of the original request with the same sector and `nr_sector` 0 (`REQ_FSEQ_DONE`; the bytes were already completed). The same tail shows up as `nr_sector` 0 `WS`, sometimes at sector 0. The data completion stays with the write. The zero-length completion is a flush. If no flush issue is left, the tail is still a flush whose issue time is the completion, so it does not add a second copy of the post-flush.
- `RA` completions are a prefix, a suffix, or a middle of an outstanding span; the uncovered part stays in flight. A later issue with the same start, the same `rwbs`, and a larger `nr_sector` replaces the earlier issue (a merge before the request grew). A second start of an identical span has no completion of its own. The one completion pairs with the earlier issue. Unpaired issues left at the end of a trace are not treated as in flight. A later issue that starts at a lower sector and covers an earlier span is left as its own request. The header lists what is still unpaired, by `rwbs`.
- The zone line is one segment. A request uses the rate at its start LBA. Positioning is 0 when the transfer estimate is longer than the service time (drive cache, or a rate that is too low).
- Extent snapshots run about every 30 s and use `FIEMAP` or `xfs_bmap`. That is metadata I/O on the measured disk. New files show up at the next snapshot. `filefrag` block numbers are filesystem blocks. `xfs_bmap -v` numbers are 512-byte sectors.
- bpftrace is print-only (`block_rq_issue` and `block_rq_complete`) filtered by `dev_t` (`major << 20 | minor`). A merged request is one issue whose `nr_sector` is the merged length. A completion may cover a prefix or a suffix of that span. There is no in-kernel map.
- `blk-run.sh` needs `sudo` unless it is already root. `bpftrace` must be installed on the host. This tree does not package it.
