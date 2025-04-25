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
    create_dir_all(&base_directory);
    // write genesis block to disk
    let mut file = File::create(Path::new(&base_directory).join(GENESIS_BLOCK_FILENAME))?;
    file.write_all(json.as_bytes())?;

    Ok(())
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
