#!/usr/bin/env bash
#
# agent_cluster.sh — Agentic-focused local Irys cluster management.
#
# Bundles build, deploy, restart, status, logs, and teardown into one tool.
# Designed for Claude Code / AI-agent workflows where the cluster is rebuilt
# and redeployed frequently.
#
# Usage:
#   agent_cluster.sh <command> [options]
#
# Commands:
#   build [OPTIONS]       Build the irys:local image (incremental, uses cargo cache)
#     -f, --force           Force rebuild even if source unchanged
#     -r, --ref REF         Build from a git branch/tag/SHA instead of working tree
#     --native              Skip Docker, build natively (cross-compile on macOS)
#   deploy [OPTIONS]      Build + deploy cluster (wipes volumes by default)
#     -n, --nodes N         Number of nodes (1-3, default 3)
#     -k, --keep-data       Keep existing volumes (don't wipe)
#     -s, --skip-build      Skip build, use existing image
#     -f, --force-rebuild   Force rebuild even if source unchanged
#     -r, --ref REF         Build from a git branch/tag/SHA instead of working tree
#     --native              Skip Docker for build (cross-compile on macOS)
#   restart [NODE]        Restart node(s) without wiping data
#                          No arg = rolling restart all; NODE = single node (1/2/3)
#   status                Show health and block heights for all running nodes
#   logs [NODE] [GREP]    Show recent logs, optionally filtered
#   stop                  Stop cluster (preserves volumes)
#   destroy               Stop cluster and remove volumes
#   hotdeploy [NODE]      Copy new binary into containers + restart
#                          Preserves writable layer (index.dat, DB, etc.)
#                          No arg = atomic all-node deploy (stop-all, copy, start-all)
#                          NODE = single node rolling deploy (1/2/3)
#   clean [target|all]    Remove build caches (default: target only)
#   exec NODE CMD...      Run a command inside a node container
#   validate-configs      Check consensus config consistency across all nodes
#
set -euo pipefail

# ── Paths ────────────────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$SCRIPT_DIR/../.."
BUILD_OUTPUT="$REPO_ROOT/docker/build-output"
HASH_FILE="$BUILD_OUTPUT/.source_hash"
CARGO_CACHE_DIR="${HOME}/.cache/irys-docker-cargo"
COMPOSE_FILE="$SCRIPT_DIR/docker-compose.yaml"
CONFIG_DIR="$SCRIPT_DIR/configs"

# ── Platform detection ─────────────────────────────────────────────────────

HOST_OS="$(uname -s)"  # Darwin or Linux

# Platform-aware defaults (overridable via env)
if [[ "$HOST_OS" == "Darwin" ]]; then
    # macOS Docker Desktop: conservative defaults (VM shares host resources)
    CARGO_JOBS="${CARGO_JOBS:-4}"
    BUILD_MEMORY="${BUILD_MEMORY:-14g}"
else
    # Native Linux: use more resources (no VM overhead)
    CARGO_JOBS="${CARGO_JOBS:-$(( $(nproc 2>/dev/null || echo 4) / 2 ))}"
    local_mem=$(free -g 2>/dev/null | awk '/Mem:/{print $2}' || echo 0)
    BUILD_MEMORY="${BUILD_MEMORY:-$(( local_mem * 3 / 4 > 0 ? local_mem * 3 / 4 : 14 ))g}"
    unset local_mem
fi

ALL_NODES=(test-irys-1 test-irys-2 test-irys-3)
NODE_PORTS=(19080 19081 19082)

# ── Formatting ───────────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
DIM='\033[2m'
RESET='\033[0m'

info()  { printf "${CYAN}==> %s${RESET}\n" "$*"; }
ok()    { printf "${GREEN} ✓  %s${RESET}\n" "$*"; }
warn()  { printf "${YELLOW}⚠   %s${RESET}\n" "$*"; }
err()   { printf "${RED}✗  %s${RESET}\n" "$*" >&2; }
die()   { err "$@"; exit 1; }

