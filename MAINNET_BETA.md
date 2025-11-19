_Node Deployment Guide_
#irys-chain/specs 
# Requirements
High level overview of the Irys Node Software requirements
## Operating System
The Irys node software is built for and tested on __Linux__, specifically on __Ubuntu 22/24.04__.

While the source may be compiled for other targets, MacOS or Windows, it is entirely untested on those platforms.
## CPU
The Irys node uses multiple CPU cores; more cores mean better performance on compute-heavy tasks. One core is dedicated to computing the VDF hashes used in mining. You’ll want a high-performance core (ideally with SHA extensions) to stay competitive with the network and mine efficiently.

**Note:** the Irys node will pin core 0 for its VDF calculations. This is expected to take up 100% of this core, almost constantly.

We recommend CPUs like the __AMD Ryzen 9 3900__ or __AMD EPYC 4344P__ as both have high enough single core performance.
## GPU
Irys supports GPU-based packing to speed up the preparation of storage for the protocol. Currently, this requires an NVIDIA CUDA-capable GPU, and packing speed scales with the number of CUDA cores. This has been tested on __NVIDIA 3090__ and __5090__ cards, and all modern NVIDIA GPUs should work.
## Storage
Miners provide storage to the network in the form of partitions. These partitions are optimized to take advantage of __22TB HDDs__.  The read speeds required to mine a partition are capped at ~100MB/s, well within the range of HDD transfer speeds so by design there is no advantage to deploying SSD storage.

The minimum amount of storage a miner can provide to the protocol is a full 22TB partition with the ability to pledge multiple partitions to the protocol.

We recommend formatting your drives with XFS - it has demonstrated the best characteristics (fast I/O, low space overhead) out of the suitable mainline file systems.
# Building from source
First download the source for the latest tagged release at https://github.com/Irys-xyz/irys/releases
## Dependencies
* clang & a C/C++ build toolchain (some of the packing routines are written in C/C++)
* gmp
* pkg-config

To build and enable NVIDIA GPU accelerated matrix packing you must have the latest CUDA toolkit (12.6+) and gcc-13 as well as g++ 13

See `.devcontainer/setup.sh` for more information.
## Compiling
Once you’ve installed the dependencies you can compile the build with

`cargo build --bin irys --release`

**Note:** the default configuration is to aggressively optimize for the machine (CPU & GPU) the compiler is operating on. For this reason, we heavily recommend compiling the binary on each distinct machine (non-identical CPU & GPU) instead of passing around the built binary
## Compile feature flags
`telemetry` - enables exporting of opentelemetry log & span collection.
*  Use the canonical `OTEL_EXPORTER_OTLP_ENDPOINT` env var to configure the opentelemetry endpoint to send both spans and logs to.
* Optionally, specify the `AXIOM_API_TOKEN` and `AXIOM_DATASET` env vars to configure support for Axiom.
`nvidia` - enables CUDA accelerated packing. 

**Note:** GPU packing currently takes priority over CPU packing for bulk packing operations - future work will enable the node to use both simultaneously.

**Note:** Multiple GPUs are currently unsupported - ensure your highest performance card shows up as the first entry in `nvidia-smi` 
## Node Configuration
When it starts the Irys node will load its configuration from `./config.toml`, you can copy the template mainnet configuration from `crates/config/templates/mainnet_config.toml` and rename it  in the directory where you will run the executable.
Additionally, you can specify the full path to the configuration using the `CONFIG` env var.

Note that the “consensus” values that are in some of the other template configs are absent in the mainnet template. This is intentional as the “consensus” values for mainnet are hard coded into the node software itself.  It’s very important that all nodes operating on the network use the same set of hard coded consensus values.

```toml
 consensus = "Mainnet"
 node_mode = "Peer"
 sync_mode = "Full"
 base_directory = ".irys"
 mining_key = "0000000000000000000000000000000000000000000000000000000000000001"
 trusted_peers = []
 initial_stake_and_pledge_whitelist = []
 reward_address = "0x0000000000000000000000000000000000000000"
 stake_pledge_drives = false
 genesis_peer_discovery_timeout_millis = 10000
```

The main things to configure in this section of the template are `mining_key`, `reward_address`, `base_directory` and `trusted_peers`.

