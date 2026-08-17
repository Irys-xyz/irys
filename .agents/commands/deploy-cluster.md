Deploy and manage the local Irys Docker test cluster.

## Arguments

$ARGUMENTS

## Instructions

You are deploying/managing the local Docker test cluster used for simulating a multi-node Irys network on localhost. Follow the steps below.

### Step 1: Determine configuration

The test cluster has these configurable parameters with defaults:

| Parameter | Default | Config key |
|---|---|---|
| Nodes | 3 | (docker-compose service count) |
| Chunk size | 262144 (256 KB) | `chunk_size` |
| Chunks/partition | 100 (= 25 MB partition) | `num_chunks_in_partition` |
| Partitions/slot | 1 | `num_partitions_per_slot` |
| Chunks/recall range | 10 | `num_chunks_in_recall_range` |
| Epoch length | 4 blocks | `num_blocks_in_epoch` |

If the user provided arguments above, parse them for any overrides to these parameters.

If no arguments were provided, ask the user:
- Deploy with defaults, or customize?
- If customize, which parameters to change and to what values?

### Step 2: Apply config to TOML files

**Always** write the resolved configuration (defaults + any user overrides) into all three TOML config files before deploying. The TOML files on disk may have stale values from a previous run. The consensus parameters live under `[consensus.Custom]` in each node's config:

- `docker/agent_cluster/configs/irys-1.toml`
- `docker/agent_cluster/configs/irys-2.toml`
- `docker/agent_cluster/configs/irys-3.toml`

Read one config first to understand the structure, then apply the resolved values to all three. The keys to set are: `chunk_size`, `num_chunks_in_partition`, `num_partitions_per_slot`, `num_chunks_in_recall_range` (under `[consensus.Custom]`), and `num_blocks_in_epoch` (under `[consensus.Custom.epoch]`).

**IMPORTANT — Consensus config consistency**: All three node configs MUST have **byte-identical** `[consensus.Custom]` sections, including ALL nested sections (`genesis`, `reth`, `mempool`, `difficulty_adjustment`, `vdf`, `block_reward_config`, `epoch`, `ema`, `hardforks`). The entire `ConsensusConfig` struct is hashed (keccak256 over canonical JSON) and compared during peer handshakes — ANY difference causes a mismatch error and nodes will fail to sync.

Common pitfalls to avoid:
- `[consensus.Custom.genesis]` — `miner_address` and `reward_address` must be the **same on every node** (use irys-1's address). Per-node identity comes from the top-level `mining_key` and `reward_address`, NOT the genesis section.
- `expected_genesis_hash` — This field is **excluded from the consensus hash** so it does not cause handshake mismatches. On irys-1 (Genesis), omit it entirely. On peer configs (irys-2, irys-3), keep it as `expected_genesis_hash = ""` — this is a placeholder that `start_peer_with_auto_restart.sh` replaces at runtime by fetching the real hash from irys-1's `/v1/genesis` API.
- `max_allowed_vdf_fork_steps` and other VDF params — must match across all configs.

After editing, verify consistency: `docker/agent_cluster/agent_cluster.sh validate-configs`. This strips `expected_genesis_hash` and comments, then diffs the `[consensus.Custom]` sections across all configs.

If the user wants fewer than 3 nodes, only start the corresponding services from the docker-compose file. Do NOT delete configs or compose services.

### Step 2b: Print resolved configuration

Before deploying, always print the resolved configuration values to the user so they can confirm what's being used. Display them as a code block like this:

```
Cluster configuration:
  Nodes:               3
  Chunk size:          262144 (256 KB)
  Chunks/partition:    100
  Partitions/slot:     1
  Chunks/recall range: 10
  Epoch length:        4 blocks
```

Substitute the actual resolved values (defaults or user overrides).

### Step 3: Build and deploy

Deploy the full cluster in one step. The `start_peer_with_auto_restart.sh` wrapper on peer nodes automatically fetches the genesis hash from irys-1's `/v1/genesis` API and injects it into the config before starting the binary. No manual hash extraction is needed.

```bash
docker/agent_cluster/agent_cluster.sh deploy [OPTIONS]
```

Available deploy flags:
- `-n, --nodes N`: Number of nodes (1, 2, or 3; default: 3)
- `-k, --keep-data`: Keep existing volumes (don't wipe)
- `-s, --skip-build`: Skip build, use existing image
- `-f, --force-rebuild`: Force rebuild even if source unchanged
- `-r, --ref REF`: Build from a specific git branch/tag/SHA instead of the working tree

The deploy command also runs `validate-configs` automatically to catch any consensus config drift before starting the cluster. It prints a status table at the end showing each node's health, block height, peer count, and mining address.

**IMPORTANT**: After the deploy command finishes, copy the raw status table output (the last lines starting from the NODE/STATUS header through the last node row) directly into your response as a code block. Do NOT reformat it into a markdown table or run a separate status command — just extract the table from the deploy output and display it.

### Other available commands

The script supports additional commands for cluster management:

```bash
docker/agent_cluster/agent_cluster.sh <command> [options]
```

| Command | Description |
|---|---|
| `build [-f] [-r REF]` | Build the irys:debug image (incremental, cached) |
| `deploy [OPTIONS]` | Build + deploy cluster |
| `hotdeploy [NODE]` | Copy new binary into containers + restart. No arg = atomic all-node deploy (stop-all, copy, start-all). NODE = single node (1/2/3) |
| `restart [NODE]` | Restart node(s) without wiping data |
| `status` | Show health and block heights for all running nodes |
| `logs [NODE] [GREP]` | Show recent logs, optionally filtered |
| `stop` | Stop cluster (preserves volumes) |
| `destroy` | Stop cluster and remove volumes |
| `exec NODE CMD...` | Run a command inside a node container |
| `validate-configs` | Check consensus config consistency across all node configs |

### Important notes

- The debug image includes bash, so `docker exec -it <container> bash` works for all nodes.
- Node configs live in `docker/agent_cluster/configs/`. If you modified them for custom parameters, remind the user they can revert with `git checkout docker/agent_cluster/configs/`.
- Peer nodes (test-irys-2, test-irys-3) depend on test-irys-1 and will wait for it to be healthy before starting.
- `hotdeploy` (no node arg) does an atomic deploy: stops all nodes first, copies the binary, then starts all nodes. This prevents protocol-incompatible nodes from running simultaneously.
- `hotdeploy N` (single node) does a rolling deploy for quick iteration.
- Use `--ref` to build from a specific branch/tag/SHA. The local Dockerfiles are always used; only the source code comes from the specified ref.
