use crate::{ApiClient, IrysApiClient, Method};
use eyre::OptionExt as _;
pub use irys_api_server::routes::block::BlockParam;
use irys_api_server::routes::price::PriceInfo;
pub use irys_types::CombinedBlockHeader;
use irys_types::{
    Base64, ChunkFormat, DataLedger, DataRoot, DataTransaction, TxChunkOffset, UnpackedChunk, H256,
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
        // TODO: find a better version
        let data = match &tx.data {
            Some(d) => d,
            None => eyre::bail!("missing required tx data"),
        };

        for (idx, chunk) in tx.chunks.iter().enumerate() {
            let unpacked_chunk = UnpackedChunk {
                data_root: tx.header.data_root,
                data_size: tx.header.data_size,
                data_path: Base64(tx.proofs[idx].proof.clone()),
                bytes: Base64(data.0[chunk.min_byte_range..chunk.max_byte_range].to_vec()),
                tx_offset: TxChunkOffset::from(idx as u32),
            };
            let _response = self
                .make_request::<(), _>(peer, Method::POST, "/chunk", Some(&unpacked_chunk))
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
                tx_offset: TxChunkOffset::from(idx as u32),
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
            if self
                .make_request::<bool, _>(
                    peer,
                    Method::GET,
                    format!("/tx/{}/is_promoted", &tx_id).as_str(),
                    None::<&()>,
                )
                .await?
                .unwrap_or(false)
            {
                return Ok(());
            }
            sleep(Duration::from_secs(1)).await;
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
}
