//! Restarting with a changed `.irys_submodules.toml` must not change what any
//! drive holds (mainnet node-1, 2026-11-06: removing one dead drive's path
//! moved 30 other drives onto new partitions and wiped them).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use irys_config::submodules::{SUBMODULES_CONFIG_FILE_NAME, StorageSubmodulesConfig};
use irys_domain::{ChunkType, PACKING_PARAMS_FILE_NAME};
use irys_types::storage::Interval;
use irys_types::{H256, NodeConfig, PartitionChunkOffset};

use crate::utils::IrysNodeTest;

const SECONDS_TO_WAIT: usize = 30;
const NUM_SUBMODULES: usize = 5;
const NUM_BLOCKS_IN_EPOCH: u64 = 3;
const MIGRATION_DEPTH: u32 = 2;

/// What a drive holds, keyed by its configured path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DriveState {
    partition_hash: Option<H256>,
    entropy: Vec<Interval<PartitionChunkOffset>>,
    reset_incomplete: bool,
    packing_params: Vec<u8>,
}

fn drive_states(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    paths: &[PathBuf],
) -> BTreeMap<PathBuf, DriveState> {
    let base = node.node_ctx.config.node_config.base_directory.clone();
    node.node_ctx
        .storage_modules_guard
        .read()
        .iter()
        .map(|sm| {
            // A module's id is its index in the configured path list.
            let path = paths[sm.id].clone();
            let params = std::fs::read(base.join(&path).join(PACKING_PARAMS_FILE_NAME))
                .expect("packing params file");
            (
                path,
                DriveState {
                    partition_hash: sm.partition_hash(),
                    entropy: sm.get_intervals(ChunkType::Entropy),
                    reset_incomplete: sm.reset_incomplete(),
                    packing_params: params,
                },
            )
        })
        .collect()
}

/// Rewrites `.irys_submodules.toml` the way an operator edits it.
fn write_submodule_paths(base: &Path, paths: &[PathBuf]) -> eyre::Result<()> {
    let quoted: Vec<String> = paths
        .iter()
        .map(|path| format!("{:?}", path.display().to_string()))
        .collect();
    let file = base.join(SUBMODULES_CONFIG_FILE_NAME);
    std::fs::write(
        &file,
        format!(
            "is_using_hardcoded_paths = true\nsubmodule_paths = [{}]\n",
            quoted.join(", ")
        ),
    )?;
    let reread = StorageSubmodulesConfig::from_toml(&file, irys_types::NodeMode::Genesis)?;
    eyre::ensure!(reread.submodule_paths == paths, "{reread:?}");
    Ok(())
}

/// Starts a genesis node with five drives, mines through two epochs, stops it,
/// rewrites the submodule list with `edit`, starts it again and checks that
/// every configured drive kept its partition, entropy and packing file.
async fn restart_with_edited_paths(
    edit: impl FnOnce(&[PathBuf]) -> Vec<PathBuf>,
) -> eyre::Result<()> {
    let mut config = NodeConfig::testing_with_epochs(NUM_BLOCKS_IN_EPOCH as usize);
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().block_migration_depth = MIGRATION_DEPTH;

    let node = IrysNodeTest::new_genesis(config);
    let base = node.cfg.base_directory.clone();
    let paths_before =
        StorageSubmodulesConfig::load_for_test(base.clone(), NUM_SUBMODULES)?.submodule_paths;
    let node = node
        .start_and_wait_for_packing("GENESIS", SECONDS_TO_WAIT)
        .await;

    // Two epochs, then migrate the last epoch block so the restarted node
    // computes the same epoch snapshot it stopped with.
    let last_epoch = 2 * NUM_BLOCKS_IN_EPOCH;
    node.mine_blocks(last_epoch as usize + MIGRATION_DEPTH as usize)
        .await?;
    node.wait_for_block_in_index(last_epoch, false, SECONDS_TO_WAIT)
        .await?;
    node.wait_for_packing(SECONDS_TO_WAIT).await;

    let before = drive_states(&node, &paths_before);
    assert_eq!(before.len(), NUM_SUBMODULES, "{before:#?}");
    for (path, state) in &before {
        assert!(
            state.partition_hash.is_some(),
            "{path:?} holds no partition before the restart: {before:#?}"
        );
        assert!(!state.entropy.is_empty(), "{path:?} is not packed");
    }

    let stopped = node.stop().await;
    let paths_after = edit(&paths_before);
    write_submodule_paths(&base, &paths_after)?;
    let node = stopped.start().await;
    node.wait_for_packing(SECONDS_TO_WAIT).await;

    let after = drive_states(&node, &paths_after);
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        {
            let mut kept: Vec<_> = paths_after.iter().collect();
            kept.sort();
            kept
        },
        "every configured drive is opened"
    );
    for (path, state) in &after {
        let old = &before[path];
        assert_eq!(
            state.partition_hash, old.partition_hash,
            "{path:?} changed partition"
        );
        assert_eq!(state.entropy, old.entropy, "{path:?} entropy changed");
        assert!(!state.reset_incomplete, "{path:?} was reset");
        assert_eq!(
            String::from_utf8_lossy(&state.packing_params),
            String::from_utf8_lossy(&old.packing_params),
            "{path:?} packing params file changed"
        );
    }

    node.stop().await;
    Ok(())
}

/// Removing one drive's path leaves every other drive as it was.
#[test_log::test(tokio::test)]
async fn heavy_restart_with_a_path_removed_changes_no_drive() -> eyre::Result<()> {
    restart_with_edited_paths(|paths| {
        let mut kept = paths.to_vec();
        kept.remove(1);
        kept
    })
    .await
}

/// Reversing the drive list leaves every drive as it was.
#[test_log::test(tokio::test)]
async fn heavy_restart_with_paths_reordered_changes_no_drive() -> eyre::Result<()> {
    restart_with_edited_paths(|paths| paths.iter().rev().cloned().collect()).await
}
