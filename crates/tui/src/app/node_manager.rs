use crate::{
    api::client::ApiClient,
    api::models::*,
    app::state::{AppState, MenuSelection},
    db::db_queue::DatabaseWriter,
    types::NodeUrl,
};
use eyre::Result;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

pub struct NodeManager;

impl NodeManager {
    pub async fn add_node(
        url: NodeUrl,
        state: &mut AppState,
        api_client: &ApiClient,
        database_writer: &Option<DatabaseWriter>,
    ) -> Result<()> {
        if state.nodes.contains_key(&url) {
            return Ok(());
        }

        state.add_node(url.clone());

        let cancel_token = CancellationToken::new();

        let start_time = Instant::now();
        if let Some(node_state) = state.nodes.get_mut(&url) {
            match api_client.get_node_info_url(&url, &cancel_token).await {
                Ok(info) => {
                    if state.is_recording
                        && let Some(db_writer) = database_writer
                    {
                        let raw_json = serde_json::to_string(&info).unwrap_or_default();
                        if let Err(e) = db_writer.record_node_info(
                            url.as_str().to_string(),
                            info.clone(),
                            raw_json,
                        ) {
                            tracing::error!("Failed to record node info to database: {}", e);
                        }
                    }

                    node_state.metrics.info = Some(info);
                    node_state.is_reachable = true;
                    node_state.response_time_ms = Some(start_time.elapsed().as_millis() as u64);
                }
                Err(_) => {
                    node_state.is_reachable = false;
                    node_state.response_time_ms = None;
                }
            }

            if node_state.is_reachable {
                if let Some(ref info) = node_state.metrics.info
                    && let Ok(height) = info.height.parse::<u64>()
                {
                    node_state.metrics.chain_height = Some(ChainHeight { height });
                }

                if let Ok(peer_response) = api_client.get_peer_list_url(&url, &cancel_token).await {
                    node_state.peers = peer_response;
                    node_state.metrics.peer_count = node_state.peers.len();
                }
            }

            node_state.last_updated = chrono::Utc::now();

            if let Some(response_time) = node_state.response_time_ms {
                node_state.metrics.add_response_time(response_time);
            }
        }

        Ok(())
    }

    pub fn remove_node(url: &NodeUrl, state: &mut AppState) {
        // CRITICAL: Maintain focused_node_index consistency during removal to prevent
        // out-of-bounds panics in grid rendering.
        // Algorithm: If removing focused node, shift focus left (unless at 0, keep 0).
        // If removing node before focused one, decrement index.
        if matches!(state.current_menu, MenuSelection::Nodes) {
            let node_urls: Vec<NodeUrl> = state.nodes.keys().cloned().collect();
            if let Some(removed_index) = node_urls.iter().position(|u| u == url) {
                if removed_index < state.focused_node_index
                    || (removed_index == state.focused_node_index && state.focused_node_index > 0)
                {
                    state.focused_node_index -= 1;
                } else if state.focused_node_index >= state.nodes.len() - 1 {
                    state.focused_node_index = state.nodes.len().saturating_sub(2);
                }
            }
        }

        state.remove_node(url);
    }

    /// Validates that a string is a valid HTTP(S) URL for a node endpoint.
    /// Returns true if the URL can be parsed and uses http or https scheme.
    pub fn is_valid_url(url: &str) -> bool {
        url::Url::parse(url)
            .map(|u| u.scheme() == "http" || u.scheme() == "https")
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_nodes(count: usize) -> AppState {
        let mut state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        state.current_menu = MenuSelection::Nodes;
        for index in 0..count {
            state.add_node(NodeUrl::new(format!("http://localhost:{}", 2000 + index)).unwrap());
        }
        state
    }

    #[test]
    fn remove_node_before_focus_keeps_focus_on_same_node() {
        let mut state = state_with_nodes(4);
        let node_urls: Vec<NodeUrl> = state.nodes.keys().cloned().collect();
        state.focused_node_index = 2;
        let focused_url = node_urls[2].clone();

        NodeManager::remove_node(&node_urls[0], &mut state);

        let remaining: Vec<NodeUrl> = state.nodes.keys().cloned().collect();
        assert_eq!(remaining[state.focused_node_index], focused_url);
    }

    #[test]
    fn remove_last_focused_node_moves_focus_to_new_last() {
        let mut state = state_with_nodes(3);
        let node_urls: Vec<NodeUrl> = state.nodes.keys().cloned().collect();
        state.focused_node_index = 2;

        NodeManager::remove_node(&node_urls[2], &mut state);

        assert_eq!(state.focused_node_index, 1);
    }
}
