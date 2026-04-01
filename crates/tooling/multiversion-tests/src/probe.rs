use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("timeout waiting for {condition} after {elapsed:?}")]
    Timeout {
        condition: String,
        elapsed: Duration,
    },
    #[error("unexpected response: {0}")]
    Unexpected(String),
}

#[derive(Debug, Deserialize)]
pub struct NodeInfo {
    #[serde(deserialize_with = "string_or_u64")]
    pub height: u64,
    /// The hash of the block at this node's current tip.
    #[serde(default)]
    pub block_hash: Option<String>,
    pub version: Option<String>,
    #[serde(default)]
    pub peer_count: Option<u64>,
    #[serde(default)]
    pub is_syncing: Option<bool>,
}

#[derive(Debug)]
pub struct PeerInfo {
    pub address: String,
}

fn string_or_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrU64 {
        Str(String),
        Num(u64),
    }
    match StringOrU64::deserialize(deserializer)? {
        StringOrU64::Str(s) => s.parse::<u64>().map_err(serde::de::Error::custom),
        StringOrU64::Num(n) => Ok(n),
    }
}

const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
const READY_POLL_INTERVAL: Duration = Duration::from_secs(2);
const HEIGHT_POLL_INTERVAL: Duration = Duration::from_secs(3);
const CONVERGENCE_POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct HttpProbe {
    client: Client,
}

impl HttpProbe {
    pub fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: Client::builder().timeout(CLIENT_TIMEOUT).build()?,
        })
    }

    pub async fn get_info(&self, api_url: &str) -> Result<NodeInfo, ProbeError> {
        let url = crate::api_endpoint(api_url, "v1/info");
        let resp = self.client.get(url).send().await?.error_for_status()?;
        Ok(resp.json::<NodeInfo>().await?)
    }

    pub async fn get_peers(&self, api_url: &str) -> Result<Vec<PeerInfo>, ProbeError> {
        let url = crate::api_endpoint(api_url, "v1/peer-list");
        let resp = self.client.get(url).send().await?.error_for_status()?;
        // The /v1/peer-list route returns Vec<PeerAddress> objects with gossip, api,
        // and execution fields. Extract the api address as the peer address string.
        let peers: Vec<serde_json::Value> = resp.json().await?;
        Ok(peers
            .into_iter()
            .filter_map(|v| {
                v.get("api")?.as_str().map(|s| PeerInfo {
                    address: s.to_owned(),
                })
            })
            .collect())
    }

    pub async fn get_network_config(&self, api_url: &str) -> Result<serde_json::Value, ProbeError> {
        let url = crate::api_endpoint(api_url, "v1/network/config");
        let resp = self.client.get(url).send().await?.error_for_status()?;
        Ok(resp.json::<serde_json::Value>().await?)
    }

    pub async fn get_genesis_hash(&self, api_url: &str) -> Result<String, ProbeError> {
        let url = crate::api_endpoint(api_url, "v1/genesis");
        let resp = self.client.get(url).send().await?.error_for_status()?;
        let body: serde_json::Value = resp.json().await?;
        body["genesis_block_hash"]
            .as_str()
            .map(std::borrow::ToOwned::to_owned)
            .ok_or_else(|| {
                ProbeError::Unexpected("missing genesis_block_hash in /v1/genesis".to_owned())
            })
    }

    pub async fn wait_for_ready(
        &self,
        api_url: &str,
        timeout: Duration,
    ) -> Result<NodeInfo, ProbeError> {
        let condition = format!("{api_url} ready");
        poll_until(READY_POLL_INTERVAL, timeout, &condition, || async {
            self.get_info(api_url).await.ok()
        })
        .await
    }

    pub async fn wait_for_height(
        &self,
        api_url: &str,
        target_height: u64,
        timeout: Duration,
    ) -> Result<NodeInfo, ProbeError> {
        let condition = format!("{api_url} height >= {target_height}");
        poll_until(HEIGHT_POLL_INTERVAL, timeout, &condition, || async {
            self.get_info(api_url)
                .await
                .ok()
                .filter(|info| info.height >= target_height)
        })
        .await
    }

    pub async fn wait_for_convergence(
        &self,
        api_urls: &[String],
        tolerance: u64,
        timeout: Duration,
    ) -> Result<Vec<NodeInfo>, ProbeError> {
        let condition = format!("convergence within {tolerance} blocks");
        poll_until(CONVERGENCE_POLL_INTERVAL, timeout, &condition, || async {
            self.poll_all_nodes(api_urls)
                .await
                .filter(|infos| is_converged(infos, tolerance))
        })
        .await
    }

    async fn poll_all_nodes(&self, api_urls: &[String]) -> Option<Vec<NodeInfo>> {
        let futs: Vec<_> = api_urls.iter().map(|url| self.get_info(url)).collect();
        futures::future::join_all(futs)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .ok()
    }
}

