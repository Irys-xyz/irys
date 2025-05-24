// //! Types for genesis configuration of a chain.

// use std::collections::BTreeMap;

// use reth_codecs::Compact;

// use crate::{
//     commitment::{Commitments, Stake},
//     shadow::Shadows,
//     Address,
// };
// use alloy_genesis::{ChainConfig, CliqueConfig};
// use alloy_primitives::{Bytes, B256, U256};
// use alloy_serde::storage::deserialize_storage_map;

// use serde::{Deserialize, Serialize};

// use super::last_tx::LastTx;

// // /// The genesis block specification.
// // #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
// // #[serde(rename_all = "camelCase", default)]
// // pub struct Genesis {
// //     /// The fork configuration for this network.
// //     #[serde(default)]
// //     pub config: ChainConfig,
// //     /// The genesis header nonce.
// //     #[serde(with = "u64_via_ruint")]
// //     pub nonce: u64,
// //     /// The genesis header timestamp.
// //     #[serde(with = "u64_via_ruint")]
// //     pub timestamp: u64,
// //     /// The genesis header extra data.
// //     pub extra_data: Bytes,
// //     /// The genesis header gas limit.
// //     #[serde(with = "u128_via_ruint")]
// //     pub gas_limit: u128,
// //     /// The genesis header difficulty.
// //     pub difficulty: U256,
// //     /// The genesis header mix hash.
// //     pub mix_hash: B256,
// //     /// The genesis header coinbase address.
// //     pub coinbase: Address,
// //     /// The initial state of accounts in the genesis block.
// //     pub alloc: BTreeMap<Address, GenesisAccount>,
// //     // NOTE: the following fields:
// //     // * base_fee_per_gas
// //     // * excess_blob_gas
// //     // * blob_gas_used
// //     // * number
// //     // should NOT be set in a real genesis file, but are included here for compatibility with
// //     // consensus tests, which have genesis files with these fields populated.
// //     /// The genesis header base fee
// //     #[serde(
// //         default,
// //         skip_serializing_if = "Option::is_none",
// //         with = "u128_opt_via_ruint"
// //     )]
// //     pub base_fee_per_gas: Option<u128>,
// //     /// The genesis header excess blob gas
// //     #[serde(
// //         default,
// //         skip_serializing_if = "Option::is_none",
// //         with = "u128_opt_via_ruint"
// //     )]
// //     pub excess_blob_gas: Option<u128>,
// //     /// The genesis header blob gas used
// //     #[serde(
// //         default,
// //         skip_serializing_if = "Option::is_none",
// //         with = "u128_opt_via_ruint"
// //     )]
// //     pub blob_gas_used: Option<u128>,
// //     /// The genesis block number
// //     #[serde(
// //         default,
// //         skip_serializing_if = "Option::is_none",
// //         with = "u64_opt_via_ruint"
// //     )]
// //     pub number: Option<u64>,
// //     #[serde(default, skip_serializing_if = "Option::is_none")]
// //     pub shadows: Option<Shadows>,
// // }

// #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
// #[serde(rename_all = "camelCase", default)]
// pub struct Genesis {
//     /// The fork configuration for this network.
//     #[serde(default)]
//     pub config: ChainConfig,
//     /// The genesis header nonce.
//     #[serde(with = "alloy_serde::quantity")]
//     pub nonce: u64,
//     /// The genesis header timestamp.
//     #[serde(with = "alloy_serde::quantity")]
//     pub timestamp: u64,
//     /// The genesis header extra data.
//     pub extra_data: Bytes,
//     /// The genesis header gas limit.
//     #[serde(with = "alloy_serde::quantity")]
//     pub gas_limit: u64,
//     /// The genesis header difficulty.
//     pub difficulty: U256,
//     /// The genesis header mix hash.
//     pub mix_hash: B256,
//     /// The genesis header coinbase address.
//     pub coinbase: Address,
//     /// The initial state of accounts in the genesis block.
//     pub alloc: BTreeMap<Address, GenesisAccount>,
//     // NOTE: the following fields:
//     // * base_fee_per_gas
//     // * excess_blob_gas
//     // * blob_gas_used
//     // * number
//     // should NOT be set in a real genesis file, but are included here for compatibility with
//     // consensus tests, which have genesis files with these fields populated.
//     /// The genesis header base fee
//     #[serde(
//         default,
//         skip_serializing_if = "Option::is_none",
//         with = "alloy_serde::quantity::opt"
//     )]
//     pub base_fee_per_gas: Option<u128>,
//     /// The genesis header excess blob gas
//     #[serde(
//         default,
//         skip_serializing_if = "Option::is_none",
//         with = "alloy_serde::quantity::opt"
//     )]
//     pub excess_blob_gas: Option<u128>,
//     /// The genesis header blob gas used
//     #[serde(
//         default,
//         skip_serializing_if = "Option::is_none",
//         with = "alloy_serde::quantity::opt"
//     )]
//     pub blob_gas_used: Option<u128>,
//     /// The genesis block number
//     #[serde(
//         default,
//         skip_serializing_if = "Option::is_none",
//         with = "alloy_serde::quantity::opt"
//     )]
//     pub number: Option<u64>,

