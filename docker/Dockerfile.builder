# One-time builder image with Rust toolchain + all system deps pre-installed.
# Built once, then reused for fast incremental compiles via bind mounts.
FROM rust:1.86-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config clang cmake git ca-certificates libgmp-dev m4 file perl rsync mold \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
