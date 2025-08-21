#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::types::{GossipError, GossipResult};
use backoff::{future::retry_notify, ExponentialBackoff};
use core::time::Duration;
use irys_domain::{PeerList, ScoreDecreaseReason, ScoreIncreaseReason};
use irys_types::{
    Address, GossipData, GossipDataRequest, GossipRequest, PeerAddress, PeerListItem,
};
use reqwest::Client;
use reqwest::StatusCode;
use serde::Serialize;
use std::sync::Arc;
use tracing::{error, warn};

#[derive(Debug, Clone, thiserror::Error)]
#[error("Gossip client error: {0}")]
pub struct GossipClientError(String);

#[derive(Debug, Clone)]
pub struct GossipClient {
    pub mining_address: Address,
    client: Arc<Client>,
}

// TODO: Remove this when PeerList is no longer an actix service
impl Default for GossipClient {
    fn default() -> Self {
        panic!("GossipClient must be initialized with a timeout and mining address. Default is implemented only to satisfy actix trait bounds.");
    }
}

impl GossipClient {
    #[must_use]
    pub fn new(timeout: Duration, mining_address: Address) -> Self {
        Self {
            mining_address,
            client: Arc::new(
                Client::builder()
                    .timeout(timeout)
                    .build()
                    .expect("Failed to create reqwest client"),
            ),
        }
    }

    pub fn create_backoff() -> ExponentialBackoff {
        Self::create_normal_backoff()
    }

    pub fn create_critical_backoff() -> ExponentialBackoff {
        ExponentialBackoff {
            initial_interval: Duration::from_millis(50),
            max_interval: Duration::from_secs(1),
            max_elapsed_time: Some(Duration::from_secs(30)),
            multiplier: 1.5,
            ..Default::default()
        }
    }

    pub fn create_normal_backoff() -> ExponentialBackoff {
        ExponentialBackoff {
            initial_interval: Duration::from_millis(100),
            max_interval: Duration::from_secs(2),
            max_elapsed_time: Some(Duration::from_secs(10)),
            ..Default::default()
        }
    }

    pub fn create_light_backoff() -> ExponentialBackoff {
        ExponentialBackoff {
            initial_interval: Duration::from_millis(200),
            max_interval: Duration::from_secs(5),
            max_elapsed_time: Some(Duration::from_secs(5)),
            multiplier: 2.5,
            ..Default::default()
        }
    }

    pub fn internal_client(&self) -> &Client {
        &self.client
    }

    /// Send data to a peer and update their score based on the result
    ///
    /// # Errors
    ///
    /// If the peer is offline or the request fails, an error is returned.
    async fn send_data_and_update_score_internal(
        &self,
        peer: (&Address, &PeerListItem),
        data: &GossipData,
        peer_list: &PeerList,
    ) -> GossipResult<()> {
        let peer_miner_address = peer.0;
        let peer = peer.1;

        let res = self.send_data(peer, data).await;
        Self::handle_score(peer_list, &res, peer_miner_address);
        res
    }

    /// Request a specific data to be gossiped. Returns true if the peer has the data,
    /// and false if it doesn't.
    pub async fn make_get_data_request_and_update_the_score(
        &self,
        peer: &(Address, PeerListItem),
        requested_data: GossipDataRequest,
        peer_list: &PeerList,
    ) -> GossipResult<bool> {
        let url = format!("http://{}/gossip/get_data", peer.1.address.gossip);
        let get_data_request = self.create_request(requested_data);
        let backoff = Self::create_backoff();

        let client = self.client.clone();

        let res = retry_notify(
            backoff,
            || async {
                let response = client
                    .post(&url)
                    .json(&get_data_request)
                    .send()
                    .await
                    .map_err(|error| {
                        warn!(
                            "Network error during get_data request to {}: {}",
                            url, error
                        );
                        backoff::Error::transient(GossipError::Network(error.to_string()))
                    })?;

                let response =
                    Self::classify_http(response, |e| GossipError::Network(e.to_string()))?;

                response.json::<bool>().await.map_err(|error| {
                    // Prefer Protocol here if you have it
                    warn!(
                        "JSON parsing error during get_data request to {}: {}",
                        url, error
                    );
                    backoff::Error::permanent(GossipError::Network(error.to_string()))
                })
            },
            |err, dur| {
                warn!(?dur, %err, "retrying get_data request after {:?}", dur);
            },
        )
        .await;

        Self::handle_score(peer_list, &res, &peer.0);
        res
    }

    pub async fn check_health(&self, peer: PeerAddress) -> Result<bool, GossipClientError> {
        let url = format!("http://{}/gossip/health", peer.gossip);
        let backoff = Self::create_backoff();
        let client = self.client.clone();

        retry_notify(
            backoff,
            || async {
                let res = client.get(&url).send().await.map_err(|error| {
                    warn!("Network error during health check to {}: {}", url, error);
                    backoff::Error::transient(GossipClientError(error.to_string()))
                })?;

                Self::classify_http(res, |e| GossipClientError(e.to_string()))?
                    .json::<bool>()
                    .await
                    .map_err(|error| {
                        warn!(
                            "JSON parsing error during health check to {}: {}",
                            url, error
                        );
                        backoff::Error::permanent(GossipClientError(error.to_string()))
                    })
            },
            |err, dur| {
                warn!(?dur, %err, "retrying health check after {:?}", dur);
            },
        )
        .await
    }