elapsed() {
    local t=$1
    if (( t >= 60 )); then printf "%dm%ds" $((t / 60)) $((t % 60))
    else printf "%ds" "$t"; fi
}

# ── Compose wrapper ──────────────────────────────────────────────────────────

dc() { docker compose -p agent-cluster -f "$COMPOSE_FILE" "$@"; }

# ── Source hash (for smart rebuild) ──────────────────────────────────────────

source_hash() {
    (
        cd "$REPO_ROOT"
        { find crates -name '*.rs' -o -name 'Cargo.toml' -o -name 'build.rs' | sort
          echo "Cargo.lock"
          echo "Cargo.toml"
          echo "docker/Dockerfile.builder"
          echo "docker/Dockerfile.local"
        } | xargs cat 2>/dev/null | shasum -a 256 | cut -d' ' -f1
    )
}

# ── Build ────────────────────────────────────────────────────────────────────

cmd_build() {
    local force=false
    local build_ref=""
    local native=false
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -f|--force) force=true; shift ;;
            -r|--ref)
                build_ref="${2:?--ref requires a branch/tag/SHA}"
                shift 2 ;;
            --native) native=true; shift ;;
            *) die "build: unknown option: $1" ;;
        esac
    done

    # Validate ref if provided
    if [[ -n "$build_ref" ]]; then
        git -C "$REPO_ROOT" rev-parse --verify "$build_ref" > /dev/null 2>&1 \
            || die "build: ref '$build_ref' does not exist"
    fi

    local start=$SECONDS

    # Smart skip check
    local current_hash stored_hash=""
    if [[ -n "$build_ref" ]]; then
        current_hash=$(git -C "$REPO_ROOT" rev-parse "$build_ref")
    else
        current_hash=$(source_hash)
    fi
    [[ -f "$HASH_FILE" ]] && stored_hash=$(cat "$HASH_FILE")
    local hash_changed=true
    if [[ "$current_hash" == "$stored_hash" ]] \
       && [[ -f "$BUILD_OUTPUT/irys" ]] \
       && docker image inspect irys:local > /dev/null 2>&1; then
        hash_changed=false
    fi

    if [[ "$hash_changed" == false ]]; then
        if [[ "$force" == false ]]; then
            ok "Source unchanged — skipping build ($(elapsed $(( SECONDS - start ))))"
            return 0
        else
            warn "No source changes detected — forcing rebuild"
        fi
    fi

    mkdir -p "$BUILD_OUTPUT"

    # ── Native build path (no Docker) ──────────────────────────────────────
    if [[ "$native" == true ]]; then
        if [[ -n "$build_ref" ]]; then
            die "build: --native and --ref cannot be combined (native builds use the working tree)"
        fi

        if [[ "$HOST_OS" == "Darwin" ]]; then
            info "Native cross-compile (aarch64-unknown-linux-gnu) with -j$CARGO_JOBS..."
            (cd "$REPO_ROOT" && \
                CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-unknown-linux-gnu-gcc \
                cargo build --release --bin irys -p irys-chain --locked \
                --target aarch64-unknown-linux-gnu -j"$CARGO_JOBS")
            cp "$REPO_ROOT/target/aarch64-unknown-linux-gnu/release/irys" "$BUILD_OUTPUT/irys"
        else
            info "Native Linux build with -j$CARGO_JOBS..."
            (cd "$REPO_ROOT" && cargo build --release --bin irys -p irys-chain --locked -j"$CARGO_JOBS")
            cp "$REPO_ROOT/target/release/irys" "$BUILD_OUTPUT/irys"
        fi

        # Package runtime image
        info "Packaging runtime image..."
        docker build -f "$REPO_ROOT/docker/Dockerfile.local" -t irys:local "$REPO_ROOT"

        # Store hash
        source_hash > "$HASH_FILE"

        ok "Native build complete ($(elapsed $(( SECONDS - start ))))"
        return 0
    fi

    # ── Docker build path ──────────────────────────────────────────────────

    # Ensure builder image
    if ! docker image inspect irys-builder:latest > /dev/null 2>&1; then
        info "Building irys-builder image (one-time)..."
        docker build -f "$REPO_ROOT/docker/Dockerfile.builder" -t irys-builder:latest "$REPO_ROOT/docker"
    fi

    # Compile
    info "Compiling Linux binary (Docker, -j$CARGO_JOBS, mem=$BUILD_MEMORY)..."

    # Platform-aware volume mounts
    local VOLUME_ARGS=()
    if [[ "$HOST_OS" == "Darwin" ]]; then
        # macOS: named volumes avoid VirtioFS overhead (use Linux VM's native ext4)
        VOLUME_ARGS=(
            -v cargo-registry:/usr/local/cargo/registry
            -v cargo-git:/usr/local/cargo/git
            -v rustup:/usr/local/rustup
            -v workspace-target:/workspace-target
        )
    else
        # Linux: bind mounts are optimal (zero overhead on native Docker)
        mkdir -p "$CARGO_CACHE_DIR/registry" "$CARGO_CACHE_DIR/git" \
                 "$CARGO_CACHE_DIR/target" "$CARGO_CACHE_DIR/rustup"
        VOLUME_ARGS=(
            -v "$CARGO_CACHE_DIR/registry:/usr/local/cargo/registry"
            -v "$CARGO_CACHE_DIR/git:/usr/local/cargo/git"
            -v "$CARGO_CACHE_DIR/rustup:/usr/local/rustup"
            -v "$CARGO_CACHE_DIR/target:/workspace-target"
        )
    fi

    # Pipe source via tar to avoid Docker Desktop VirtioFS file truncation bugs.
    # This bypasses the macOS file sharing layer entirely.
    local src_vol="irys-build-src"
    docker volume create "$src_vol" > /dev/null 2>&1 || true

    # Sync source into build volume.
    # For --ref builds, skip re-extract if the volume already has the same ref
    # (avoids mtime changes that cause cargo to do a full rebuild).
    local ref_marker_file="$BUILD_OUTPUT/.volume_ref"
    local need_sync=true

    if [[ -n "$build_ref" ]]; then
        local resolved_ref
        resolved_ref=$(git -C "$REPO_ROOT" rev-parse "$build_ref")
        if [[ -f "$ref_marker_file" ]] && [[ "$(cat "$ref_marker_file")" == "$resolved_ref" ]]; then
            info "Build volume already has ref $build_ref — skipping source sync"
            need_sync=false
        fi
    fi

    # Track changed files for build container logging
    local sync_changes_file="$BUILD_OUTPUT/.sync_changes"
    rm -f "$sync_changes_file"

    if [[ "$need_sync" == true ]]; then
        info "Syncing source into build volume (rsync — only changed files)..."
        if [[ -n "$build_ref" ]]; then
            # Export source from the specified git ref, rsync into volume
            # so only files with changed content get new mtimes (enables
            # cargo incremental builds).
            git -C "$REPO_ROOT" archive --format=tar "$build_ref" \
            | docker run --rm -i \
                -v "$src_vol:/workspace" \
                -v "$BUILD_OUTPUT:/build-output" \
                irys-builder:latest \
                bash -c '
                    mkdir -p /tmp/staging && tar xf - -C /tmp/staging
                    rsync -a --checksum --delete -i /tmp/staging/ /workspace/ \
                        | grep -v "^\." | sed "s/^[^ ]* //" > /build-output/.sync_changes || true
                    rm -rf /tmp/staging
                '
            # Record which ref is on the volume
            local resolved_ref
            resolved_ref=$(git -C "$REPO_ROOT" rev-parse "$build_ref")
            info "Building ref: $build_ref (${resolved_ref:0:12})"
            echo "$resolved_ref" > "$ref_marker_file"
        else
            # Use working tree — rsync into volume for incremental builds
            local head_ref head_sha dirty=""
            head_ref=$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "detached")
            head_sha=$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null | head -c 12)
            git -C "$REPO_ROOT" diff --quiet 2>/dev/null || dirty=" (dirty)"
            info "Building working tree: $head_ref (${head_sha})${dirty}"
            tar -C "$REPO_ROOT" -cf - \
                --exclude='./target' --exclude='./.git' --exclude='./docker/build-output' \
                --exclude='./crates/.tmp' --exclude='./venv' \
                . \
            | docker run --rm -i \
                -v "$src_vol:/workspace" \
                -v "$BUILD_OUTPUT:/build-output" \
                irys-builder:latest \
                bash -c '
                    mkdir -p /tmp/staging && tar xf - -C /tmp/staging
                    rsync -a --checksum --delete -i /tmp/staging/ /workspace/ \
                        | grep -v "^\." | sed "s/^[^ ]* //" > /build-output/.sync_changes || true
                    rm -rf /tmp/staging
                '
            # Clear ref marker since volume now has working tree
            rm -f "$ref_marker_file"
        fi
    fi

    docker run --rm \
        --memory="$BUILD_MEMORY" --memory-swap=-1 \
        -e HASH_CHANGED="$hash_changed" \
        -v "$src_vol:/workspace:ro" \
        "${VOLUME_ARGS[@]}" \
        -v "$BUILD_OUTPUT:/build-output" \
        -w /workspace \
        irys-builder:latest \
        bash -c '
            if [ "$HASH_CHANGED" = "false" ]; then
                echo "⚠  No source changes detected — forced rebuild"
            fi
            if [ -s /build-output/.sync_changes ]; then
                echo "==> Changed files:"
                cat /build-output/.sync_changes
                echo ""
            elif [ -f /build-output/.sync_changes ]; then
                echo "==> No files changed since last sync"
            fi
            echo "==> Starting cargo build (-j'"$CARGO_JOBS"', mem='"$BUILD_MEMORY"')..."
            CARGO_TARGET_DIR=/workspace-target cargo build --release --bin irys -p irys-chain --locked -j'"$CARGO_JOBS"' \
            && cp /workspace-target/release/irys /build-output/irys
        '

    # Package runtime image
    info "Packaging runtime image..."
    docker build -f "$REPO_ROOT/docker/Dockerfile.local" -t irys:local "$REPO_ROOT"

    # Store hash
    if [[ -n "$build_ref" ]]; then
        git -C "$REPO_ROOT" rev-parse "$build_ref" > "$HASH_FILE"
    else
        source_hash > "$HASH_FILE"
    fi

    ok "Build complete ($(elapsed $(( SECONDS - start ))))"
}

