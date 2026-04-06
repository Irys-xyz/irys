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

# Capture git metadata from the host — .git is excluded from Docker context
GIT_SHA="$(git rev-parse --short=7 HEAD 2>/dev/null || echo unknown)"
GIT_HAS_TAG="$(git describe --exact-match --tags HEAD 2>/dev/null && echo true || echo false)"
GIT_DIRTY="$(git diff-index --quiet HEAD -- 2>/dev/null && echo false || echo true)"
GIT_COMMIT="$(git rev-parse HEAD 2>/dev/null || echo unknown)"

echo "Git SHA: $GIT_SHA (dirty=$GIT_DIRTY, tagged=$GIT_HAS_TAG)"

BUILD_ARGS=(
    --build-arg "GIT_SHA=$GIT_SHA"
    --build-arg "GIT_HAS_TAG=$GIT_HAS_TAG"
    --build-arg "GIT_DIRTY=$GIT_DIRTY"
    --build-arg "GIT_COMMIT=$GIT_COMMIT"
)
if [ "$ENABLE_TELEMETRY" = "true" ] || [ "$ENABLE_TELEMETRY" = "1" ]; then
    BUILD_ARGS+=(--build-arg CARGO_FEATURES=telemetry)
    echo "Building with telemetry support..."
else
    echo "Building without telemetry (set ENABLE_TELEMETRY=true to enable)"
fi

docker build \
    "${BUILD_ARGS[@]}" \
    --platform linux/amd64 \
    --load \
    -t "$IMAGE_NAME" \
    -f docker/Dockerfile.debug \
    .

echo ""
echo "========================================"
echo "Image built: $IMAGE_NAME"
echo "========================================"
