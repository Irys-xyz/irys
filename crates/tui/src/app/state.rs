use crate::{
    api::models::{
        BlockTreeForksResponse, MempoolStatus, MiningInfo, NodeConfig, NodeMetrics, PeerInfo,
    },
    types::NodeUrl,
};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;

const SPLASH_TIMEOUT_SECS: u64 = 3;

#[derive(Debug, Clone)]
pub enum AppScreen {
    Splash,
    Main,
}

#[derive(Debug, Clone)]
pub enum MenuSelection {
    Nodes,
    DataSync,
    Mempool,
    Mining,
    Forks,
    Metrics,
    Config,
    Logs,
    Settings,
}

#[derive(Debug, Clone)]
pub struct NodeState {
    pub url: NodeUrl,
    pub alias: Option<Arc<str>>,
    pub metrics: NodeMetrics,
    pub peers: Vec<PeerInfo>,
    pub mempool_status: Option<MempoolStatus>,
    pub mining_info: Option<MiningInfo>,
    pub fork_info: Option<BlockTreeForksResponse>,
    pub config: Option<NodeConfig>,
    pub last_updated: DateTime<Utc>,
    pub is_reachable: bool,
    pub response_time_ms: Option<u64>,
    /// Scroll offset for this node's display in the UI
    pub scroll_offset: u16,
    /// Total content height for scrolling
    pub content_height: u16,
}

impl NodeState {
    pub fn new(url: NodeUrl) -> Self {
        Self {
            url,
            alias: None,
            metrics: NodeMetrics::new(),
            peers: Vec::new(),
            mempool_status: None,
            mining_info: None,
            fork_info: None,
            config: None,
            last_updated: Utc::now(),
            is_reachable: false,
            response_time_ms: None,
            scroll_offset: 0,
            content_height: 0,
        }
    }

    pub fn display_name(&self) -> String {
        if let Some(alias) = &self.alias {
            alias.to_string()
        } else {
            // Extract hostname:port from URL for cleaner display
            self.url
                .as_str()
                .replace("http://", "")
                .replace("https://", "")
                .split('/')
                .next()
                .unwrap_or(self.url.as_str())
                .to_string()
        }
    }

    pub fn set_alias(&mut self, alias: Option<String>) {
        self.alias = alias.map(|s| Arc::from(s.as_str()));
    }
}

#[derive(Debug)]
pub struct AppState {
    /// Current screen being displayed (splash or main)
    pub screen: AppScreen,
    /// Start time for splash screen animation timing
    pub splash_start_time: Option<std::time::Instant>,
    /// URL of the primary/initial node
    pub primary_node_url: NodeUrl,
    /// Currently selected menu in the main interface
    pub current_menu: MenuSelection,
    /// Map of all monitored nodes keyed by their URLs
    pub nodes: HashMap<NodeUrl, NodeState>,
    /// URL of the currently selected/focused node
    pub selected_node: Option<NodeUrl>,
    /// Index of the currently focused node box for scrolling (used in Nodes view)
    pub focused_node_index: usize,
    /// Auto-refresh interval in seconds
    pub refresh_interval_secs: u64,
    /// Whether auto-refresh is enabled
    pub auto_refresh: bool,
    /// Time since last refresh for countdown display
    pub time_since_last_refresh: std::time::Duration,
    /// Whether a refresh is currently in progress
    pub is_refreshing: bool,
    /// Flag to signal the application should exit
    pub should_quit: bool,
    /// Whether recording mode is enabled
    pub is_recording: bool,
}

impl AppState {
    pub fn new(primary_url: NodeUrl) -> Self {
        Self {
            screen: AppScreen::Splash,
            splash_start_time: Some(std::time::Instant::now()),
            primary_node_url: primary_url.clone(),
            current_menu: MenuSelection::Nodes,
            nodes: HashMap::new(),
            selected_node: Some(primary_url),
            focused_node_index: 0,
            refresh_interval_secs: 10,
            auto_refresh: true,
            time_since_last_refresh: std::time::Duration::from_secs(0),
            is_refreshing: false,
            should_quit: false,
            is_recording: false,
        }
    }

    /// Checks if the splash screen timeout period has elapsed.
    pub fn is_splash_finished(&self) -> bool {
        self.splash_start_time
            .is_none_or(|start| start.elapsed().as_secs() >= SPLASH_TIMEOUT_SECS)
    }

