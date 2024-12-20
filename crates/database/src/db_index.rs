/// Once data has been well confirmed by being part of a transaction that is
/// in a block with several confirmations, it can move out of the `db_cache`
/// and into a `db_index` that is able to store more properties (to support
/// mining) now that the data is confirmed.
const x: u64 = 1;
