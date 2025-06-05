# irys

## Development setup

```cli
git submodule update --init --recursive --remote
```

other deps (for cuda):

```
clang & a C/C++ build toolchain
gmp
pkg-config
```

Local development commands:

```cli
cargo xtask --help

cargo xtask check
cargo xtask test
cargo xtask unused-deps
cargo xtask typos
```

## Testing

Testing code examples in comments

```cli
cargo test --doc
```

Testing Block Serialization

```cli
cargo test -p irys-types  -- --nocapture
```

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
