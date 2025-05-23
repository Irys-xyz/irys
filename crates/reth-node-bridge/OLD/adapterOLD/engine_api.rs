use crate::adapter::traits::PayloadEnvelopeExt;
use alloy_primitives::B256;
use jsonrpsee::{
    core::client::ClientT,
    http_client::{transport::HttpBackend, HttpClient},
};
use reth::{
    api::EngineTypes,
    providers::CanonStateNotificationStream,
    rpc::{
        api::EngineApiClient,
        types::engine::{ForkchoiceState, PayloadStatusEnum},
    },
};
use reth_payload_builder::PayloadId;
use reth_rpc_layer::AuthClientService;
use std::marker::PhantomData;

/// Helper for engine api operations
#[derive(Debug)]
pub struct EngineApiContext<E> {
    pub canonical_stream: CanonStateNotificationStream,
    pub engine_api_client: HttpClient<AuthClientService<HttpBackend>>,
    pub _marker: PhantomData<E>,
}

impl<E: EngineTypes> EngineApiContext<E> {
    /// Retrieves a v1 Irys payload from the engine api
    pub async fn get_payload_v1_irys(
        &self,
        payload_id: PayloadId,
    ) -> eyre::Result<E::ExecutionPayloadV1Irys> {
        Ok(EngineApiClient::<E>::get_payload_v1_irys(&self.engine_api_client, payload_id).await?)
    }

    pub async fn build_payload_v1_irys(
        &self,
        parent: B256,
        payload_attributes: E::PayloadAttributes,
    ) -> eyre::Result<E::ExecutionPayloadV1Irys> {
        Ok(EngineApiClient::<E>::build_new_payload_irys(
            &self.engine_api_client,
            parent,
            payload_attributes,
        )
        .await?)
    }

    /// Retrieves a v1 Irys payload from the engine api as serde value
    pub async fn get_payload_v1_irys_value(
        &self,
        payload_id: PayloadId,
    ) -> eyre::Result<serde_json::Value> {
        Ok(self
            .engine_api_client
            .request("engine_getPayloadV1Irys", (payload_id,))
            .await?)
    }

    /// Submits a payload to the engine api
    pub async fn submit_payload(
        &self,
        payload: E::BuiltPayload,
        _payload_builder_attributes: E::PayloadBuilderAttributes,
        expected_status: PayloadStatusEnum,
        _versioned_hashes: Vec<B256>,
    ) -> eyre::Result<B256>
    where
        E::ExecutionPayloadV1Irys: From<E::BuiltPayload> + PayloadEnvelopeExt,
    {
        // setup payload for submission
        let envelope_v3: <E as EngineTypes>::ExecutionPayloadV1Irys = payload.into();

        // submit payload to engine api
        let submission = EngineApiClient::<E>::submit_new_payload_irys(
            &self.engine_api_client,
            envelope_v3.execution_payload(),
            // versioned_hashes,
            // payload_builder_attributes.parent_beacon_block_root().unwrap(),
        )
        .await?;

        assert_eq!(submission.status, expected_status);

        Ok(submission.latest_valid_hash.unwrap_or_default())
    }

    /// Sends forkchoice update to the engine api
    pub async fn update_forkchoice(&self, current_head: B256, new_head: B256) -> eyre::Result<()> {
        EngineApiClient::<E>::fork_choice_updated_v1_irys(
            &self.engine_api_client,
            ForkchoiceState {
                head_block_hash: new_head,
                safe_block_hash: current_head,
                finalized_block_hash: current_head,
            },
            None,
        )
        .await?;
        Ok(())
    }

    /// Sends forkchoice update to the engine api
    // we can set safe or finalized to ZERO to skip updating them, but head is mandatory.
    // safe (confirmed) we update in the block confirmed handler
    // finalized we update in the block finalized handler
    pub async fn update_forkchoice_full(
        &self,
        head_block_hash: B256,
        confirmed_block_hash: Option<B256>,
        finalized_block_hash: Option<B256>,
    ) -> eyre::Result<()> {
        EngineApiClient::<E>::fork_choice_updated_v1_irys(
            &self.engine_api_client,
            ForkchoiceState {
                head_block_hash,
                safe_block_hash: confirmed_block_hash.unwrap_or(B256::ZERO),
                finalized_block_hash: finalized_block_hash.unwrap_or(B256::ZERO),
            },
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn update_forkchoice_payload_attr(
        &self,
        current_head: B256,
        new_head: B256,
        payload_attributes: Option<E::PayloadAttributes>,
    ) -> eyre::Result<()> {
        EngineApiClient::<E>::fork_choice_updated_v1_irys(
            &self.engine_api_client,
            ForkchoiceState {
                head_block_hash: new_head,
                safe_block_hash: current_head,
                finalized_block_hash: current_head,
            },
            payload_attributes,
        )
        .await?;
        Ok(())
    }

    /// Sends forkchoice update to the engine api with a zero finalized hash
    pub async fn update_optimistic_forkchoice(&self, hash: B256) -> eyre::Result<()> {
        EngineApiClient::<E>::fork_choice_updated_v1_irys(
            &self.engine_api_client,
            ForkchoiceState {
                head_block_hash: hash,
                safe_block_hash: B256::ZERO,
                finalized_block_hash: B256::ZERO,
            },
            None,
        )
        .await?;

        Ok(())
    }

    // pub async fn add_shadows(&self, block_hash: B256, shadows: Shadows) -> eyre::Result<()> {
    //     EngineApiClient::<E>::add_shadows_v1(&self.engine_api_client, block_hash, shadows).await?;
    //     Ok(())
    // }
}
