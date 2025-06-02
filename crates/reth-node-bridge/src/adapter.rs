use reth_e2e_test_utils::node::NodeTestContext;

use crate::node::{eth_payload_attributes, RethNode, RethNodeAdapter, RethNodeAddOns};

pub type RethContext = NodeTestContext<RethNodeAdapter, RethNodeAddOns>;

pub async fn new_reth_context(node: RethNode) -> eyre::Result<RethContext> {
    NodeTestContext::new(node, eth_payload_attributes).await
}
