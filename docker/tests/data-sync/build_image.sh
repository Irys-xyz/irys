#!/usr/bin/env bash
set -e

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
REPO_ROOT="$SCRIPT_DIR/../../.."

echo "Building debug image..."
docker build -f "$REPO_ROOT/docker/Dockerfile.debug" -t irys:debug "$REPO_ROOT"
echo "Done. Image: irys:debug"