    /// Transitions from splash screen to the main application interface.
    pub fn transition_to_main(&mut self) {
        self.screen = AppScreen::Main;
        self.splash_start_time = None;
    }

    /// Adds a new node to the monitoring list.
    pub fn add_node(&mut self, url: NodeUrl) {
        let node_state = NodeState::new(url.clone());
        self.nodes.insert(url.clone(), node_state);
        if self.selected_node.is_none() {
            self.selected_node = Some(url);
        }
    }

    /// Removes a node from the monitoring list.
    pub fn remove_node(&mut self, url: &NodeUrl) {
        self.nodes.remove(url);
        if self.selected_node.as_ref() == Some(url) {
            self.selected_node = self.nodes.keys().next().cloned();
        }
    }

    pub fn get_selected_node(&self) -> Option<&NodeState> {
        let url = self.selected_node.as_ref()?;
        self.nodes.get(url)
    }

    pub fn select_next_node(&mut self) {
        if self.nodes.is_empty() {
            return;
        }

        let node_urls: Vec<NodeUrl> = self.nodes.keys().cloned().collect();

        if let Some(current_url) = &self.selected_node {
            if let Some(current_index) = node_urls.iter().position(|url| url == current_url) {
                let next_index = (current_index + 1) % node_urls.len();
                self.selected_node = Some(node_urls[next_index].clone());
            }
        } else if !node_urls.is_empty() {
            self.selected_node = Some(node_urls[0].clone());
        }
    }

    pub fn select_previous_node(&mut self) {
        if self.nodes.is_empty() {
            return;
        }

        let node_urls: Vec<NodeUrl> = self.nodes.keys().cloned().collect();

        if let Some(current_url) = &self.selected_node {
            if let Some(current_index) = node_urls.iter().position(|url| url == current_url) {
                let prev_index = if current_index == 0 {
                    node_urls.len() - 1
                } else {
                    current_index - 1
                };
                self.selected_node = Some(node_urls[prev_index].clone());
            }
        } else if !node_urls.is_empty() {
            self.selected_node = Some(node_urls[0].clone());
        }
    }

    pub fn get_selected_node_url(&self) -> Option<String> {
        self.selected_node
            .as_ref()
            .map(|url| url.as_str().to_string())
    }

    pub fn set_node_alias(&mut self, url: &NodeUrl, alias: Option<String>) {
        if let Some(node) = self.nodes.get_mut(url) {
            node.set_alias(alias);
        }
    }

    pub fn get_node_urls(&self) -> Vec<String> {
        self.nodes.keys().map(|k| k.as_str().to_string()).collect()
    }

    /// Increases the refresh interval by 1 second, up to a maximum of 60 seconds.
    pub fn increase_refresh_interval(&mut self) {
        if self.refresh_interval_secs < 60 {
            self.refresh_interval_secs += 1;
        }
    }

    /// Decreases the refresh interval by 1 second, down to a minimum of 1 second.
    pub fn decrease_refresh_interval(&mut self) {
        if self.refresh_interval_secs > 1 {
            self.refresh_interval_secs -= 1;
        }
    }