//     #[serde(default, skip_serializing_if = "Option::is_none")]
//     pub shadows: Option<Shadows>,
// }

// impl Genesis {
//     /// Creates a chain config for Clique using the given chain id.
//     /// and funds the given address with max coins.
//     ///
//     /// Enables all hard forks up to London at genesis.
//     pub fn clique_genesis(chain_id: u64, signer_addr: Address) -> Genesis {
//         // set up a clique config with an instant sealing period and short (8 block) epoch
//         let clique_config = CliqueConfig {
//             period: Some(0),
//             epoch: Some(8),
//         };

//         let config = ChainConfig {
//             chain_id,
//             eip155_block: Some(0),
//             eip150_block: Some(0),
//             eip158_block: Some(0),

//             homestead_block: Some(0),
//             byzantium_block: Some(0),
//             constantinople_block: Some(0),
//             petersburg_block: Some(0),
//             istanbul_block: Some(0),
//             muir_glacier_block: Some(0),
//             berlin_block: Some(0),
//             london_block: Some(0),
//             clique: Some(clique_config),
//             ..Default::default()
//         };

//         // fund account
//         let mut alloc = BTreeMap::default();
//         alloc.insert(
//             signer_addr,
//             GenesisAccount {
//                 balance: U256::MAX,
//                 nonce: None,
//                 code: None,
//                 storage: None,
//                 private_key: None,
//                 commitments: None,
//                 stake: None,
//                 last_tx: None,
//                 mining_permission: None,
//             },
//         );

//         // put signer address in the extra data, padded by the required amount of zeros
//         // Clique issue: https://github.com/ethereum/EIPs/issues/225
//         // Clique EIP: https://eips.ethereum.org/EIPS/eip-225
//         //
//         // The first 32 bytes are vanity data, so we will populate it with zeros
//         // This is followed by the signer address, which is 20 bytes
//         // There are 65 bytes of zeros after the signer address, which is usually populated with the
//         // proposer signature. Because the genesis does not have a proposer signature, it will be
//         // populated with zeros.
//         let extra_data_bytes = [&[0u8; 32][..], signer_addr.as_slice(), &[0u8; 65][..]].concat();
//         let extra_data = extra_data_bytes.into();

//         Genesis {
//             config,
//             alloc,
//             difficulty: U256::from(1),
//             gas_limit: 5_000_000,
//             extra_data,
//             ..Default::default()
//         }
//     }

//     /// Set the nonce.
//     pub const fn with_nonce(mut self, nonce: u64) -> Self {
//         self.nonce = nonce;
//         self
//     }

//     /// Set the timestamp.
//     pub const fn with_timestamp(mut self, timestamp: u64) -> Self {
//         self.timestamp = timestamp;
//         self
//     }

//     /// Set the extra data.
//     pub fn with_extra_data(mut self, extra_data: Bytes) -> Self {
//         self.extra_data = extra_data;
//         self
//     }

//     /// Set the gas limit.
//     pub const fn with_gas_limit(mut self, gas_limit: u64) -> Self {
//         self.gas_limit = gas_limit;
//         self
//     }

//     /// Set the difficulty.
//     pub const fn with_difficulty(mut self, difficulty: U256) -> Self {
//         self.difficulty = difficulty;
//         self
//     }

//     /// Set the mix hash of the header.
//     pub const fn with_mix_hash(mut self, mix_hash: B256) -> Self {
//         self.mix_hash = mix_hash;
//         self
//     }

//     /// Set the coinbase address.
//     pub const fn with_coinbase(mut self, address: Address) -> Self {
//         self.coinbase = address;
//         self
//     }

//     /// Set the base fee.
//     pub const fn with_base_fee(mut self, base_fee: Option<u128>) -> Self {
//         self.base_fee_per_gas = base_fee;
//         self
//     }

//     /// Set the excess blob gas.
//     pub const fn with_excess_blob_gas(mut self, excess_blob_gas: Option<u128>) -> Self {
//         self.excess_blob_gas = excess_blob_gas;
//         self
//     }

//     /// Set the blob gas used.
//     pub const fn with_blob_gas_used(mut self, blob_gas_used: Option<u128>) -> Self {
//         self.blob_gas_used = blob_gas_used;
//         self
//     }

//     pub fn with_shadows(mut self, shadows: Option<Shadows>) -> Self {
//         self.shadows = shadows;
//         self
//     }
//     /// Add accounts to the genesis block. If the address is already present,
//     /// the account is updated.
//     pub fn extend_accounts(
//         mut self,
//         accounts: impl IntoIterator<Item = (Address, GenesisAccount)>,
//     ) -> Self {
//         self.alloc.extend(accounts);
//         self
//     }
// }

