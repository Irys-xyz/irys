#!/usr/bin/env bash
set -e

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
REPO_ROOT="$SCRIPT_DIR/../../.."

IMAGE_NAME="${IMAGE_NAME:-localhost/irys-test:latest}"
ENABLE_TELEMETRY="${ENABLE_TELEMETRY:-false}"

echo "========================================"
echo "Building Irys Docker Image"
echo "========================================"
echo "Image: $IMAGE_NAME"
echo "Context: $REPO_ROOT"
echo "Telemetry: $ENABLE_TELEMETRY"
echo ""

cd "$REPO_ROOT"

BUILD_ARGS=()
if [ "$ENABLE_TELEMETRY" = "true" ] || [ "$ENABLE_TELEMETRY" = "1" ]; then
    BUILD_ARGS+=(--build-arg CARGO_FEATURES=telemetry)
    echo "Building with telemetry support..."
else
    echo "Building without telemetry (set ENABLE_TELEMETRY=true to enable)"
fi

# Use DOCKER_BUILDKIT=0 to avoid buildx issues
DOCKER_BUILDKIT=0 docker build \
    "${BUILD_ARGS[@]}" \
    -t "$IMAGE_NAME" \
    -f docker/Dockerfile.release \
    .

echo ""
echo "========================================"
echo "Image built: $IMAGE_NAME"
echo "========================================"
