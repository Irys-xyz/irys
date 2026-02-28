# irys

## Development setup
Clone the repository, install Rust
rust-toolchain.toml contains the version you should be using.\
Or, use the development container configuration contained in .devcontainer

Other dependencies (for the OpenSSL build):

```
clang & a C/C++ build toolchain
gmp
pkg-config
```
The NVIDIA feature flag/CUDA accelerated matrix packing requires the latest CUDA toolkit (12.6+)\
and GCC-13 (as well as g++ 13).\
See `.devcontainer/setup.sh` for more information.

Local development commands:

```cli
cargo xtask --help

cargo xtask check
cargo xtask test
cargo xtask unused-deps
cargo xtask typos

cargo xtask local-checks # runs 99% of the tasks CI does
```

## Testing

General:
```cli
cargo xtask test
```

Testing code examples in comments

```cli
cargo test --doc
```

Re-run only failing tests:
```cli
cargo xtask test --rerun-failures
```

Run a single package:
```cli
cargo xtask test -- -p irys-actors
```

### Test resource monitoring

`cargo xtask test` always tracks pass/fail results per test (via the `nextest-wrapper` binary). Add `--monitor` to also sample CPU and memory usage at 50 ms intervals:

```cli
# Run tests with CPU + memory monitoring
cargo xtask test --monitor

# Run three times for better averages, then analyze
cargo xtask test --monitor
cargo xtask test --monitor
cargo xtask test --monitor

# See which tests are heaviest
cargo run --bin nextest-report -- summary --sort peak --top 10

# See which tests use the most memory
cargo run --bin nextest-report -- summary --sort peak_rss --top 10

# Find tests that need reclassification (e.g. missing heavy_ prefix)
cargo run --bin nextest-report -- analyze
```

Tests are classified by naming convention in `.config/nextest.toml`:
- `serial_*` — run serially
- `slow_*` — 90 s timeout
- `heavy_*` — 2 threads required
- `heavy3_*` — 3 threads required
- `heavy4_*` — 4 threads required

The `analyze` command identifies tests whose actual CPU usage doesn't match their classification and suggests the correct prefix.

See [`crates/utils/nextest-monitor/README.md`](crates/utils/nextest-monitor/README.md) for full documentation.

## Debugging
If you're debugging and noticing any issues (i.e unable to inspect local variables)\
comment out all instances of the  `debug = "line-tables-only"` and `split-debuginfo = "unpacked"` lines in the root Cargo.toml\
these options accelerate debug build times at the cost of interfering with debugging.


## Common env issues
MacOS has a soft limit of 256 open file limit per process.
Some tests currently require more than 256 open files.
Here we significantly increase that and persist across reboots.

```sh
sudo tee /Library/LaunchDaemons/limit.maxfiles.plist >/dev/null <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple/DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key>
      <string>limit.maxfiles</string>
    <key>ProgramArguments</key>
      <array>
        <string>launchctl</string>
        <string>limit</string>
        <string>maxfiles</string>
        <string>8192</string>
        <string>81920</string>
      </array>
    <key>RunAtLoad</key>
      <true/>
    <key>ServiceIPC</key>
      <false/>
  </dict>
</plist>
EOF

sudo chown root:wheel /Library/LaunchDaemons/limit.maxfiles.plist
sudo chmod 644 /Library/LaunchDaemons/limit.maxfiles.plist

sudo launchctl load -w /Library/LaunchDaemons/limit.maxfiles.plist
```
## Misc
use the `IRYS_CUSTOM_TMP_DIR` env var to change the temporary directory used for tests from ./.tmp to whatever path you like. it can also be set to another env var to lookup and resolve as a path at runtime.
