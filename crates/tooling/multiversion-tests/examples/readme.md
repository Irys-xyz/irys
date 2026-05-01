example set of configurations for running multiversion tests from a significantly older version (d071fc03af2721a6eb58ffcc20fd234162f49ecc) to HEAD (fd107a5e9280498ae84fcccc784ffceeeb6f8fa8)
full command:
```
cargo xtask multiversion-test \
  --profile dev \
  --old-ref d071fc03af2721a6eb58ffcc20fd234162f49ecc \
  --new-ref CURRENT \
  --base-config-old ./crates/tooling/multiversion-tests/examples/base-config-old.toml \
  --base-config-new ./crates/tooling/multiversion-tests/examples/base-config-new.toml \
  --run-config ./crates/tooling/multiversion-tests/examples/run-config-d071fc03.toml \
  -- --no-fail-fast
```
