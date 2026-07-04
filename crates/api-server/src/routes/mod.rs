pub mod anchor;
pub mod balance;
pub mod block;
pub mod block_index;
pub mod block_tree;
pub mod commitment;
pub mod config;
pub mod get_chunk;
pub mod index;
pub mod ledger;
pub mod mempool;
pub mod mining;
pub mod peer_list;
pub mod post_chunk;
pub mod price;
pub mod proxy;
pub mod storage;
pub mod supply;
pub mod tip;
pub mod tx;

/// Shared parse for `{ledger_id}` path params (numeric [`DataLedger`] ids:
/// 0 = Publish, 1 = Submit, 10 = OneYear, 20 = ThirtyDay).
pub(crate) fn parse_ledger_id(
    ledger_id: u32,
) -> Result<irys_types::DataLedger, crate::error::ApiError> {
    irys_types::DataLedger::try_from(ledger_id).map_err(|_| {
        (
            format!("Invalid ledger id: {ledger_id}"),
            awc::http::StatusCode::BAD_REQUEST,
        )
            .into()
    })
}
