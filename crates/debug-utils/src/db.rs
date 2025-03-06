use irys_database::{
    open_or_create_db,
    reth_db::{Database as _, DatabaseEnv},
    tables::{IngressProofs, IrysTables, IrysTxHeaders},
    walk_all,
};

pub fn load_db() -> DatabaseEnv {
    let path = "/workspaces/irys-rs/.irys/1/reth/db";
    open_or_create_db(path, IrysTables::ALL, None).unwrap()
}

fn _promotion_debug() -> eyre::Result<()> {
    let db = load_db();
    let read_tx = db.tx()?;
    let ingress_proofs = walk_all::<IngressProofs>(&read_tx)?;
    dbg!(ingress_proofs);
    let storage_transactions = walk_all::<IrysTxHeaders>(&read_tx)?;
    dbg!(storage_transactions);

    Ok(())
}
