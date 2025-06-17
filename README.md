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
See .devcontainer/setup.sh for more information.

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