    /// Toggles auto-refresh on/off.
    pub fn toggle_auto_refresh(&mut self) {
        self.auto_refresh = !self.auto_refresh;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_app_screen_debug() {
        let splash = AppScreen::Splash;
        let main = AppScreen::Main;

        assert!(format!("{splash:?}").contains("Splash"));
        assert!(format!("{main:?}").contains("Main"));
    }

    #[test]
    fn test_menu_selection_debug() {
        let nodes = MenuSelection::Nodes;
        let data_sync = MenuSelection::DataSync;
        let metrics = MenuSelection::Metrics;
        let logs = MenuSelection::Logs;
        let settings = MenuSelection::Settings;

        assert!(format!("{nodes:?}").contains("Nodes"));
        assert!(format!("{data_sync:?}").contains("DataSync"));
        assert!(format!("{metrics:?}").contains("Metrics"));
        assert!(format!("{logs:?}").contains("Logs"));
        assert!(format!("{settings:?}").contains("Settings"));
    }

    #[test]
    fn test_node_state_new() {
        let url = NodeUrl::new("http://localhost:1984").unwrap();
        let node = NodeState::new(url.clone());

        assert_eq!(node.url, url);
        assert_eq!(node.alias, None);
        assert!(!node.is_reachable);
        assert_eq!(node.response_time_ms, None);
        assert!(node.peers.is_empty());
        assert!(node.last_updated <= Utc::now());
        assert!(
            Utc::now()
                .signed_duration_since(node.last_updated)
                .num_seconds()
                < 5
        );
    }

    #[test]
    fn test_node_state_display_name_without_alias() {
        let node = NodeState::new(NodeUrl::new("http://localhost:1984").unwrap());
        assert_eq!(node.display_name(), "localhost:1984");

        let node2 = NodeState::new(NodeUrl::new("https://api.irys.xyz:443").unwrap());
        assert_eq!(node2.display_name(), "api.irys.xyz:443");

        let node3 = NodeState::new(NodeUrl::new("http://192.168.1.100:8080/path").unwrap());
        assert_eq!(node3.display_name(), "192.168.1.100:8080");
    }

    #[test]
    fn test_node_state_display_name_with_alias() {
        let mut node = NodeState::new(NodeUrl::new("http://localhost:1984").unwrap());
        node.set_alias(Some("Main Node".to_string()));
        assert_eq!(node.display_name(), "Main Node");
    }

    #[test]
    fn test_node_state_set_alias() {
        let mut node = NodeState::new(NodeUrl::new("http://localhost:1984").unwrap());

        node.set_alias(Some("Test Node".to_string()));
        assert_eq!(node.alias.as_deref(), Some("Test Node"));

        node.set_alias(None);
        assert_eq!(node.alias, None);
    }

    #[test]
    fn test_node_state_display_name_edge_cases() {
        // Test URL with just domain (no path)
        let node1 = NodeState::new(NodeUrl::new("http://localhost").unwrap());
        assert_eq!(node1.display_name(), "localhost");

        // Test URL with IP address
        let node2 = NodeState::new(NodeUrl::new("http://127.0.0.1").unwrap());
        assert_eq!(node2.display_name(), "127.0.0.1");

        // Test URL with subdomain
        let node3 = NodeState::new(NodeUrl::new("https://node.example.com").unwrap());
        assert_eq!(node3.display_name(), "node.example.com");
    }

    #[test]
    fn test_app_state_new() {
        let primary_url = NodeUrl::new("http://localhost:1984").unwrap();
        let app_state = AppState::new(primary_url.clone());

        assert!(matches!(app_state.screen, AppScreen::Splash));
        assert!(app_state.splash_start_time.is_some());
        assert_eq!(app_state.primary_node_url, primary_url);
        assert!(matches!(app_state.current_menu, MenuSelection::Nodes));
        assert!(app_state.nodes.is_empty());
        assert_eq!(app_state.selected_node, Some(primary_url));
        assert_eq!(app_state.refresh_interval_secs, 10);
        assert!(app_state.auto_refresh);
        assert_eq!(
            app_state.time_since_last_refresh,
            std::time::Duration::from_secs(0)
        );
        assert!(!app_state.is_refreshing);
        assert!(!app_state.should_quit);
    }

    #[test]
    fn test_app_state_is_splash_finished() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());

        // Should not be finished immediately
        assert!(!app_state.is_splash_finished());

