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

Recall at 1/s, then ingest at 2, 4, 8, 16, and 32 MiB/s. Each step is 600 s. The run stops at the first failing step.

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

## Output

`--out` receives:

- `header.json` — git commit, dirty flag, rustc version, and the resolved settings. Written before the drivers start. Git data comes from `git` in the current directory. A failed git command records commit `unknown`.
- `samples.jsonl` — one JSON object per second per driver (`recall`, `ingest`, `migration`, `serve`), including idle drivers. Latency fields are integer microseconds. The line is flushed as it is written.
- `summary.json` — one object per step, plus `passed`, `exit_code`, and driver notes.
- `table.txt` — the same table the process prints.

Exit `0` when every scored step passed. Exit `1` for usage, a safety refusal, a runtime error, or Ctrl-C. Exit `2` when a scored step failed. A scored failure plus Ctrl-C exits `2`. The summary is still written when the run had started.

A step fails when any of these hold:

- durable bytes are short of the offered bytes by more than one chunk, or short of the enqueued bytes by more than one chunk
- recall start-delay p99 is above 200 ms, or any recall was skipped (start lag of 3 steps or more, the mining rule)
- more than 20% of seconds at or above 5 s were at the pending-write ceiling, or pending bytes are still above the ceiling after the drain
- the pending-write backlog grew by more than one flush batch and is still high after the drain (needs at least 30 samples)
- a body-migration row remains after the drain
- a driver or the lane aborts

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
- Ingest uses the low half of the write window. Migration uses the high half. Production has one append frontier.
- The write path does not check chunk merkle proofs. Chunk bytes are `0x5A` and data paths are 64-byte synthetic values. Serve uses `read_chunks`, which also does not check proofs.
- `--prepopulate-index` grows the MDBX file. Recall does not look up paths, so those pages are touched by commits and the freelist.
- Index updates run on the block path, under one mutex, on the same MDBX the lane drains. That split matches production.
- Startup deletes leftover body-migration rows in this bench database. A resumed run keeps a `Data` prefix and does not finish a transaction a crashed process indexed only in part.
- The bench does not start the mining service. Recall calls `read_recall_chunks` on this thread's blocking pool.
