use crate::{ApiClient, IrysApiClient, Method};
use eyre::OptionExt as _;
pub use irys_api_server::routes::block::BlockParam;
use irys_api_server::routes::{anchor::AnchorResponse, price::PriceInfo, tx::PromotionStatus};
pub use irys_types::CombinedBlockHeader;
use irys_types::{
    Base64, BlockHash, ChunkFormat, DataLedger, DataRoot, DataTransaction, TxChunkOffset,
    UnpackedChunk, H256,
};
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};
use tokio::time::sleep;
use tracing::debug;

/// Trait defining the interface for the extended API client
#[async_trait::async_trait]
pub trait ApiClientExt: ApiClient {
    async fn get_block_by_param(
        &self,
        peer: SocketAddr,
        param: BlockParam,
    ) -> eyre::Result<CombinedBlockHeader>;

    async fn upload_chunks(&self, peer: SocketAddr, tx: &DataTransaction) -> eyre::Result<()>;

    async fn upload_chunks_iter(
        &self,
        peer: SocketAddr,
        tx: &DataTransaction,
        mut data: impl Iterator<Item = eyre::Result<Vec<u8>>> + Send,
    ) -> eyre::Result<()>;

    async fn wait_for_promotion(
        &self,
        peer: SocketAddr,
        tx_id: H256,
        attempts: u64,
    ) -> eyre::Result<()>;

    async fn get_data_price(
        &self,
        peer: SocketAddr,
        ledger: DataLedger,
        data_size: u64,
    ) -> eyre::Result<PriceInfo>;

    async fn get_chunk(
        &self,
        peer: SocketAddr,
        ledger: DataLedger,
        data_root: DataRoot,
        offset: u32, // data root relative offset
    ) -> eyre::Result<ChunkFormat>;

    async fn get_anchor(&self, peer: SocketAddr) -> eyre::Result<BlockHash>;
}

#[async_trait::async_trait]
impl ApiClientExt for IrysApiClient {
    async fn get_block_by_param(
        &self,
        peer: SocketAddr,
        param: BlockParam,
    ) -> eyre::Result<CombinedBlockHeader> {
        let path = format!("/block/{}", param);
        let r = self
            .make_request::<CombinedBlockHeader, _>(peer, Method::GET, &path, None::<&()>)
            .await?;
        r.ok_or_eyre("unable to get block header")
    }

    async fn upload_chunks(&self, peer: SocketAddr, tx: &DataTransaction) -> eyre::Result<()> {
        for chunk in tx.data_chunks()? {
            let _response = self
                .make_request::<(), _>(peer, Method::POST, "/chunk", Some(&chunk))
                .await?;
        }

        Ok(())
    }

    async fn upload_chunks_iter(
        &self,
        peer: SocketAddr,
        tx: &DataTransaction,
        data: impl Iterator<Item = eyre::Result<Vec<u8>>> + Send,
    ) -> eyre::Result<()> {
        for (idx, data) in data.enumerate() {
            let data = data?;
            let proof = &tx.proofs[idx];
            let unpacked_chunk = UnpackedChunk {
                data_root: tx.header.data_root,
                data_size: tx.header.data_size,
                data_path: Base64(proof.proof.clone()),
                bytes: Base64(data),
                tx_offset: TxChunkOffset::from(std::convert::TryInto::<u32>::try_into(idx)?),
            };
            let now = Instant::now();
            let _response = self
                .make_request::<(), _>(peer, Method::POST, "/chunk", Some(&unpacked_chunk))
                .await?;
            debug!("Uploading chunk {} took {:.3?}", idx, now.elapsed());
        }

        Ok(())
    }

    async fn wait_for_promotion(
        &self,
        peer: SocketAddr,
        tx_id: H256,
        attempts: u64,
    ) -> eyre::Result<()> {
        for _i in 0..attempts {
            let status = self
                .make_request::<PromotionStatus, _>(
                    peer,
                    Method::GET,
                    format!("/tx/{}/promotion_status", &tx_id).as_str(),
                    None::<&()>,
                )
                .await?;
            if status.map(|s| s.is_promoted).unwrap_or(false) {
                return Ok(());
            }
            sleep(Duration::from_millis(200)).await;
        }

        Err(eyre::eyre!(
            "Tx {} did not promote after {} attempts",
            &tx_id,
            &attempts
        ))
    }

    async fn get_data_price(
        &self,
        peer: SocketAddr,
        ledger: DataLedger,
        data_size: u64,
    ) -> eyre::Result<PriceInfo> {
        let response = self
            .make_request(
                peer,
                Method::GET,
                format!("/price/{}/{}", ledger as u32, data_size).as_str(),
                None::<&()>,
            )
            .await?;
        response.ok_or_eyre("unable to get price info")
    }

    async fn get_chunk(
        &self,
        peer: SocketAddr,
        ledger_id: DataLedger,
        data_root: DataRoot,
        offset: u32, // data root relative offset
    ) -> eyre::Result<ChunkFormat> {
        self.make_request(
            peer,
            Method::GET,
            format!("/chunk/data_root/{}/{data_root}/{offset}", ledger_id as u32).as_str(),
            None::<&()>,
        )
        .await?
        .ok_or_eyre("Unable to fetch chunk")
    }

    async fn get_anchor(&self, peer: SocketAddr) -> eyre::Result<BlockHash> {
        Ok(self
            .make_request::<AnchorResponse, _>(peer, Method::GET, "/anchor", None::<&()>)
            .await?
            .ok_or_eyre("Unable to fetch anchor")?
            .block_hash)
    }
}
