#!/usr/bin/env bash
# setup-sccache.sh — create the sccache-wrap alias at ~/.local/bin/sccache-wrap
#
# The `cc` crate auto-detects RUSTC_WRAPPER and, when the binary is named
# exactly "sccache", also wraps the C compiler with it. This breaks vendored
# OpenSSL: sccache can't create temp files next to /dev/null (Permission
# denied on /dev/.tmpXXXX), so OpenSSL's perlasm probe for GNU `as` fails
# silently and it emits Solaris-style `#alloc` section flags that GNU
# assembler rejects.
#
# Fix: a thin "sccache-wrap" shell script whose name doesn't match the cc
# crate's VALID_WRAPPERS list ("sccache", "cachepot", "buildcache"). Cargo
# still routes all rustc invocations through it, but the cc crate leaves CC
# alone.
#
# Reference: cc crate src/lib.rs `rustc_wrapper_fallback()` — checks
# Path::file_stem(RUSTC_WRAPPER) against VALID_WRAPPERS.
#
# Usage:
#   scripts/setup-sccache.sh
#   # Then point rustc-wrapper at ~/.local/bin/sccache-wrap, e.g.:
#   export CARGO_BUILD_RUSTC_WRAPPER="$HOME/.local/bin/sccache-wrap"
set -euo pipefail

WRAPPER="$HOME/.local/bin/sccache-wrap"

if ! command -v sccache >/dev/null 2>&1; then
  echo "setup-sccache: sccache not found on PATH, skipping" >&2
  exit 0
fi

if [ -x "$WRAPPER" ] && "$WRAPPER" --show-stats >/dev/null 2>&1; then
  echo "setup-sccache: $WRAPPER already exists"
  exit 0
fi

mkdir -p "$(dirname "$WRAPPER")"
printf '#!/bin/sh\nexec sccache "$@"\n' > "$WRAPPER"
chmod +x "$WRAPPER"
echo "setup-sccache: created $WRAPPER"
