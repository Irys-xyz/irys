mod block_reward_system_tx_validatoin;
mod stake_tx_validation;

// todo test cases:
// - [ ] spawn a normal evm chain, gossip with an irys node, try to force propagate a block that has an invalid state root, expect the block to be rejected in validatoin