        // Test with no splash start time
        app_state.splash_start_time = None;
        assert!(app_state.is_splash_finished());
    }

    #[test]
    fn test_app_state_transition_to_main() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());

        assert!(matches!(app_state.screen, AppScreen::Splash));
        assert!(app_state.splash_start_time.is_some());

        app_state.transition_to_main();

        assert!(matches!(app_state.screen, AppScreen::Main));
        assert!(app_state.splash_start_time.is_none());
    }

    #[test]
    fn test_app_state_add_node() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let new_node_url = NodeUrl::new("http://localhost:1985").unwrap();

        app_state.add_node(new_node_url.clone());

        assert!(app_state.nodes.contains_key(&new_node_url));
        assert_eq!(app_state.nodes[&new_node_url].url, new_node_url);
    }

    #[test]
    fn test_app_state_add_node_sets_selection_when_none() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        app_state.selected_node = None;

        let new_node_url = NodeUrl::new("http://localhost:1985").unwrap();
        app_state.add_node(new_node_url.clone());

        assert_eq!(app_state.selected_node, Some(new_node_url));
    }

    #[test]
    fn test_app_state_remove_node() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let node_url = NodeUrl::new("http://localhost:1985").unwrap();

        app_state.add_node(node_url.clone());
        assert!(app_state.nodes.contains_key(&node_url));

        app_state.remove_node(&node_url);
        assert!(!app_state.nodes.contains_key(&node_url));
    }

    #[test]
    fn test_app_state_remove_selected_node() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let node1_url = NodeUrl::new("http://localhost:1985").unwrap();
        let node2_url = NodeUrl::new("http://localhost:1986").unwrap();

        app_state.add_node(node1_url.clone());
        app_state.add_node(node2_url);
        app_state.selected_node = Some(node1_url.clone());

        app_state.remove_node(&node1_url);

        // Should automatically select another available node
        assert_ne!(app_state.selected_node, Some(node1_url));
        assert!(app_state.selected_node.is_some());
    }

    #[test]
    fn test_app_state_remove_last_node() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let node_url = NodeUrl::new("http://localhost:1985").unwrap();

        app_state.add_node(node_url.clone());
        app_state.selected_node = Some(node_url.clone());

        app_state.remove_node(&node_url);

        // Should have no selected node
        assert_eq!(app_state.selected_node, None);
    }

    #[test]
    fn test_app_state_get_selected_node() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let node_url = NodeUrl::new("http://localhost:1985").unwrap();

        // No selected node returns None
        app_state.selected_node = None;
        assert!(app_state.get_selected_node().is_none());

        // Add a node and select it
        app_state.add_node(node_url.clone());
        app_state.selected_node = Some(node_url.clone());

        let selected = app_state.get_selected_node();
        assert!(selected.is_some());
        assert_eq!(selected.unwrap().url, node_url);
    }

    #[test]
    fn test_app_state_select_next_node_empty() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        app_state.selected_node = None;

        app_state.select_next_node();

        // Should remain None with no nodes
        assert_eq!(app_state.selected_node, None);
    }

    #[test]
    fn test_app_state_select_next_node() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let node1_url = NodeUrl::new("http://localhost:1985").unwrap();
        let node2_url = NodeUrl::new("http://localhost:1986").unwrap();

        app_state.add_node(node1_url.clone());
        app_state.add_node(node2_url);
        app_state.selected_node = Some(node1_url);

        let initial_selection = app_state.selected_node.clone();
        app_state.select_next_node();

        // Should select a different node
        assert_ne!(app_state.selected_node, initial_selection);
        assert!(app_state.selected_node.is_some());
    }

    #[test]
    fn test_app_state_select_next_node_wraps_around() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let node_url = NodeUrl::new("http://localhost:1985").unwrap();

        app_state.add_node(node_url.clone());
        app_state.selected_node = Some(node_url.clone());

        app_state.select_next_node();

        // With only one node, should stay selected
        assert_eq!(app_state.selected_node, Some(node_url));
    }

    #[test]
    fn test_app_state_select_previous_node_empty() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        app_state.selected_node = None;

        app_state.select_previous_node();

        // Should remain None with no nodes
        assert_eq!(app_state.selected_node, None);
    }

    #[test]
    fn test_app_state_select_previous_node() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let node1_url = NodeUrl::new("http://localhost:1985").unwrap();
        let node2_url = NodeUrl::new("http://localhost:1986").unwrap();

        app_state.add_node(node1_url.clone());
        app_state.add_node(node2_url);
        app_state.selected_node = Some(node1_url);

        let initial_selection = app_state.selected_node.clone();
        app_state.select_previous_node();

        // Should select a different node
        assert_ne!(app_state.selected_node, initial_selection);
        assert!(app_state.selected_node.is_some());
    }

    #[test]
    fn test_app_state_select_previous_node_with_no_selection() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let node_url = NodeUrl::new("http://localhost:1985").unwrap();

        app_state.add_node(node_url.clone());
        app_state.selected_node = None;

        app_state.select_previous_node();

        // Should select the first (and only) node
        assert_eq!(app_state.selected_node, Some(node_url));
    }

    #[test]
    fn test_app_state_get_selected_node_url() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let node_url = NodeUrl::new("http://localhost:1985").unwrap();

        // No selection
        app_state.selected_node = None;
        assert_eq!(app_state.get_selected_node_url(), None);

        // With selection
        app_state.selected_node = Some(node_url.clone());
        assert_eq!(
            app_state.get_selected_node_url(),
            Some(node_url.as_str().to_string())
        );
    }

    #[test]
    fn test_app_state_set_node_alias() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let node_url = NodeUrl::new("http://localhost:1985").unwrap();

        app_state.add_node(node_url.clone());

        // Set alias
        app_state.set_node_alias(&node_url, Some("Test Node".to_string()));
        assert_eq!(
            app_state.nodes[&node_url].alias.as_deref(),
            Some("Test Node")
        );

        // Clear alias
        app_state.set_node_alias(&node_url, None);
        assert_eq!(app_state.nodes[&node_url].alias, None);
    }

    #[test]
    fn test_app_state_set_node_alias_nonexistent() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let nonexistent_url = NodeUrl::new("http://localhost:9999").unwrap();

        // Should not panic when setting alias for non-existent node
        app_state.set_node_alias(&nonexistent_url, Some("Test".to_string()));

        // No node should be added
        assert!(!app_state.nodes.contains_key(&nonexistent_url));
    }

    #[test]
    fn test_app_state_get_node_urls() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let node1_url = NodeUrl::new("http://localhost:1985").unwrap();
        let node2_url = NodeUrl::new("http://localhost:1986").unwrap();

        // Empty initially
        assert!(app_state.get_node_urls().is_empty());

        app_state.add_node(node1_url.clone());
        app_state.add_node(node2_url.clone());

        let urls = app_state.get_node_urls();
        assert_eq!(urls.len(), 2);
        assert!(urls.contains(&node1_url.as_str().to_string()));
        assert!(urls.contains(&node2_url.as_str().to_string()));
    }

    #[test]
    fn test_node_selection_cycling_with_multiple_nodes() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let node1_url = NodeUrl::new("http://localhost:1985").unwrap();
        let node2_url = NodeUrl::new("http://localhost:1986").unwrap();
        let node3_url = NodeUrl::new("http://localhost:1987").unwrap();

        app_state.add_node(node1_url.clone());
        app_state.add_node(node2_url);
        app_state.add_node(node3_url);

        // Set initial selection
        app_state.selected_node = Some(node1_url);

        let initial = app_state.selected_node.clone();

        // Cycle through all nodes
        app_state.select_next_node();
        let second = app_state.selected_node.clone();
        assert_ne!(second, initial);

        app_state.select_next_node();
        let third = app_state.selected_node.clone();
        assert_ne!(third, second);
        assert_ne!(third, initial);

        app_state.select_next_node();
        let fourth = app_state.selected_node.clone();
        // Should wrap around
        assert_eq!(fourth, initial);
    }

    #[test]
    fn test_node_selection_cycling_backwards() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());
        let node1_url = NodeUrl::new("http://localhost:1985").unwrap();
        let node2_url = NodeUrl::new("http://localhost:1986").unwrap();

        app_state.add_node(node1_url.clone());
        app_state.add_node(node2_url);

        // Set initial selection
        app_state.selected_node = Some(node1_url);

        let initial = app_state.selected_node.clone();

        // Go backwards
        app_state.select_previous_node();
        let previous = app_state.selected_node.clone();
        assert_ne!(previous, initial);

        // Go backwards again should wrap to initial
        app_state.select_previous_node();
        assert_eq!(app_state.selected_node, initial);
    }

    #[test]
    fn test_increase_refresh_interval() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());

        // Default is 10
        assert_eq!(app_state.refresh_interval_secs, 10);

        // Increase by 1
        app_state.increase_refresh_interval();
        assert_eq!(app_state.refresh_interval_secs, 11);

        // Test near maximum (60)
        app_state.refresh_interval_secs = 59;
        app_state.increase_refresh_interval();
        assert_eq!(app_state.refresh_interval_secs, 60);

        // Should not exceed 60
        app_state.increase_refresh_interval();
        assert_eq!(app_state.refresh_interval_secs, 60);
    }

    #[test]
    fn test_decrease_refresh_interval() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());

        // Default is 10
        assert_eq!(app_state.refresh_interval_secs, 10);

        // Decrease by 1
        app_state.decrease_refresh_interval();
        assert_eq!(app_state.refresh_interval_secs, 9);

        // Test near minimum (1)
        app_state.refresh_interval_secs = 2;
        app_state.decrease_refresh_interval();
        assert_eq!(app_state.refresh_interval_secs, 1);

        // Should not go below 1
        app_state.decrease_refresh_interval();
        assert_eq!(app_state.refresh_interval_secs, 1);
    }

    #[test]
    fn test_toggle_auto_refresh() {
        let mut app_state = AppState::new(NodeUrl::new("http://localhost:1984").unwrap());

        // Default is true
        assert!(app_state.auto_refresh);

        // Toggle off
        app_state.toggle_auto_refresh();
        assert!(!app_state.auto_refresh);

        // Toggle on
        app_state.toggle_auto_refresh();
        assert!(app_state.auto_refresh);
    }
}
