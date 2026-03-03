//! Irys engine validators.
//!
//! This module contains the Engine API validators for Irys that wrap
//! the standard Ethereum execution payload validators.

use std::sync::Arc;

use alloy_rpc_types_engine::ExecutionData;
use reth::api::FullNodeComponents;
use reth_chainspec::ChainSpec;
use reth_ethereum_engine_primitives::EthPayloadAttributes;
use reth_ethereum_payload_builder::EthereumExecutionPayloadValidator;
use reth_ethereum_primitives::Block;
use reth_node_api::{
    EngineApiValidator, NewPayloadError, PayloadValidator,
    node::AddOnsContext,
    payload::{EngineApiMessageVersion, EngineObjectValidationError, PayloadOrAttributes},
    validate_version_specific_fields,
};
use reth_node_builder::rpc::PayloadValidatorBuilder;
use reth_node_ethereum::EthEngineTypes;
use reth_primitives_traits::SealedBlock;

use crate::IrysEthereumNode;
use crate::engine::{IrysPayloadAttributes, IrysPayloadTypes};

/// Custom engine validator for Irys that wraps the Ethereum execution payload validator.
#[derive(Debug, Clone)]
pub struct IrysEngineValidator {
    inner: EthereumExecutionPayloadValidator<ChainSpec>,
}

impl IrysEngineValidator {
    /// Instantiates a new validator.
    pub const fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self {
            inner: EthereumExecutionPayloadValidator::new(chain_spec),
        }
    }

    /// Returns the chain spec used by the validator.
    #[inline]
    fn chain_spec(&self) -> &ChainSpec {
        self.inner.chain_spec()
    }
}

impl PayloadValidator<EthEngineTypes<IrysPayloadTypes>> for IrysEngineValidator {
    type Block = Block;

    fn convert_payload_to_block(
        &self,
        payload: ExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        self.inner
            .ensure_well_formed_payload(payload)
            .map_err(Into::into)
    }
}

impl EngineApiValidator<EthEngineTypes<IrysPayloadTypes>> for IrysEngineValidator {
    fn validate_version_specific_fields(
        &self,
        version: EngineApiMessageVersion,
        payload_or_attrs: PayloadOrAttributes<'_, ExecutionData, IrysPayloadAttributes>,
    ) -> Result<(), EngineObjectValidationError> {
        // Convert IrysPayloadAttributes to EthPayloadAttributes for validation
        match payload_or_attrs {
            PayloadOrAttributes::ExecutionPayload(payload) => validate_version_specific_fields(
                self.chain_spec(),
                version,
                PayloadOrAttributes::<ExecutionData, EthPayloadAttributes>::ExecutionPayload(
                    payload,
                ),
            ),
            PayloadOrAttributes::PayloadAttributes(attrs) => validate_version_specific_fields(
                self.chain_spec(),
                version,
                PayloadOrAttributes::<ExecutionData, EthPayloadAttributes>::PayloadAttributes(
                    &attrs.inner,
                ),
            ),
        }
    }

    fn ensure_well_formed_attributes(
        &self,
        version: EngineApiMessageVersion,
        attributes: &IrysPayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        validate_version_specific_fields(
            self.chain_spec(),
            version,
            PayloadOrAttributes::<ExecutionData, EthPayloadAttributes>::PayloadAttributes(
                &attributes.inner,
            ),
        )
    }
}

/// Custom engine validator builder for Irys
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct IrysEngineValidatorBuilder;

impl<N> PayloadValidatorBuilder<N> for IrysEngineValidatorBuilder
where
    N: FullNodeComponents<Types = IrysEthereumNode>,
{
    type Validator = IrysEngineValidator;

    async fn build(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::Validator> {
        Ok(IrysEngineValidator::new(ctx.config.chain.clone()))
    }
}
