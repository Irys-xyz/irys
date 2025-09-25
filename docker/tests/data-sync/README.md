# Data Sync Test

This directory contains the remote data synchronization test for validating partition data sync across a multi-node Irys cluster using HTTP APIs.

## Overview

The test runs `sync_partition_data_remote_test` which validates data synchronization across a 3-node Irys cluster. The test interacts with nodes via HTTP APIs to:

1. Verify cluster formation and node connectivity
2. Post data transactions and monitor their propagation
3. Validate partition assignments and data chunk synchronization
4. Ensure all nodes sync data chunks to their assigned partitions

## Test Configuration

The cluster is configured with:
- 3 Irys nodes running on ports 19080, 19081, and 19082
- 3 partition replicas per slot
- 1 ingress proof required for promotion
- Network parameters defined in each node's configuration

## Prerequisites

1. **Build the Irys binary** (if not already built):
```bash
cargo build --bin irys
```

2. **Docker and Docker Compose** must be installed and running

## Running the Test

### Quick Start

Run the test with automatic container management:
```bash
./run_test.sh
```

The script will:
1. Check if Docker containers are already running
2. If not running, build and start the 3-node cluster via Docker Compose
3. Wait for all nodes to be ready (checking `/v1/info` endpoint)
4. Execute the `sync_partition_data_remote_test` cargo test
5. Automatically shut down containers when the test completes

### Test Execution Details

The test runs with the following environment:
- `NODE_URLS`: Set to the three node endpoints (http://localhost:19080,19081,19082)
- Test flags: `--nocapture --ignored` to show output and run ignored tests

### Manual Container Management

If you prefer to manage containers manually:

1. Start the cluster:
```bash
docker compose up -d --build
```

2. Run the test directly:
```bash
NODE_URLS="http://localhost:19080,http://localhost:19081,http://localhost:19082" \
cargo test --package irys-chain --test mod sync_partition_data_remote_test -- --nocapture --ignored
```

3. Stop the cluster when done:
```bash
docker compose down -v
```

## Test Phases

The `sync_partition_data_remote_test` executes these steps:

1. **Node Readiness**: Waits for all nodes to respond to health checks
2. **Network Configuration**: Fetches and validates network parameters
3. **Data Transaction**: Posts a test data transaction to the cluster
4. **Staking** (optional): Stakes additional signers for partition assignment
5. **Chain Progression**: Waits for epoch boundaries to trigger assignments
6. **Partition Verification**: Validates partition assignments across nodes
7. **Data Synchronization**: Monitors and validates chunk synchronization
8. **Mining Verification**: Ensures nodes can continue mining after sync

## Expected Behavior

- All three nodes should start successfully and be reachable
- The test should complete with "Test passed" message
- Containers are automatically cleaned up after test completion
- Exit code reflects test success (0) or failure (non-zero)

## Troubleshooting

### Containers Won't Start
- Check Docker daemon is running: `docker info`
- Ensure ports 19080-19082 are not in use: `lsof -i :19080`
- Check Docker Compose logs: `docker compose logs`

### Test Fails to Connect
- Verify nodes are responding: `curl http://localhost:19080/v1/info`
- Check container status: `docker ps`
- Review node logs: `docker compose logs irys-node-1`

### Test Timeout
- The test waits up to 30 seconds for each node to be ready
- If initialization takes longer, nodes may need more time or have configuration issues
- Check individual node logs for errors

```

## Related Tests

This test is the remote/HTTP API equivalent of the local `sync_partition_data_tests` found in the main test suite. It validates the same synchronization behavior but through external APIs rather than direct function calls.
