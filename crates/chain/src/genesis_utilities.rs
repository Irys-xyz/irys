use irys_types::{CommitmentTransaction, IrysBlockHeader, IrysBlockHeaderV1};
use std::{
    fs::{create_dir_all, File},
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};

const GENESIS_BLOCK_FILENAME: &str = ".irys_genesis.json";
const GENESIS_COMMITMENTS_FILENAME: &str = ".irys_genesis_commitments.json";

/// Write genesis block to disk
pub fn save_genesis_block_to_disk(
    genesis_block: Arc<IrysBlockHeader>,
    base_directory: &PathBuf,
) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(&genesis_block)
        .expect("genesis block should convert to json string");
    // ensure base_directory exists and create if not
    // TODO this dir creation should be handled in a single place in the application, it's currently also done by the storage module
    if let Err(e) = create_dir_all(base_directory) {
        panic!(
            "unable to recursively read or create directory \"{:?}\" error {}",
            base_directory, e
        );
    }
    // write genesis block to disk
    let mut file = File::create(Path::new(&base_directory).join(GENESIS_BLOCK_FILENAME))?;
    file.write_all(json.as_bytes())?;

    Ok(())
}

/// Check if genesis block exists on disk
pub fn genesis_block_exists_on_disk(base_directory: &PathBuf) -> bool {
    let path = Path::new(base_directory).join(GENESIS_BLOCK_FILENAME);
    path.is_file()
}

/// Read genesis block from disk
pub fn load_genesis_block_from_disk(
    base_directory: &PathBuf,
) -> std::io::Result<Arc<IrysBlockHeader>> {
    let file = File::open(Path::new(&base_directory).join(GENESIS_BLOCK_FILENAME))?;
    let reader = std::io::BufReader::new(file);
    let genesis: IrysBlockHeaderV1 = serde_json::from_reader(reader)
        .expect("genesis.json should be valid JSON and match IrysBlockHeaderV1");

    Ok(Arc::new(IrysBlockHeader::V1(genesis)))
}

/// Write genesis commitment transactions to disk as JSON.
pub fn save_genesis_commitments_to_disk(
    commitments: &[CommitmentTransaction],
    base_directory: &Path,
) -> eyre::Result<()> {
    let json = serde_json::to_string_pretty(commitments)
        .map_err(|e| eyre::eyre!("failed to serialize genesis commitments: {e}"))?;
    create_dir_all(base_directory)
        .map_err(|e| eyre::eyre!("failed to create directory {:?}: {e}", base_directory))?;
    let mut file = File::create(base_directory.join(GENESIS_COMMITMENTS_FILENAME))?;
    file.write_all(json.as_bytes())?;
    Ok(())
}

/// Read genesis commitment transactions from disk.
pub fn load_genesis_commitments_from_disk(
    base_directory: &Path,
) -> eyre::Result<Vec<CommitmentTransaction>> {
    let file = File::open(base_directory.join(GENESIS_COMMITMENTS_FILENAME))
        .map_err(|e| eyre::eyre!("failed to open {}: {e}", GENESIS_COMMITMENTS_FILENAME))?;
    let reader = std::io::BufReader::new(file);
    let commitments: Vec<CommitmentTransaction> = serde_json::from_reader(reader)
        .map_err(|e| eyre::eyre!("failed to parse {}: {e}", GENESIS_COMMITMENTS_FILENAME))?;
    Ok(commitments)
}
