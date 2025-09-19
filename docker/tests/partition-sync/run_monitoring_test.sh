#!/usr/bin/env bash
set -e

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
cd "$SCRIPT_DIR"

# Parse command line arguments
BUILD_IMAGE=true
while [[ $# -gt 0 ]]; do
    case $1 in
        --no-build)
            BUILD_IMAGE=false
            shift
            ;;
        -h|--help)
            echo "Usage: $0 [--no-build] [--help]"
            echo ""
            echo "Options:"
            echo "  --no-build    Skip building Docker images, use existing ones"
            echo "  --help        Show this help message"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

echo "========================================"
echo "Irys Partition Data Sync Monitoring Test"
echo "========================================"

# Function to cleanup on exit
cleanup() {
    echo ""
    echo "Cleaning up..."
    docker compose down -v 2>/dev/null || true
}

# Set up trap to cleanup on exit
trap cleanup EXIT

# 1. Check if irys binary exists
echo "1. Checking for irys binary..."
if [ ! -f "../../../target/debug/irys" ]; then
    echo "ERROR: irys binary not found at ../../../target/debug/irys"
    echo "Please build it first with: cargo build --bin irys"
    echo "And ensure it's in target/debug/ directory"
    exit 1
fi
echo "✓ irys binary found"

# 2. Stop any existing test containers
echo ""
echo "2. Stopping any existing test containers..."
docker compose down -v 2>/dev/null || true

# 3. Build and start the test cluster
echo ""
if [ "$BUILD_IMAGE" = true ]; then
    echo "3. Building and starting 3-node test cluster..."
    docker compose up --build -d
else
    echo "3. Starting 3-node test cluster (using existing images)..."
    docker compose up -d
fi

# 4. Wait for nodes to be ready
echo ""
echo "4. Waiting for nodes to initialize..."
sleep 10

# 5. Check node connectivity
echo ""
echo "5. Checking node connectivity..."
for port in 9080 9081 9082; do
    echo -n "  Checking localhost:$port... "
    max_attempts=30
    attempt=0
    
    while [ $attempt -lt $max_attempts ]; do
        if curl -s http://localhost:$port/v1/info > /dev/null 2>&1; then
            echo "✓ Ready"
            break
        fi
        attempt=$((attempt + 1))
        if [ $attempt -eq $max_attempts ]; then
            echo "✗ Failed"
            echo "ERROR: Node on port $port not responding after 30 seconds"
            exit 1
        fi
        sleep 1
    done
done

# 6. Run the monitoring partition sync test
echo ""
echo "6. Running monitoring partition sync test..."
echo "========================================"
cd scripts
python3 monitoring_test.py
test_result=$?

# 7. Show final status
echo ""
echo "========================================"
if [ $test_result -eq 0 ]; then
    echo "✓ TEST PASSED: Monitoring partition sync test completed successfully!"
else
    echo "✗ TEST FAILED: Monitoring partition sync test encountered errors"
fi

# 8. Optional: Keep containers running for inspection
echo ""
read -p "Keep containers running for inspection? (y/n) " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    docker compose down -v
else
    echo ""
    echo "Containers are still running. To monitor sync status:"
    echo "  cd $SCRIPT_DIR/scripts"
    echo "  python3 sync_monitor.py"
    echo ""
    echo "To stop containers:"
    echo "  cd $SCRIPT_DIR"
    echo "  docker compose down -v"
fi

exit $test_result