    /// Send data to a peer
    ///
    /// # Errors
    ///
    /// If the peer is offline or the request fails, an error is returned.
    async fn send_data(&self, peer: &PeerListItem, data: &GossipData) -> GossipResult<()> {
        Self::check_if_peer_is_online(peer)?;
        match data {
            GossipData::Chunk(unpacked_chunk) => {
                let url = format!("http://{}/gossip/chunk", peer.address.gossip);
                self.send_data_internal(&url, unpacked_chunk, Self::create_critical_backoff())
                    .await?;
            }
            GossipData::Transaction(irys_transaction_header) => {
                let url = format!("http://{}/gossip/transaction", peer.address.gossip);
                self.send_data_internal(
                    &url,
                    irys_transaction_header,
                    Self::create_critical_backoff(),
                )
                .await?;
            }
            GossipData::CommitmentTransaction(commitment_tx) => {
                let url = format!("http://{}/gossip/commitment_tx", peer.address.gossip);
                self.send_data_internal(&url, commitment_tx, Self::create_critical_backoff())
                    .await?;
            }
            GossipData::Block(irys_block_header) => {
                let url = format!("http://{}/gossip/block", peer.address.gossip);
                self.send_data_internal(&url, &irys_block_header, Self::create_normal_backoff())
                    .await?;
            }
            GossipData::ExecutionPayload(execution_payload) => {
                let url = format!("http://{}/gossip/execution_payload", peer.address.gossip);
                self.send_data_internal(&url, &execution_payload, Self::create_normal_backoff())
                    .await?;
            }
            GossipData::IngressProof(ingress_proof) => {
                let url = format!("http://{}/gossip/ingress_proof", peer.address.gossip);
                self.send_data_internal(&url, &ingress_proof, Self::create_normal_backoff())
                    .await?;
            }
        };

        Ok(())
    }

    fn check_if_peer_is_online(peer: &PeerListItem) -> GossipResult<()> {
        if !peer.is_online {
            return Err(GossipError::InvalidPeer("Peer is offline".into()));
        }
        Ok(())
    }
    fn classify_http<E>(
        res: reqwest::Response,
        map: impl FnOnce(reqwest::Error) -> E,
    ) -> Result<reqwest::Response, backoff::Error<E>> {
        match res.error_for_status_ref() {
            Ok(_) => Ok(res),
            Err(e) => {
                let s = res.status();
                if s.is_client_error() && s != StatusCode::TOO_MANY_REQUESTS {
                    Err(backoff::Error::permanent(map(e)))
                } else {
                    Err(backoff::Error::transient(map(e)))
                }
            }
        }
    }
    async fn send_data_internal<T: Serialize + ?Sized>(
        &self,
        url: &str,
        data: &T,
        backoff_strategy: ExponentialBackoff,
    ) -> Result<(), GossipError> {
        let req = self.create_request(data);
        let client = self.client.clone();

        retry_notify(
            backoff_strategy,
            || async {
                let res = client.post(url).json(&req).send().await.map_err(|error| {
                    warn!("Network error during send_data to {}: {}", url, error);
                    backoff::Error::transient(GossipError::Network(error.to_string()))
                })?;

                // HTTP status -> transient/permanent
                Self::classify_http(res, |e| GossipError::Network(e.to_string()))?;
                Ok(())
            },
            |err, dur| {
                warn!(?dur, %err, "retrying send_data after {:?}", dur);
            },
        )
        .await
    }

    fn handle_score<T>(
        peer_list: &PeerList,
        result: &GossipResult<T>,
        peer_miner_address: &Address,
    ) {
        match &result {
            Ok(_) => {
                // Successful send, increase score
                peer_list.increase_peer_score(peer_miner_address, ScoreIncreaseReason::Online);
            }
            Err(_) => {
                // Failed to send, decrease score
                peer_list.decrease_peer_score(peer_miner_address, ScoreDecreaseReason::Offline);
            }
        }
    }

    /// Sends data to a peer and update their score in a detached task
    pub fn send_data_and_update_the_score_detached(
        &self,
        peer: (&Address, &PeerListItem),
        data: Arc<GossipData>,
        peer_list: &PeerList,
    ) {
        let client = self.clone();
        let peer_list = peer_list.clone();
        let peer_miner_address = *peer.0;
        let peer = peer.1.clone();

        tokio::spawn(async move {
            if let Err(e) = client
                .send_data_and_update_score_internal(
                    (&peer_miner_address, &peer),
                    &data,
                    &peer_list,
                )
                .await
            {
                error!("Error sending data to peer: {}", e);
            }
        });
    }

    fn create_request<T>(&self, data: T) -> GossipRequest<T> {
        GossipRequest {
            miner_address: self.mining_address,
            data,
        }
    }
}
