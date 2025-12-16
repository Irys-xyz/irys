#!/usr/bin/env bash
set -e

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
REPO_ROOT="$SCRIPT_DIR/.."

IMAGE_NAME="${IMAGE_NAME:-irys:release}"

echo "========================================"
echo "Building Irys Release Image"
echo "========================================"
echo "Dockerfile: docker/Dockerfile.release"
echo "Image: $IMAGE_NAME"
echo ""

cd "$REPO_ROOT"

docker build \
    -t "$IMAGE_NAME" \
    -f docker/Dockerfile.release \
    .

echo ""
echo "========================================"
echo "Image built: $IMAGE_NAME"
echo "========================================"
