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

```
cargo xtask --help

cargo xtask check
cargo xtask test
cargo xtask unused-deps
cargo xtask typos
```

## Testing

Testing Block Serialization

```cli
cargo test -p irys-types  -- --nocapture
```