# ── Clean ────────────────────────────────────────────────────────────────────

cmd_clean() {
    local target="${1:-target}"
    case "$target" in
        target)
            info "Removing build target cache..."
            if [[ "$HOST_OS" == "Darwin" ]]; then
                docker volume rm workspace-target 2>/dev/null || true
            else
                rm -rf "$CARGO_CACHE_DIR/target"
            fi
            ok "Target cache cleared"
            ;;
        all)
            info "Removing all build caches..."
            if [[ "$HOST_OS" == "Darwin" ]]; then
                docker volume rm cargo-registry cargo-git rustup workspace-target 2>/dev/null || true
            else
                rm -rf "$CARGO_CACHE_DIR"
            fi
            docker volume rm irys-build-src 2>/dev/null || true
            ok "All build caches cleared"
            ;;
        *) die "clean: expected 'target' or 'all', got '$target'" ;;
    esac
}

# ── Deploy ───────────────────────────────────────────────────────────────────

cmd_deploy() {
    local nodes=3 keep_data=false skip_build=false force_rebuild=false build_ref="" native=false
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -n|--nodes)
                nodes="${2:?--nodes requires 1, 2, or 3}"
                [[ "$nodes" =~ ^[123]$ ]] || die "--nodes must be 1, 2, or 3"
                shift 2 ;;
            -k|--keep-data)     keep_data=true; shift ;;
            -s|--skip-build)    skip_build=true; shift ;;
            -f|--force-rebuild) force_rebuild=true; shift ;;
            -r|--ref)
                build_ref="${2:?--ref requires a branch/tag/SHA}"
                shift 2 ;;
            --native) native=true; shift ;;
            *) die "deploy: unknown option: $1" ;;
        esac
    done

    # Build
    local build_args=()
    [[ -n "$build_ref" ]] && build_args+=(--ref "$build_ref")
    [[ "$native" == true ]] && build_args+=(--native)

    if [[ "$skip_build" == true ]]; then
        docker image inspect irys:local > /dev/null 2>&1 || die "irys:local image not found. Remove --skip-build."
    elif [[ "$force_rebuild" == true ]]; then
        cmd_build --force "${build_args[@]:+${build_args[@]}}"
    else
        cmd_build "${build_args[@]:+${build_args[@]}}"
    fi

    # Validate config consistency before deploying
    cmd_validate_configs

    # Tear down
    if [[ "$keep_data" == true ]]; then
        info "Stopping cluster (keeping volumes)..."
        dc down 2>/dev/null || true
    else
        info "Stopping cluster and removing volumes..."
        dc down -v 2>/dev/null || true
    fi

    # Start
    info "Starting $nodes node(s)..."
    case "$nodes" in
        1) dc up -d test-irys-1 ;;
        2) dc up -d test-irys-1 test-irys-2 ;;
        3) dc up -d ;;
    esac

    # Health check
    wait_healthy "$nodes"

    # Show cluster status
    cmd_status
}

