# syntax=docker/dockerfile:1.6

# --- Builder stage: compile the Rust binary for the container's platform ---
FROM --platform=$BUILDPLATFORM rust:1.91-slim AS builder

ARG BUILDPLATFORM
ARG TARGETPLATFORM
WORKDIR /workspace

# Install build deps (vendored OpenSSL is enabled but we still need common build tools)
RUN apt-get update && apt-get install -y --no-install-recommends \
	build-essential pkg-config clang cmake git ca-certificates libgmp-dev m4 file && \
	rm -rf /var/lib/apt/lists/*

# Copy the full workspace (simpler and more robust than fine-grained caching here)
COPY . .

# Build the binary defined in crates/chain [[bin]] name = "irys"
# Use the existing debug-release profile to match compose expectations
# Reduce memory usage: single job, no incremental, minimal debuginfo, fewer codegen units
# ENV CARGO_BUILD_JOBS=1 \
# 	CARGO_INCREMENTAL=0 \
# 	RUSTFLAGS="-C debuginfo=0 -C codegen-units=1"
RUN cargo build --profile debug-release --bin irys -p irys-chain

# --- Runtime stage: minimal Ubuntu with ufw and tools ---
FROM ubuntu:24.04

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
	ufw curl iputils-ping iproute2 ca-certificates libgmp10 && \
	rm -rf /var/lib/apt/lists/*

# Copy the compiled binary and entrypoint script
COPY --from=builder /workspace/target/debug-release/irys /app/irys
COPY ./docker/entrypoint.sh /app/entrypoint.sh

RUN chmod +x /app/irys /app/entrypoint.sh

ENTRYPOINT ["/app/entrypoint.sh"]
