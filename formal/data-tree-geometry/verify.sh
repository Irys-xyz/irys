#!/usr/bin/env bash
# Run the Kani proofs. About 10 minutes when the build cache is warm.
# See README.md. Does not install Kani.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

missing() {
  printf 'missing %s\n' "$1" >&2
  exit 1
}

command -v cargo >/dev/null 2>&1 || missing cargo
command -v cargo-kani >/dev/null 2>&1 || missing 'cargo-kani (cargo install --locked kani-verifier && cargo kani setup)'

cd "$root"
cargo kani
