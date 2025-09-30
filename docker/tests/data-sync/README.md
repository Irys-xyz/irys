# Data Sync Test

Remote data synchronization test for validating partition data sync across a multi-node Irys cluster.

## Prerequisites

- Docker and Docker Compose installed and running
- Irys built: `cargo build`

## Running the Test

### Automated (Recommended)
```bash
./run_test.sh
```

This script automatically manages containers and runs the test.

### Manual
```bash
docker compose down && docker compose up -d --force-recreate --remove-orphans && \
NODE_URLS="http://localhost:19080,http://localhost:19081,http://localhost:19082" \
cargo test --package irys-chain --test mod sync_partition_data_remote_test -- --nocapture --ignored
```

Stop containers when done:
```bash
docker compose down -v
```

## Configuration

- 3 Irys nodes on ports 19080, 19081, 19082
- Environment variables (can be overridden) :
  - `MIN_HEIGHT`: Height when peer nodes start in the Docker compose (default: 40 for node 2, 50 for node 3)
  - `PEER_START_HEIGHT`: Height when rust test begins peer interaction (default: 60)
