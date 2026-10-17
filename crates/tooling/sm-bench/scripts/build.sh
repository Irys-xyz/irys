#!/bin/bash
# Release build of irys-sm-bench into its own target directory.
#
# usage: build.sh [repo-dir]
#
# repo-dir defaults to the current directory.
# CARGO_TARGET_DIR defaults to <repo-dir>/../target when unset, so the build
# does not share the checkout's target directory.
# JOBS defaults to 8. NICE defaults to 15.

set -euo pipefail

repo=${1:-.}
repo=$(cd "$repo" && pwd)
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    CARGO_TARGET_DIR=$(cd "$repo/.." && pwd)/target
fi
export CARGO_TARGET_DIR
jobs=${JOBS:-8}
nice_level=${NICE:-15}

if [ -d "$HOME/.cargo/bin" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

cd "$repo"
nice -n "$nice_level" cargo build --release -j "$jobs" -p irys-sm-bench
echo "exit=$?"
echo "binary=$CARGO_TARGET_DIR/release/irys-sm-bench"
