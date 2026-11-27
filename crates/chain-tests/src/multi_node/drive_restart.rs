//! Restarting with a changed `.irys_submodules.toml` must not change what any
//! drive holds (mainnet node-1, 2026-11-06: removing one dead drive's path
//! moved 30 other drives onto new partitions and wiped them).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use irys_chain::IrysNodeCtx;
use irys_config::submodules::StorageSubmodulesConfig;
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
    node: &IrysNodeTest<IrysNodeCtx>,
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
    StorageSubmodulesConfig {
        is_using_hardcoded_paths: true,
        submodule_paths: paths.to_vec(),
    }
    .write_to(base)?;
    Ok(())
}

/// A genesis node with five drives, mined through two epochs with every drive
/// packed. Returns the node, its base directory, the configured paths and
/// what each drive holds.
async fn mined_node() -> eyre::Result<(
    IrysNodeTest<IrysNodeCtx>,
    PathBuf,
    Vec<PathBuf>,
    BTreeMap<PathBuf, DriveState>,
)> {
    let mut config = NodeConfig::testing_with_epochs(NUM_BLOCKS_IN_EPOCH as usize);
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().block_migration_depth = MIGRATION_DEPTH;

    let node = IrysNodeTest::new_genesis(config);
    let base = node.cfg.base_directory.clone();
    let paths =
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

    let before = drive_states(&node, &paths);
    assert_eq!(before.len(), NUM_SUBMODULES, "{before:#?}");
    for (path, state) in &before {
        assert!(
            state.partition_hash.is_some(),
            "{path:?} holds no partition before the restart: {before:#?}"
        );
        assert!(!state.entropy.is_empty(), "{path:?} is not packed");
    }
    Ok((node, base, paths, before))
}

/// Every drive in `after` holds what it held in `before`, and none was reset.
fn assert_unchanged(before: &BTreeMap<PathBuf, DriveState>, after: &BTreeMap<PathBuf, DriveState>) {
    for (path, state) in after {
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
}

/// Starts a genesis node with five drives, mines through two epochs, stops it,
/// rewrites the submodule list with `edit`, starts it again and checks that
/// every configured drive kept its partition, entropy and packing file.
async fn restart_with_edited_paths(
    edit: impl FnOnce(&[PathBuf]) -> Vec<PathBuf>,
) -> eyre::Result<()> {
    let (node, base, paths_before, before) = mined_node().await?;

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
    assert_unchanged(&before, &after);

    node.stop().await;
    Ok(())
}

/// Every file under `dir`, by path relative to it, with its bytes.
fn files_under(dir: &Path) -> eyre::Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(path.strip_prefix(dir)?.to_path_buf(), std::fs::read(&path)?);
            }
        }
    }
    Ok(files)
}

/// A drive whose packing file is garbage at startup stops the node: startup
/// fails with an error naming that drive, and no drive is skipped. Nothing on
/// the corrupt drive changes (no reset marker, no rewritten file). (A second
/// start in the same process is not attempted: the failed start's Reth
/// database lock outlives it in this harness.)
#[test_log::test(tokio::test)]
async fn heavy_restart_with_a_corrupt_drive_file_fails_naming_the_drive() -> eyre::Result<()> {
    let (node, base, paths, _before) = mined_node().await?;

    let stopped = node.stop().await;
    let corrupt = paths[1].clone();
    let corrupt_dir = base.join(&corrupt);
    let params_path = corrupt_dir.join(PACKING_PARAMS_FILE_NAME);
    let garbage: &[u8] = b"partition_hash = [[[ not toml \xff\x00";
    std::fs::write(&params_path, garbage)?;
    let corrupt_files = files_under(&corrupt_dir)?;

    let Err((err, _stopped)) = stopped.try_start().await else {
        panic!("a node with a corrupt drive file must not start");
    };
    let message = format!("{err:#}");
    assert!(
        message.contains(&format!("drive {}", corrupt.display())),
        "the startup error does not name the drive: {message}"
    );
    assert!(
        message.contains("cannot be read"),
        "the startup error does not give the reason: {message}"
    );
    assert_eq!(
        std::fs::read(&params_path)?,
        garbage,
        "the corrupt packing file is left as it was"
    );
    assert!(
        files_under(&corrupt_dir)? == corrupt_files,
        "nothing on the corrupt drive changed (no reset marker, no write)"
    );
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
