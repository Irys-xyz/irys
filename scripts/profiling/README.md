# Profiling a running node

Scripts for diagnosing CPU hot spots and memory growth on a live irys node.
Run them on the node host. They need `perf` and `sudo`; heap profiling also
needs `jeprof` (`apt install libjemalloc-dev`).

| Script | What it answers |
|---|---|
| `profile-node.sh cpu` | Where CPU time goes (perf on-CPU samples). |
| `profile-node.sh faults` | Which code touches fresh memory: where anonymous memory grows. No rebuild needed. |
| `profile-node.sh threads` | Per-thread CPU and the RSS split (anonymous vs file-backed). |
| `memwatch.sh` | Logs memory every few seconds and captures `threads` + `faults` the first time anonymous memory passes a threshold. |
| `heap.sh` | Which allocation sites hold live memory, from jemalloc heap dumps. Needs a `jemalloc-prof` build. |
| `summarize_perf.py` | Charges each `perf script` sample to its innermost irys frame. Can also write folded stacks for a flamegraph. |

Output goes to `~/irys-profile/<timestamp>/` by default.

## Memory growth, step by step

1. **Check what is growing.** File-backed RSS is the MDBX/RocksDB page cache and the kernel can reclaim it. Anonymous RSS is the heap.

       grep -E 'RssAnon|RssFile' /proc/$(pm2 pid "Irys Node")/status

2. **Find where fresh memory is touched**, with no rebuild:

       scripts/profiling/profile-node.sh --seconds 5 faults

3. **Find which sites hold live memory.** Build with `--features jemalloc-prof`, then start with sampling on:

       mkdir -p ~/irys-heap
       _RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:20,lg_prof_interval:33,prof_prefix:$HOME/irys-heap/jeprof \
         pm2 restart "Irys Node" --update-env

   Then compare two dumps, so only growth shows:

       scripts/profiling/heap.sh list
       scripts/profiling/heap.sh report target/debug-release/irys NEW.heap --base OLD.heap

## Notes

- **Overhead.** `perf` sampling at 199 Hz and page-fault sampling cost little. jemalloc sampling at 1 MiB costs a few percent of allocation time, so leave `jemalloc-prof` out of production builds unless you are chasing a leak.
- **A full call chain.** `perf -g` uses frame pointers and can stop inside std or btree code. If a summary only shows library frames, re-record with `--call-graph dwarf` on a short window (the files get large).
