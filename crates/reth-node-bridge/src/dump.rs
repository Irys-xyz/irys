use alloy_consensus::BlockHeader;
use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256};
use irys_database::reth_db::{
    self, cursor::*, transaction::*, Bytecodes, PlainAccountState, PlainStorageState,
    StageCheckpoints,
};
use reth_provider::HeaderProvider;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write as _};
use std::path::PathBuf;
use tracing::info;

// structs taken from  (ext)/reth/crates/storage/db-common/src/init.rs:607+
// these shouldn't ever change (as external dumps exist from other tooling)

/// Type to deserialize state root from state dump file.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateRoot {
    pub root: B256,
}

/// An account as in the state dump file. This contains a [`GenesisAccount`] and the account's
/// address.
#[derive(Debug, Serialize, Deserialize)]
pub struct GenesisAccountWithAddress {
    /// The account's balance, nonce, code, and storage.
    #[serde(flatten)]
    genesis_account: GenesisAccount,
    /// The account's address.
    address: Address,
}

/// Writes a JSONL formatted dump of the current execution state (Balances, nonces, bytecode, storage)\
/// with the first line being a `state_root` entry to `<dump_base_path>/accounts-<latest_block_number>.jsonl`
/// returning the path to this file.\
/// This generated file is designed to be compatible with Reth's existing `init-state` command, for loading larger genesis states.
/// note: there is a (possible) hazard here where the latest reth block might not be the same state that's represented in the State tables\
/// so it is recommended you capture either when the node is off, or a few seconds after it's produced a block.
pub fn dump_state<P>(
    reth_db: impl reth_db::Database,
    header_provider: &P,
    dump_base_path: PathBuf,
) -> eyre::Result<PathBuf>
where
    P: HeaderProvider,
    P::Header: BlockHeader,
{
    let read_tx = reth_db.tx()?;
    // read the latest block
    let latest_reth_block = read_tx
        .get::<StageCheckpoints>("Finish".to_owned())?
        .map(|ch| ch.block_number)
        .expect("unable to get latest reth block");

    // Headers are stored in static files, so we need to use the HeaderProvider to access them
    let latest_reth_block_header = header_provider
        .header_by_number(latest_reth_block)?
        .expect("To get latest block header");

    let row_count = read_tx.entries::<PlainAccountState>()?;

    let mut read_cursor = read_tx.cursor_read::<PlainAccountState>()?;

    let mut walker = read_cursor.walk(None)?;

    fs::create_dir_all(&dump_base_path)?;

    let dump_file_name = format!("accounts-{}.jsonl", &latest_reth_block);

    let dump_path = dump_base_path.join(&dump_file_name);
    let file = File::create(&dump_path)?;

    info!(
        "Saving {} accounts @ block {} to {:?}",
        &row_count, &latest_reth_block, &dump_path
    );

    let mut writer = BufWriter::new(file);

    serde_json::to_writer(
        &mut writer,
        &StateRoot {
            root: latest_reth_block_header.state_root(),
        },
    )?;

    writer.write_all(b"\n")?; // serde_json doesn't write a newline

    let mut accounts_saved = 0;
    let log_batch = 100;

    // peer 2, 3, 4, 5, 6, 7, 8, 9, 10
    let peer_addresses = [
        "0x81c23e4bde4c7086400cdcbca2dfe9a96dbd0fad",
        "0x12268eae1d5a3607bfa6e0c7f5fd1407f03e3bd7",
        "0x2a506815924e0db0b9e226f4aa362c0e3f6944a4",
        "0x5f9e44ec965f44a5bd941b620b409206d21ce176",
        "0x94cb7dec3942722cf1f9cf9d4e0fb95c2d235aad",
        "0x50ff82c6aa8ccddae1674b1f936f3b171d49761f",
        "0x11901031b594465477fd7408e0a5d3c6a6d89ff8",
        "0x60267208d1fa09d2b392fbb2e0ce60dfcc195312",
        "0x577b412bc03804496a1f787280c66dcd82873375",
    ];

    while let Some((address, account)) = walker.next().transpose()? {
        // bytecode
        let bytecode = account
            .bytecode_hash
            .and_then(|bch| read_tx.get::<Bytecodes>(bch).unwrap().map(|bc| bc.bytes()));

        // storage
        let mut dup_read_cursor = read_tx.cursor_dup_read::<PlainStorageState>()?;
        let walk = dup_read_cursor.walk_dup(Some(address), None)?;

        let mut storage_map = BTreeMap::new();
        for v in walk {
            let (_a, se) = v?;
            storage_map.insert(se.key, se.value.into());
        }

        let genesis_account = GenesisAccount {
            nonce: Some(account.nonce),
            balance: if peer_addresses.contains(&address.to_string().to_lowercase().as_str()) {
                // give the peers 10mil
                alloy_primitives::U256::from(10_000_000_u128 * 1000000000000000000_u128)
            } else {
                account.balance
            },
            code: bytecode,
            storage: Some(storage_map),
            private_key: None,
        };

        let account_with_address = GenesisAccountWithAddress {
            genesis_account,
            address,
        };

        serde_json::to_writer(&mut writer, &account_with_address)?;
        writer.write_all(b"\n")?; // JSONL

        accounts_saved += 1;

        if accounts_saved % log_batch == 0 {
            info!("Saved {}/{} accounts", &accounts_saved, &row_count);
        }
    }

    writer.flush()?;

    read_tx.commit()?;

    info!("Accounts saved to {:?}", &dump_path);

    Ok(dump_path)
}