# ── Restart ──────────────────────────────────────────────────────────────────

cmd_restart() {
    local target="${1:-all}"

    if [[ "$target" == "all" ]]; then
        # Rolling restart: one node at a time
        for i in 0 1 2; do
            local node="${ALL_NODES[$i]}"
            local port="${NODE_PORTS[$i]}"

            # Skip nodes that aren't running
            if ! docker ps --format '{{.Names}}' | grep -q "^${node}$"; then
                continue
            fi

            info "Restarting $node..."
            dc stop "$node"
            dc up -d "$node"
            wait_for_node "$node" "$port" 90
            ok "$node is back"
        done
        ok "Rolling restart complete"
    elif [[ "$target" =~ ^[123]$ ]]; then
        local idx=$((target - 1))
        local node="${ALL_NODES[$idx]}"
        local port="${NODE_PORTS[$idx]}"

        info "Restarting $node..."
        dc stop "$node"
        dc up -d "$node"
        wait_for_node "$node" "$port" 90
        ok "$node is back"
    else
        die "restart: expected 1, 2, 3, or 'all', got '$target'"
    fi
}

# ── Status ───────────────────────────────────────────────────────────────────

cmd_status() {
    printf "\n${BOLD}%-14s %-10s %8s %8s  %-6s  %s${RESET}\n" \
        "NODE" "STATUS" "HEIGHT" "BI_HT" "PEERS" "MINING ADDR"
    printf "%s\n" "────────────────────────────────────────────────────────────────────"

    for i in 0 1 2; do
        local node="${ALL_NODES[$i]}"
        local port="${NODE_PORTS[$i]}"

        # Check if container is running
        if ! docker ps --format '{{.Names}}' | grep -q "^${node}$"; then
            printf "%-14s ${DIM}%-10s${RESET}\n" "$node" "stopped"
            continue
        fi

        # Fetch info
        local info_json
        info_json=$(curl -sf "http://localhost:${port}/v1/info" 2>/dev/null) || {
            printf "%-14s ${YELLOW}%-10s${RESET}\n" "$node" "starting"
            continue
        }

        local height bi_height peers mining syncing
        height=$(echo "$info_json" | jq -r '.height // "?"')
        bi_height=$(echo "$info_json" | jq -r '.blockIndexHeight // "?"')
        peers=$(echo "$info_json" | jq -r '.peerCount // "?"')
        mining=$(echo "$info_json" | jq -r '.miningAddress // "?"')
        syncing=$(echo "$info_json" | jq -r '.isSyncing // false')

        local status_str="${GREEN}healthy${RESET}"
        if [[ "$syncing" == "true" ]]; then
            status_str="${YELLOW}syncing${RESET}"
        fi

        printf "%-14s ${status_str}  %8s %8s  %-6s  %s\n" \
            "$node" "$height" "$bi_height" "$peers" "$mining"
    done
    echo ""
}

