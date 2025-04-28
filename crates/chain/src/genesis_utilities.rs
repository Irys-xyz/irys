use irys_types::IrysBlockHeader;
use std::{
    fs::{create_dir_all, File},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

const GENESIS_BLOCK_FILENAME: &str = ".genesis.json";

/// Write genesis block to disk
pub fn save_genesis_block_to_disk(
    genesis_block: Arc<IrysBlockHeader>,
    base_directory: &PathBuf,
) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(&genesis_block)
        .expect("genesis block should convert to json string");
    // ensure base_directory exists and create if not
    // TODO this dir creation should be handled in a single place in the application, it's currently also done by the storage module
    if let Err(e) = create_dir_all(&base_directory) {
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
    let genesis: IrysBlockHeader = serde_json::from_reader(reader)
        .expect("genesis.json should be valid JSON and match IrysBlockHeader");

    Ok(Arc::new(genesis))
}