async fn poll_until<T, F, Fut>(
    interval: Duration,
    timeout: Duration,
    condition_name: &str,
    mut check: F,
) -> Result<T, ProbeError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let start = tokio::time::Instant::now();
    loop {
        if let Some(result) = check().await {
            return Ok(result);
        }
        if start.elapsed() > timeout {
            return Err(ProbeError::Timeout {
                condition: condition_name.to_owned(),
                elapsed: timeout,
            });
        }
        tokio::time::sleep(interval).await;
    }
}

fn is_converged(infos: &[NodeInfo], tolerance: u64) -> bool {
    if infos.is_empty() {
        return false;
    }
    // A node that is still syncing is not converged.
    if infos.iter().any(|n| n.is_syncing == Some(true)) {
        return false;
    }
    let max = infos.iter().map(|i| i.height).max().unwrap_or(0);
    let min = infos.iter().map(|i| i.height).min().unwrap_or(0);
    if max.saturating_sub(min) > tolerance || min == 0 {
        return false;
    }
    // Nodes at the same height must agree on the block hash.
    for (i, a) in infos.iter().enumerate() {
        for b in &infos[i + 1..] {
            if a.height == b.height
                && let (Some(ha), Some(hb)) = (&a.block_hash, &b.block_hash)
                && ha != hb
            {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    proptest! {
        #[test]
        fn single_node_above_zero_is_converged(height in 1_u64..1000) {
            let infos = vec![NodeInfo { height, block_hash: None, version: None, peer_count: None, is_syncing: None }];
            prop_assert!(is_converged(&infos, 0));
        }

        #[test]
        fn identical_heights_above_zero_are_converged(
            height in 1_u64..1000,
            count in 2_usize..10,
            tolerance in 0_u64..5,
        ) {
            let infos: Vec<NodeInfo> = (0..count)
                .map(|_| NodeInfo { height, block_hash: None, version: None, peer_count: None, is_syncing: None })
                .collect();
            prop_assert!(is_converged(&infos, tolerance));
        }

        #[test]
        fn spread_exceeding_tolerance_is_not_converged(
            min_height in 1_u64..500,
            extra in 1_u64..100,
            tolerance in 0_u64..10,
        ) {
            let max_height = min_height + tolerance + extra;
            let infos = vec![
                NodeInfo { height: min_height, block_hash: None, version: None, peer_count: None, is_syncing: None },
                NodeInfo { height: max_height, block_hash: None, version: None, peer_count: None, is_syncing: None },
            ];
            prop_assert!(!is_converged(&infos, tolerance));
        }

        #[test]
        fn zero_min_is_not_converged(
            max_height in 0_u64..100,
            tolerance in 0_u64..200,
        ) {
            let infos = vec![
                NodeInfo { height: 0, block_hash: None, version: None, peer_count: None, is_syncing: None },
                NodeInfo { height: max_height, block_hash: None, version: None, peer_count: None, is_syncing: None },
            ];
            prop_assert!(!is_converged(&infos, tolerance));
        }

        #[test]
        fn within_tolerance_is_converged(
            base in 1_u64..500,
            tolerance in 1_u64..10,
            offset in 0_u64..10,
        ) {
            let actual_offset = offset % (tolerance + 1);
            let infos = vec![
                NodeInfo { height: base, block_hash: None, version: None, peer_count: None, is_syncing: None },
                NodeInfo { height: base + actual_offset, block_hash: None, version: None, peer_count: None, is_syncing: None },
            ];
            prop_assert!(is_converged(&infos, tolerance));
        }
    }

    #[test]
    fn empty_is_not_converged() {
        assert!(!is_converged(&[], 0));
    }

    #[test]
    fn syncing_node_is_not_converged() {
        let infos = vec![
            NodeInfo {
                height: 5,
                block_hash: Some("0xabc".to_owned()),
                version: None,
                peer_count: None,
                is_syncing: Some(true),
            },
            NodeInfo {
                height: 5,
                block_hash: Some("0xabc".to_owned()),
                version: None,
                peer_count: None,
                is_syncing: Some(false),
            },
        ];
        assert!(!is_converged(&infos, 0));
    }

    #[test]
    fn not_syncing_nodes_are_converged() {
        let infos = vec![
            NodeInfo {
                height: 5,
                block_hash: Some("0xabc".to_owned()),
                version: None,
                peer_count: None,
                is_syncing: Some(false),
            },
            NodeInfo {
                height: 5,
                block_hash: Some("0xabc".to_owned()),
                version: None,
                peer_count: None,
                is_syncing: Some(false),
            },
        ];
        assert!(is_converged(&infos, 0));
    }

    #[test]
    fn same_height_matching_hashes_is_converged() {
        let hash = Some("0xabc".to_owned());
        let infos = vec![
            NodeInfo {
                height: 5,
                block_hash: hash.clone(),
                version: None,
                peer_count: None,
                is_syncing: None,
            },
            NodeInfo {
                height: 5,
                block_hash: hash,
                version: None,
                peer_count: None,
                is_syncing: None,
            },
        ];
        assert!(is_converged(&infos, 0));
    }

    #[test]
    fn same_height_mismatched_hashes_is_not_converged() {
        let infos = vec![
            NodeInfo {
                height: 5,
                block_hash: Some("0xabc".to_owned()),
                version: None,
                peer_count: None,
                is_syncing: None,
            },
            NodeInfo {
                height: 5,
                block_hash: Some("0xdef".to_owned()),
                version: None,
                peer_count: None,
                is_syncing: None,
            },
        ];
        assert!(!is_converged(&infos, 0));
    }

    #[test]
    fn different_heights_within_tolerance_mismatched_hashes_is_converged() {
        // Different heights — hash comparison doesn't apply.
        let infos = vec![
            NodeInfo {
                height: 5,
                block_hash: Some("0xabc".to_owned()),
                version: None,
                peer_count: None,
                is_syncing: None,
            },
            NodeInfo {
                height: 6,
                block_hash: Some("0xdef".to_owned()),
                version: None,
                peer_count: None,
                is_syncing: None,
            },
        ];
        assert!(is_converged(&infos, 1));
    }

    #[tokio::test]
    async fn poll_until_succeeds_immediately() {
        let result = poll_until(
            Duration::from_millis(10),
            Duration::from_secs(1),
            "immediate",
            || async { Some(42) },
        )
        .await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn poll_until_times_out() {
        let result: Result<i32, ProbeError> = poll_until(
            Duration::from_millis(10),
            Duration::from_millis(50),
            "never-ready",
            || async { None },
        )
        .await;
        assert!(matches!(result, Err(ProbeError::Timeout { .. })));
    }

    #[tokio::test]
    async fn poll_until_retries_then_succeeds() {
        let counter = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&counter); // clone: shared ownership across async iterations
        let result = poll_until(
            Duration::from_millis(10),
            Duration::from_secs(2),
            "retry-test",
            move || {
                let count = c.fetch_add(1, Ordering::Relaxed);
                async move { if count >= 3 { Some("done") } else { None } }
            },
        )
        .await;
        assert_eq!(result.unwrap(), "done");
        assert!(counter.load(Ordering::Relaxed) >= 4);
    }

    #[tokio::test]
    async fn poll_until_timeout_reports_condition_name() {
        let result: Result<(), ProbeError> = poll_until(
            Duration::from_millis(5),
            Duration::from_millis(20),
            "my-condition",
            || async { None },
        )
        .await;
        match result {
            Err(ProbeError::Timeout { condition, .. }) => {
                assert_eq!(condition, "my-condition");
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }
}
