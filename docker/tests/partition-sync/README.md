# Partition Data Sync Test

This test replicates the `slow_heavy_sync_partition_data_between_peers_test` using HTTP APIs and Docker Compose.

## Overview

The test validates partition data synchronization across a 3-node Irys cluster with the following configuration:
- 3 partition replicas per slot
- 4 blocks per epoch
- 32-byte chunk size
- 60 chunks per partition

## Prerequisites

1. Build the Irys binary:
```bash
cargo build --bin irys
# Ensure the binary is in target/debug/irys
```

2. Install Python dependencies:
```bash
cd docker/tests/partition-sync
pip install -r requirements.txt
# Or manually: pip install aiohttp asyncio
```

3. Ensure Docker and Docker Compose are installed

## Running the Monitoring Test

### Quick Start

Run the complete test suite:
```bash
./run_monitoring_test.sh
```


If you have built the images prior and just want to run the script without building then you can use the `--no-build` flag. 
```bash
./run_monitoring_test.sh --no-build
```

This script will:
1. Build and start a 3-node test cluster
2. Wait for nodes to be ready
3. Run a simple node progression sync test
4. Report results

## Expected Results

The test validates:
1. Successful cluster formation with 3 nodes
2. Proper partition assignment after epoch transitions
3. Data chunk synchronization across all assigned partitions
4. Storage interval consistency matching expected chunk counts:
   - Publish(0): 50 data chunks, 10 packed chunks
   - Submit(0): 50 data chunks, 10 packed chunks
   - Submit(1): 0 data chunks, 60 packed chunks

## Running the Full Test

### Quick Start

Run the complete test suite:
```bash
./run_full_test.sh
```


If you have built the images prior and just want to run the script without building then you can use the `--no-build` flag. 
```bash
./run_full_test.sh --no-build
```

This script will:
1. Build and start a 3-node test cluster
2. Wait for nodes to be ready
3. Run the comprehensive partition sync test
4. Report results

## Expected Results

The test validates:
1. Successful cluster formation with 3 nodes
2. Proper partition assignment after epoch transitions
3. Data chunk synchronization across all assigned partitions
4. Storage interval consistency matching expected chunk counts:
   - Publish(0): 50 data chunks, 10 packed chunks
   - Submit(0): 50 data chunks, 10 packed chunks
   - Submit(1): 0 data chunks, 60 packed chunks
