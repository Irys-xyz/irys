use std::sync::Arc;

use reth_db::DatabaseEnv;

#[derive(Debug, Clone)]
pub struct IrysRethProvider {
    pub db: Arc<DatabaseEnv>,
}