# ── Logs ─────────────────────────────────────────────────────────────────────

cmd_logs() {
    local target="${1:-all}"
    local filter="${2:-}"
    local lines=100

    if [[ "$target" == "all" ]]; then
        if [[ -n "$filter" ]]; then
            for node in "${ALL_NODES[@]}"; do
                if docker ps --format '{{.Names}}' | grep -q "^${node}$"; then
                    local matches
                    matches=$(docker logs "$node" 2>&1 | grep -i "$filter" | tail -20)
                    if [[ -n "$matches" ]]; then
                        printf "${BOLD}── %s ──${RESET}\n" "$node"
                        echo "$matches"
                        echo ""
                    fi
                fi
            done
        else
            dc logs --tail="$lines"
        fi
    elif [[ "$target" =~ ^[123]$ ]]; then
        local node="test-irys-${target}"
        if [[ -n "$filter" ]]; then
            docker logs "$node" 2>&1 | grep -i "$filter" | tail -50
        else
            docker logs --tail="$lines" "$node" 2>&1
        fi
    else
        die "logs: expected 1, 2, 3, or 'all', got '$target'"
    fi
}

# ── Stop / Destroy ───────────────────────────────────────────────────────────

cmd_stop() {
    info "Stopping cluster (volumes preserved)..."
    dc down 2>/dev/null || true
    ok "Cluster stopped"
}

