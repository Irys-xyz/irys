#!/usr/bin/env bash
set -e

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
REPO_ROOT="$SCRIPT_DIR/../../.."

IMAGE_NAME="${IMAGE_NAME:-localhost/irys-test:latest}"

echo "========================================"
echo "Building Irys Docker Image"
echo "========================================"
echo "Image: $IMAGE_NAME"
echo "Context: $REPO_ROOT"
echo ""

cd "$REPO_ROOT"

# Use DOCKER_BUILDKIT=0 to avoid buildx issues
DOCKER_BUILDKIT=0 docker build \
    -t "$IMAGE_NAME" \
    -f docker/Dockerfile.release \
    .

echo ""
echo "========================================"
echo "✓ Image built: $IMAGE_NAME"
echo "========================================"