* `mining_key` is the hex encoded bytes of your node's private key which will be used for signing the blocks your node produces.
* `reward_address` is the hex encoded actress of the account that will receive the block rewards for any blocks your node produces. In most cases this will be the account address of your `mining_key` but it can be any valid account address you wish to receive the block rewards.
* `base_directory` is the path to the folder you want irys to store all it’s state in (consensus data)
**Note:** Irys will not store any protocol data in this folder, instead it will symlink to locations provided in the `submodules.toml` (see [Storage Configuration](#storage-configuration))
* `trusted_peers` This is a set of trusted peers (operated by Irys) that are used to bootstrap the node securely

```toml
 [gossip]
 public_ip = "127.0.0.1"
 public_port = 8081
 bind_ip = "0.0.0.0"
 bind_port = 8081

 [http]
 public_ip = "127.0.0.1"
 public_port = 8080
 bind_ip = "0.0.0.0"
 bind_port = 8080

 [reth.network]
 use_random_ports = false
 public_ip = "127.0.0.1"
 public_port = 30303
 bind_ip = "0.0.0.0"
 bind_port = 30303
```
The Irys node exposes three web services, each on its own port:
* `[http]` for user transactions and node discovery
* `[gossip]` for peer handshakes and p2p communication
* `[reth.network]` for standard EVM RPC
They can share the same IP, but each must run on a unique port to avoid conflicts.

**Note:** Reverse proxies are not currently supported, they will cause your node to have difficulty joining the network.

```toml
 [packing.local]
 cpu_packing_concurrency = 4
 gpu_packing_batch_size = 1024
```

The `[packing.local]` config controls how the node uses CPU and GPU resources for packing/unpacking chunks.

__CPU__ packing is faster per-chunk, ideal for validating chunks from other miners. Also used in syncing with the network initially.

__GPU__ packing is slower per core but can process thousands of chunks in parallel, making it best for bulk packing when packing a fresh partition for use in mining.
  * `cpu_packing_concurrency` sets how many CPU cores are used for parallel chunk operations.
  * `gpu_packing_batch_size` sets how many chunks are sent to the GPU at once. Typically this value is aligned with your GPU’s CUDA core count. If you don’t have a GPU, this value is ignored.

```toml
[vdf]
parallel_verification_thread_limit = 4
```

 `parallel_verification_thread_limit` controls the maximum number of threads to use when validating VDF steps in blocks. A value that is too low could lead to you lagging behind the network - we recommend setting this to 2-4 cores below the total available. VDF validation is very CPU intensive, especially when syncing with the chain for the first time.

```toml
[[oracles]]
```
The `[[oracles]]` sections can be left unchanged until the $IRYS token is listed on coingecko and coinmarket cap.

## Storage Configuration
To participate in mining on Irys you must provide drives for your assigned partitions to be stored on. Once provided and pledged the protocol will assign a partition hash which your node will use to pack your partition. Once packed the Irys node software will begin to mine the storage and you will be able to earn block rewards for any blocks you produce.

To tell the Irys Node Software what drives to use for partitions you must first configure them in the `<base directory>/irys_submodules.toml` (where `base_directory` is the one in your `config.toml`) - this file has a simple format:

```toml
submodule_paths = [
 "/Users/dmac/irys/storage_modules/submodule_0",
 "/Users/dmac/irys/storage_modules/submodule_1",
 "/Users/dmac/irys/storage_modules/submodule_2",
]
```

The irys node will automatically create symlinks to the specified mount points in the `<base_dir>/storage_modules` folder. 

Ensure the user running the executable has read-write permissions for drive.
# Running the node
Once you’ve configured the `./config.toml` file and `<base dir>/irys_submodules.toml` file, it’s time to run the node.

`cargo run --bin irys --release`

OR run the executable directly: 

`target/release/irys`
## Auto Stake and Pledge
By default your mining address needs to be staked before your storage modules can be pledged and mined. When you are ready to do this change the `stake_pledge_drives` config value in `./config.toml` to `true` and restart your node. If you have the funds in your miners account the node will automatically stake your mining address and pledge any storage modules you’ve configured for mining on Irys.
## Logs
The node supports the conventional `RUST_LOG` environment variable to configure logging. By default, the logging level is set to `info`. Note that levels “below” info (`debug`, `trace`) are very high volume, but debug is recommended if you can support it, as it means we'll be able to diagnose any issues you’re having faster.