cmd_destroy() {
    info "Stopping cluster and removing volumes..."
    dc down -v 2>/dev/null || true
    ok "Cluster destroyed"
}

# ── Hot Deploy ────────────────────────────────────────────────────────────────

cmd_hotdeploy() {
    local target="${1:-all}"
    local binary="$BUILD_OUTPUT/irys"

    [[ -f "$binary" ]] || die "No binary at $binary. Run 'build' first."

    if [[ "$target" == "all" ]]; then
        # Atomic all-node deploy: stop-all → copy-all → start-all
        # Prevents protocol-incompatible nodes from running simultaneously.
        local running_nodes=()
        local running_ports=()

        for i in 0 1 2; do
            local node="${ALL_NODES[$i]}"
            if docker ps --format '{{.Names}}' | grep -q "^${node}$"; then
                running_nodes+=("$node")
                running_ports+=("${NODE_PORTS[$i]}")
            fi
        done

        if [[ ${#running_nodes[@]} -eq 0 ]]; then
            die "No running nodes found"
        fi

        # Phase 1: Stop all running nodes
        info "Stopping ${#running_nodes[@]} node(s)..."
        for node in "${running_nodes[@]}"; do
            docker stop "$node" > /dev/null
        done
        ok "All nodes stopped"

        # Phase 2: Copy binary to all stopped containers
        info "Copying binary to ${#running_nodes[@]} container(s)..."
        for node in "${running_nodes[@]}"; do
            docker cp "$binary" "${node}:/app/irys"
        done
        ok "Binary copied to all containers"

        # Phase 3: Start all nodes
        info "Starting ${#running_nodes[@]} node(s)..."
        for node in "${running_nodes[@]}"; do
            docker start "$node" > /dev/null
        done

        # Phase 4: Wait for health on all nodes
        for idx in "${!running_nodes[@]}"; do
            wait_for_node "${running_nodes[$idx]}" "${running_ports[$idx]}" 90
        done
        ok "Atomic hot deploy complete"
    elif [[ "$target" =~ ^[123]$ ]]; then
        local idx=$((target - 1))
        local node="${ALL_NODES[$idx]}"
        local port="${NODE_PORTS[$idx]}"

        docker ps --format '{{.Names}}' | grep -q "^${node}$" \
            || die "$node is not running"

        info "Hot-deploying to $node..."
        docker cp "$binary" "${node}:/app/irys"
        docker restart "$node"
        wait_for_node "$node" "$port" 90
        ok "$node updated"
    else
        die "hotdeploy: expected 1, 2, 3, or 'all', got '$target'"
    fi
}

# ── Exec ─────────────────────────────────────────────────────────────────────

cmd_exec() {
    local target="${1:?exec requires a node number (1/2/3)}"
    shift
    [[ "$target" =~ ^[123]$ ]] || die "exec: expected node 1, 2, or 3"
    local node="test-irys-${target}"
    docker exec "$node" "$@"
}

# ── Helpers ──────────────────────────────────────────────────────────────────

wait_for_node() {
    local node="$1" port="$2" timeout="${3:-60}"
    local deadline=$(( SECONDS + timeout ))
    printf "  Waiting for %s (port %s)... " "$node" "$port"
    while true; do
        if curl -sf "http://localhost:${port}/v1/info" > /dev/null 2>&1; then
            printf "${GREEN}up${RESET}\n"
            return 0
        fi
        if (( SECONDS >= deadline )); then
            printf "${RED}timeout${RESET}\n"
            die "$node not responsive after ${timeout}s. Check: docker logs $node"
        fi
        sleep 2
    done
}

wait_healthy() {
    local nodes="$1"
    info "Waiting for nodes to become healthy..."
    for i in $(seq 0 $(( nodes - 1 ))); do
        wait_for_node "${ALL_NODES[$i]}" "${NODE_PORTS[$i]}" 60
    done
    ok "All $nodes node(s) responding"
}

# ── Validate Configs ─────────────────────────────────────────────────────

cmd_validate_configs() {
    info "Validating consensus config consistency across nodes..."

    local tmp_dir
    tmp_dir=$(mktemp -d)
    trap "rm -rf '$tmp_dir'" RETURN

    # Extract [consensus.Custom] section from each config, stripping
    # expected_genesis_hash (legitimately differs) and comment/blank lines.
    local cfg
    for cfg in "$CONFIG_DIR"/irys-1.toml "$CONFIG_DIR"/irys-2.toml "$CONFIG_DIR"/irys-3.toml; do
        local name
        name=$(basename "$cfg" .toml)
        sed -n '/^\[consensus\.Custom\]/,$ p' "$cfg" \
            | grep -v '^expected_genesis_hash' \
            | grep -v '^#' \
            | grep -v '^[[:space:]]*$' \
            > "$tmp_dir/$name" || true
    done

    local base="$tmp_dir/irys-1"
    local ok_flag=true

    local name
    for name in irys-2 irys-3; do
        if ! diff -u "$base" "$tmp_dir/$name" > "$tmp_dir/diff-$name" 2>&1; then
            err "Consensus config mismatch between irys-1 and $name:"
            cat "$tmp_dir/diff-$name" >&2
            ok_flag=false
        fi
    done

    if [[ "$ok_flag" == true ]]; then
        ok "All consensus configs are consistent"
    else
        die "Fix the config inconsistencies above before deploying"
    fi
}

# ── Usage ────────────────────────────────────────────────────────────────────

usage() {
    sed -n '2,/^$/{ /^#/s/^# *//p; }' "$0"
    exit 0
}

# ── Main ─────────────────────────────────────────────────────────────────────

COMMAND="${1:-}"
shift || true

case "$COMMAND" in
    build)   cmd_build "$@" ;;
    deploy)  cmd_deploy "$@" ;;
    restart) cmd_restart "$@" ;;
    status)  cmd_status "$@" ;;
    logs)    cmd_logs "$@" ;;
    stop)    cmd_stop "$@" ;;
    destroy)   cmd_destroy "$@" ;;
    hotdeploy) cmd_hotdeploy "$@" ;;
    clean)     cmd_clean "$@" ;;
    exec)      cmd_exec "$@" ;;
    validate-configs) cmd_validate_configs "$@" ;;
    -h|--help|help|"") usage ;;
    *) die "Unknown command: $COMMAND (try --help)" ;;
esac