// #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
// #[serde(deny_unknown_fields)]
// pub struct GenesisAccount {
//     /// The nonce of the account at genesis.
//     #[serde(
//         skip_serializing_if = "Option::is_none",
//         with = "alloy_serde::quantity::opt",
//         default
//     )]
//     pub nonce: Option<u64>,
//     /// The balance of the account at genesis.
//     pub balance: U256,
//     /// The account's bytecode at genesis.
//     #[serde(default, skip_serializing_if = "Option::is_none")]
//     pub code: Option<Bytes>,
//     /// The account's storage at genesis.
//     #[serde(
//         default,
//         skip_serializing_if = "Option::is_none",
//         deserialize_with = "deserialize_storage_map"
//     )]
//     pub storage: Option<BTreeMap<B256, B256>>,
//     /// The account's private key. Should only be used for testing.
//     #[serde(rename = "secretKey", default, skip_serializing_if = "Option::is_none")]
//     pub private_key: Option<B256>,
//     #[serde(default, skip_serializing_if = "Option::is_none")]
//     pub commitments: Option<Commitments>,
//     #[serde(default, skip_serializing_if = "Option::is_none")]
//     pub stake: Option<Stake>,
//     // default to "false"
//     #[serde(default, skip_serializing_if = "Option::is_none")]
//     pub mining_permission: Option<bool>,
//     #[serde(default, skip_serializing_if = "Option::is_none")]
//     pub last_tx: Option<LastTx>,
// }

// #[derive(Debug, Clone, PartialEq, Eq, Default, Compact, Serialize, Deserialize)]

// struct StorageEntries {
//     entries: Vec<StorageEntry>,
// }

// #[derive(Debug, Clone, PartialEq, Eq, Default, Compact, Serialize, Deserialize)]

// struct StorageEntry {
//     key: B256,
//     value: B256,
// }

// // compactable variant of GenesisAccount
// #[derive(Debug, Clone, PartialEq, Eq, Default, Compact, Serialize, Deserialize)]

// struct CompactGenesisAccount {
//     /// The nonce of the account at genesis.
//     nonce: Option<u64>,
//     /// The balance of the account at genesis.
//     balance: U256,
//     /// The account's bytecode at genesis.
//     code: Option<Bytes>,
//     /// The account's storage at genesis.
//     storage: Option<StorageEntries>,
//     /// The account's private key. Should only be used for testing.
//     private_key: Option<B256>,
//     stake: Option<Stake>,
//     commitments: Option<Commitments>,
//     last_tx: Option<LastTx>,
//     mining_permission: Option<bool>,
// }

// impl Compact for GenesisAccount {
//     fn to_compact<B>(&self, buf: &mut B) -> usize
//     where
//         B: bytes::BufMut + AsMut<[u8]>,
//     {
//         let account = CompactGenesisAccount {
//             nonce: self.nonce,
//             balance: self.balance,
//             code: self.code.clone(),
//             storage: self.storage.clone().map(|s| StorageEntries {
//                 entries: s
//                     .into_iter()
//                     .map(|(key, value)| StorageEntry { key, value })
//                     .collect(),
//             }),
//             stake: self.stake,
//             commitments: self.commitments.clone(),
//             private_key: self.private_key,
//             mining_permission: self.mining_permission,
//             last_tx: self.last_tx,
//         };
//         account.to_compact(buf)
//     }

//     fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
//         let (account, _) = CompactGenesisAccount::from_compact(buf, len);
//         let alloy_account = GenesisAccount {
//             nonce: account.nonce,
//             balance: account.balance,
//             code: account.code,
//             storage: account.storage.map(|s| {
//                 s.entries
//                     .into_iter()
//                     .map(|entry| (entry.key, entry.value))
//                     .collect()
//             }),
//             private_key: account.private_key,
//             stake: account.stake,
//             commitments: account.commitments,
//             mining_permission: account.mining_permission,
//             last_tx: account.last_tx,
//         };
//         (alloy_account, buf)
//     }
// }

// impl GenesisAccount {
//     /// Set the nonce.
//     pub const fn with_nonce(mut self, nonce: Option<u64>) -> Self {
//         self.nonce = nonce;
//         self
//     }

//     /// Set the balance.
//     pub const fn with_balance(mut self, balance: U256) -> Self {
//         self.balance = balance;
//         self
//     }

//     /// Set the code.
//     pub fn with_code(mut self, code: Option<Bytes>) -> Self {
//         self.code = code;
//         self
//     }

//     /// Set the storage.
//     pub fn with_storage(mut self, storage: Option<BTreeMap<B256, B256>>) -> Self {
//         self.storage = storage;
//         self
//     }
// }
