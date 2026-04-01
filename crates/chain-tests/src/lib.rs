// Public utility module (always compiled) — contains IrysNodeTest and helpers
pub mod utils;

// Test modules — only compiled during `cargo test`
#[cfg(test)]
mod api;
#[cfg(test)]
mod block_production;
#[cfg(test)]
mod data_sync;
#[cfg(test)]
mod ema_pricing;
#[cfg(test)]
mod external;
#[cfg(test)]
mod integration;
#[cfg(test)]
mod multi_node;
#[cfg(test)]
mod packing;
#[cfg(test)]
mod partition_assignments;
#[cfg(test)]
mod perm_ledger_expiry;
#[cfg(test)]
mod programmable_data;
#[cfg(test)]
mod promotion;
#[cfg(test)]
mod startup;
#[cfg(test)]
mod synchronization;
#[cfg(test)]
mod term_ledger_expiry;
#[cfg(test)]
mod validation;
