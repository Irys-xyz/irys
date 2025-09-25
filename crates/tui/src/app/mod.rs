pub mod config;
pub mod state;

use crate::{
    api::client::ApiClient,
    ui::{
        input::{InputHandler, InputResult},
        menu::MainMenu,
        splash::SplashScreen,
    },
    utils::{
        events::{AppEvent, EventHandler},
        terminal::Tui,
    },
};
use config::TuiConfig;
use crossterm::event::{KeyCode, KeyEvent};
use eyre::Result;
use state::AppState;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Temporary structure to hold node refresh data
#[derive(Default)]
struct NodeData {
    info: Option<crate::api::models::NodeInfo>,
    is_reachable: bool,
    response_time_ms: Option<u64>,
    chain_height: Option<crate::api::models::ChainHeight>,
    peers: Option<crate::api::models::PeerListResponse>,
    mempool_status: Option<crate::api::models::MempoolStatus>,
}

pub struct App {
    state: AppState,
    api_client: ApiClient,
    menu: MainMenu,
    splash: SplashScreen,
    input_handler: InputHandler,
    event_handler: EventHandler,
    last_refresh: Instant,
    refresh_cancel_token: CancellationToken,
}

impl App {
    pub fn new(node_urls: Vec<String>, config_path: Option<String>) -> Result<Self> {
        let config = TuiConfig::load(config_path)?;
        let mut all_nodes = Vec::new();

        // Add nodes from command line arguments
        for url in node_urls {
            all_nodes.push((url, None));
        }

        // Add nodes from config file
        for node in config.nodes.iter() {
            all_nodes.push((node.url.clone(), node.alias.clone()));
        }

        // Ensure we have at least one node configured
        if all_nodes.is_empty() {
            return Err(eyre::eyre!("No nodes configured. Please specify nodes via command line arguments or config file."));
        }

        let primary_url = all_nodes
            .first()
            .map(|(url, _)| url.clone())
            .expect("Should have at least one node");

        let mut state = AppState::new(primary_url);
        let api_client = ApiClient::new(config.connection_timeout_secs)?;
        let menu = MainMenu::new();
        let input_handler = InputHandler::new();
        let event_handler = EventHandler::new(config.refresh_interval_secs);

        for (url, alias) in all_nodes {
            state.add_node(url.clone());
            if let Some(alias_name) = alias {
                state.set_node_alias(&url, Some(alias_name));
            }
        }

        if let Some(first_url) = state.nodes.keys().next().cloned() {
            state.selected_node = Some(first_url);
        }

        Ok(Self {
            state,
            api_client,
            menu,
            splash: SplashScreen::new(),
            input_handler,
            event_handler,
            last_refresh: Instant::now(),
            refresh_cancel_token: CancellationToken::new(),
        })
    }

    pub async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        // Do initial refresh before loop starts
        // This ensures state is properly updated with fetched data
        // The splash screen will cover any wait time
        let _ = self.refresh_data().await;

        loop {
            if matches!(self.state.screen, state::AppScreen::Splash)
                && self.state.is_splash_finished()
            {
                self.state.transition_to_main();
            }

            self.state.time_since_last_refresh = self.last_refresh.elapsed();

            terminal.draw(|frame| {
                self.render_ui(frame);
            })?;

            match self.event_handler.next_event()? {
                AppEvent::Key(key) => {
                    if self.handle_key_event(key).await? {
                        break;
                    }
                }
                AppEvent::Tick => {
                    if matches!(self.state.screen, state::AppScreen::Splash)
                        && self.state.is_splash_finished()
                    {
                        self.state.transition_to_main();
                    }
                }
                AppEvent::Quit => break,
                AppEvent::Refresh => {
                    if matches!(self.state.screen, state::AppScreen::Main)
                        && self.state.auto_refresh
                    {
                        self.refresh_data().await?;

                        // Also refresh observability data if in DataSync view
                        if matches!(self.state.current_menu, state::MenuSelection::DataSync) {
                            self.refresh_observability_data().await?;
                        }

                        // Also refresh mempool data if in Mempool view
                        if matches!(self.state.current_menu, state::MenuSelection::Mempool) {
                            self.refresh_mempool_data().await?;
                        }

                        // Also refresh mining data if in Mining view
                        if matches!(self.state.current_menu, state::MenuSelection::Mining) {
                            self.refresh_mining_data().await?;
                        }
                    }
                }
            }

            if self.state.should_quit {
                break;
            }
        }

        Ok(())
    }

    fn render_ui(&mut self, frame: &mut ratatui::Frame) {
        match self.state.screen {
            state::AppScreen::Splash => {
                self.splash.render(frame, frame.area(), &self.state);
            }
            state::AppScreen::Main => {
                use crate::ui::input::InputMode;

                self.menu.render(frame, frame.area(), &mut self.state);

                match self.input_handler.mode {
                    InputMode::AddNode | InputMode::SetAlias => {
                        if let Some((prompt, text, cursor)) = self.input_handler.get_prompt() {
                            self.render_input_dialog(frame, frame.area(), prompt, text, cursor);
                        }
                    }
                    InputMode::ConfirmRemove => {
                        if let Some(message) = self.input_handler.get_confirmation_message() {
                            self.render_confirmation_dialog(frame, frame.area(), &message);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn render_input_dialog(
        &self,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        prompt: &str,
        text: &str,
        _cursor: usize,
    ) {
        use ratatui::{
            layout::{Alignment, Constraint, Direction, Layout},
            style::{Color, Style},
            widgets::{Block, Borders, Clear, Paragraph},
        };

        let popup_area = self.centered_rect(60, 20, area);

        frame.render_widget(Clear, popup_area);

        let input_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(2),
            ])
            .split(popup_area);

        let prompt_paragraph = Paragraph::new(prompt)
            .style(Style::default().fg(Color::Yellow))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL));

        frame.render_widget(prompt_paragraph, input_chunks[0]);

        let input_paragraph = Paragraph::new(text)
            .style(Style::default().fg(Color::White))
            .block(Block::default().borders(Borders::ALL).title("URL"));

        frame.render_widget(input_paragraph, input_chunks[1]);

        let help_paragraph = Paragraph::new("Press Enter to confirm, Esc to cancel")
            .style(Style::default().fg(Color::Gray))
            .alignment(Alignment::Center);

        frame.render_widget(help_paragraph, input_chunks[2]);
    }

    fn render_confirmation_dialog(
        &self,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        message: &str,
    ) {
        use ratatui::{
            layout::{Alignment, Constraint, Direction, Layout},
            style::{Color, Style},
            widgets::{Block, Borders, Clear, Paragraph},
        };

        let popup_area = self.centered_rect(50, 15, area);

        frame.render_widget(Clear, popup_area);

        let confirm_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Length(2)])
            .split(popup_area);

        let message_paragraph = Paragraph::new(message)
            .style(Style::default().fg(Color::Yellow))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title("Confirm"));

        frame.render_widget(message_paragraph, confirm_chunks[0]);

        let help_paragraph = Paragraph::new("Press Y to confirm, N or Esc to cancel")
            .style(Style::default().fg(Color::Gray))
            .alignment(Alignment::Center);

        frame.render_widget(help_paragraph, confirm_chunks[1]);
    }

    fn centered_rect(
        &self,
        percent_x: u16,
        percent_y: u16,
        r: ratatui::layout::Rect,
    ) -> ratatui::layout::Rect {
        use ratatui::layout::{Constraint, Direction, Layout};

        let popup_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage((100 - percent_y) / 2),
                Constraint::Percentage(percent_y),
                Constraint::Percentage((100 - percent_y) / 2),
            ])
            .split(r);

        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage((100 - percent_x) / 2),
                Constraint::Percentage(percent_x),
                Constraint::Percentage((100 - percent_x) / 2),
            ])
            .split(popup_layout[1])[1]
    }

    async fn handle_key_event(&mut self, key: KeyEvent) -> Result<bool> {
        use crate::ui::input::InputMode;

        if let Some(result) = self.input_handler.handle_input(key)? {
            match result {
                InputResult::AddNode(url) => {
                    if self.is_valid_url(&url) {
                        self.add_node(url).await?;
                    }
                }
                InputResult::SetNodeAlias { url, alias } => {
                    self.state.set_node_alias(&url, alias);
                }
                InputResult::RemoveNode(url) => {
                    self.remove_node(&url);
                }
            }
            return Ok(false);
        }

        if matches!(self.state.screen, state::AppScreen::Splash) {
            self.state.transition_to_main();
            // Refresh will happen in the main loop after transition
            return Ok(false);
        }

        if self.input_handler.mode == InputMode::Normal {
            match key.code {
                KeyCode::Char('q') => {
                    self.state.should_quit = true;
                    return Ok(true);
                }
                KeyCode::Char('r') => {
                    // Spawn refresh in background
                    let refresh_future = self.refresh_data();

                    // Run with short timeout to avoid blocking UI
                    let _ = tokio::time::timeout(
                        tokio::time::Duration::from_millis(50),
                        refresh_future,
                    )
                    .await;

                    // Also refresh observability data if in DataSync view
                    if matches!(self.state.current_menu, state::MenuSelection::DataSync) {
                        let _ = tokio::time::timeout(
                            tokio::time::Duration::from_millis(50),
                            self.refresh_observability_data(),
                        )
                        .await;
                    }

                    // Also refresh mempool data if in Mempool view
                    if matches!(self.state.current_menu, state::MenuSelection::Mempool) {
                        let _ = tokio::time::timeout(
                            tokio::time::Duration::from_millis(50),
                            self.refresh_mempool_data(),
                        )
                        .await;
                    }

                    // Also refresh mining data if in Mining view
                    if matches!(self.state.current_menu, state::MenuSelection::Mining) {
                        let _ = tokio::time::timeout(
                            tokio::time::Duration::from_millis(50),
                            self.refresh_mining_data(),
                        )
                        .await;
                    }
                }
                KeyCode::Char('t') => {
                    self.state.toggle_auto_refresh();
                }
                KeyCode::Char('+' | '=') => {
                    self.state.increase_refresh_interval();
                }
                KeyCode::Char('-' | '_') => {
                    self.state.decrease_refresh_interval();
                }
                KeyCode::Char('a') => {
                    self.input_handler.start_add_node();
                }
                KeyCode::Char('e') => {
                    let url_to_edit =
                        if matches!(self.state.current_menu, state::MenuSelection::Nodes) {
                            let node_urls: Vec<String> = self.state.nodes.keys().cloned().collect();
                            if self.state.focused_node_index < node_urls.len() {
                                Some(node_urls[self.state.focused_node_index].clone())
                            } else {
                                None
                            }
                        } else {
                            self.state.get_selected_node_url()
                        };

                    if let Some(url) = url_to_edit {
                        let current_alias = self
                            .state
                            .nodes
                            .get(&url)
                            .and_then(|node| node.alias.clone());
                        self.input_handler.start_set_alias(url, current_alias);
                    }
                }
                KeyCode::Char('d') => {
                    let url_to_delete =
                        if matches!(self.state.current_menu, state::MenuSelection::Nodes) {
                            let node_urls: Vec<String> = self.state.nodes.keys().cloned().collect();
                            if self.state.focused_node_index < node_urls.len() {
                                Some(node_urls[self.state.focused_node_index].clone())
                            } else {
                                None
                            }
                        } else {
                            self.state.get_selected_node_url()
                        };

                    if let Some(url) = url_to_delete {
                        if self.state.nodes.len() > 1 {
                            self.input_handler.start_remove_node(url);
                        }
                    }
                }
                KeyCode::Up => match self.state.current_menu {
                    state::MenuSelection::Nodes => {
                        let node_urls: Vec<String> = self.state.nodes.keys().cloned().collect();
                        if self.state.focused_node_index < node_urls.len() {
                            let url = &node_urls[self.state.focused_node_index];
                            if let Some(node) = self.state.nodes.get_mut(url) {
                                if node.scroll_offset > 0 {
                                    node.scroll_offset = node.scroll_offset.saturating_sub(1);
                                }
                            }
                        }
                    }
                    state::MenuSelection::DataSync => {
                        self.state.select_previous_node();
                    }
                    _ => {
                        self.menu.previous();
                    }
                },
                KeyCode::Down => match self.state.current_menu {
                    state::MenuSelection::Nodes => {
                        let node_urls: Vec<String> = self.state.nodes.keys().cloned().collect();
                        if self.state.focused_node_index < node_urls.len() {
                            let url = &node_urls[self.state.focused_node_index];
                            if let Some(node) = self.state.nodes.get_mut(url) {
                                node.scroll_offset = node.scroll_offset.saturating_add(1);
                            }
                        }
                    }
                    state::MenuSelection::DataSync => {
                        self.state.select_next_node();
                    }
                    _ => {
                        self.menu.next();
                    }
                },
                KeyCode::Left => {
                    self.menu.previous();
                }
                KeyCode::Right => {
                    self.menu.next();
                }
                KeyCode::Enter => {
                    if let Some(selection) = self.menu.get_selected() {
                        self.state.current_menu = selection.clone();

                        // Fetch observability data when entering DataSync view
                        if matches!(selection, state::MenuSelection::DataSync) {
                            let _ = self.refresh_observability_data().await;
                        }

                        // Fetch mempool data when entering Mempool view
                        if matches!(selection, state::MenuSelection::Mempool) {
                            let _ = self.refresh_mempool_data().await;
                        }

                        // Fetch mining data when entering Mining view
                        if matches!(selection, state::MenuSelection::Mining) {
                            let _ = self.refresh_mining_data().await;
                        }
                    }
                }
                KeyCode::Tab => {
                    if matches!(self.state.current_menu, state::MenuSelection::Nodes) {
                        let node_count = self.state.nodes.len();
                        if node_count > 0 {
                            self.state.focused_node_index =
                                (self.state.focused_node_index + 1) % node_count;
                        }
                    } else {
                        self.menu.next();
                    }
                }
                KeyCode::BackTab => {
                    if matches!(self.state.current_menu, state::MenuSelection::Nodes) {
                        let node_count = self.state.nodes.len();
                        if node_count > 0 {
                            if self.state.focused_node_index == 0 {
                                self.state.focused_node_index = node_count - 1;
                            } else {
                                self.state.focused_node_index -= 1;
                            }
                        }
                    } else {
                        self.menu.previous();
                    }
                }
                KeyCode::PageUp => {
                    if matches!(self.state.current_menu, state::MenuSelection::Nodes) {
                        let node_urls: Vec<String> = self.state.nodes.keys().cloned().collect();
                        if self.state.focused_node_index < node_urls.len() {
                            let url = &node_urls[self.state.focused_node_index];
                            if let Some(node) = self.state.nodes.get_mut(url) {
                                node.scroll_offset = node.scroll_offset.saturating_sub(5);
                            }
                        }
                    }
                }
                KeyCode::PageDown => {
                    if matches!(self.state.current_menu, state::MenuSelection::Nodes) {
                        let node_urls: Vec<String> = self.state.nodes.keys().cloned().collect();
                        if self.state.focused_node_index < node_urls.len() {
                            let url = &node_urls[self.state.focused_node_index];
                            if let Some(node) = self.state.nodes.get_mut(url) {
                                node.scroll_offset = node.scroll_offset.saturating_add(5);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(false)
    }

    async fn refresh_observability_data(&mut self) -> Result<()> {
        // Only fetch observability data for reachable nodes
        let urls: Vec<String> = self
            .state
            .nodes
            .iter()
            .filter(|(_, node)| node.is_reachable)
            .map(|(url, _)| url.clone())
            .collect();

        let cancel_token = CancellationToken::new();

        for url in urls {
            if let Ok(chunk_counts) = self
                .api_client
                .get_all_partition_chunk_counts_cancellable(&url, &cancel_token)
                .await
            {
                if let Some(node_state) = self.state.nodes.get_mut(&url) {
                    node_state.metrics.chunk_counts = chunk_counts;
                }
            }
        }

        Ok(())
    }

    async fn refresh_mempool_data(&mut self) -> Result<()> {
        // Only fetch mempool data for reachable nodes
        let urls: Vec<String> = self
            .state
            .nodes
            .iter()
            .filter(|(_, node)| node.is_reachable)
            .map(|(url, _)| url.clone())
            .collect();

        let cancel_token = CancellationToken::new();

        for url in urls {
            if let Ok(mempool_status) = self
                .api_client
                .get_mempool_status_cancellable(&url, &cancel_token)
                .await
            {
                if let Some(node_state) = self.state.nodes.get_mut(&url) {
                    node_state.mempool_status = Some(mempool_status);
                }
            }
        }

        Ok(())
    }

    async fn refresh_mining_data(&mut self) -> Result<()> {
        // Only fetch mining data for reachable nodes
        let urls: Vec<String> = self
            .state
            .nodes
            .iter()
            .filter(|(_, node)| node.is_reachable)
            .map(|(url, _)| url.clone())
            .collect();

        let cancel_token = CancellationToken::new();

        for url in urls {
            if let Ok(mining_info) = self
                .api_client
                .get_mining_info_cancellable(&url, &cancel_token)
                .await
            {
                if let Some(node_state) = self.state.nodes.get_mut(&url) {
                    node_state.mining_info = Some(mining_info);
                }
            }
        }

        Ok(())
    }

    async fn refresh_data(&mut self) -> Result<()> {
        // Cancel any existing refresh operation
        self.refresh_cancel_token.cancel();

        // Small delay to ensure cancellation takes effect
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Create a new cancellation token for this refresh
        self.refresh_cancel_token = CancellationToken::new();
        let cancel_token = self.refresh_cancel_token.clone();

        // Mark refresh as in progress
        self.state.is_refreshing = true;

        self.last_refresh = Instant::now();
        self.state.time_since_last_refresh = std::time::Duration::from_secs(0);

        let urls: Vec<String> = self.state.nodes.keys().cloned().collect();

        // Fetch all nodes concurrently with reduced timeout
        use futures::future::join_all;

        let fetch_futures: Vec<_> = urls
            .iter()
            .map(|url| {
                let url = url.clone();
                let api_client = self.api_client.clone();
                let cancel_token = cancel_token.clone();

                async move {
                    // Use a more generous timeout for initial refresh
                    let result = tokio::time::timeout(
                        tokio::time::Duration::from_secs(3),
                        Self::fetch_node_data(&url, &api_client, &cancel_token),
                    )
                    .await;

                    match result {
                        Ok(data) => (url, data),
                        Err(_) => (url.clone(), Self::create_timeout_data()),
                    }
                }
            })
            .collect();

        let results = join_all(fetch_futures).await;

        // Update state with results
        for (url, node_data) in results {
            if cancel_token.is_cancelled() {
                break;
            }

            if let Some(node_state) = self.state.nodes.get_mut(&url) {
                Self::update_node_state(node_state, node_data);
            }
        }

        // Mark refresh as complete
        self.state.is_refreshing = false;

        Ok(())
    }

    async fn fetch_node_data(
        url: &str,
        api_client: &ApiClient,
        cancel_token: &CancellationToken,
    ) -> NodeData {
        if cancel_token.is_cancelled() {
            return Self::create_timeout_data();
        }

        let start_time = Instant::now();
        let mut data = NodeData::default();

        match api_client
            .get_node_info_cancellable(url, cancel_token)
            .await
        {
            Ok(info) => {
                data.info = Some(info);
                data.is_reachable = true;
                data.response_time_ms = Some(start_time.elapsed().as_millis() as u64);
            }
            Err(_) => {
                data.is_reachable = false;
                return data;
            }
        }

        // Fetch only basic data for node health (info already fetched above)
        // Only fetch peer list for basic connectivity info
        if let Ok(p) = api_client
            .get_peer_list_cancellable(url, cancel_token)
            .await
        {
            data.peers = Some(p);
        }

        // Extract chain height from info if available (already fetched)
        if let Some(ref info) = data.info {
            if let Ok(height) = info.height.parse::<u64>() {
                data.chain_height = Some(crate::api::models::ChainHeight { height });
            }
        }

        data
    }

    fn create_timeout_data() -> NodeData {
        NodeData {
            is_reachable: false,
            ..Default::default()
        }
    }

    fn update_node_state(node_state: &mut state::NodeState, data: NodeData) {
        node_state.metrics.info = data.info;
        node_state.is_reachable = data.is_reachable;
        node_state.response_time_ms = data.response_time_ms;

        if node_state.is_reachable {
            if let Some(height) = data.chain_height {
                node_state.metrics.chain_height = Some(height);
            }
            if let Some(peers) = data.peers {
                node_state.peers = peers.clone();
                node_state.metrics.peer_count = peers.len();
            }
            if let Some(mempool) = data.mempool_status {
                node_state.mempool_status = Some(mempool);
            }
            // Don't update chunk_counts here - only fetch when needed for DataSync view
        }

        node_state.last_updated = chrono::Utc::now();

        if let Some(response_time) = node_state.response_time_ms {
            node_state.metrics.add_response_time(response_time);
        }
    }

    /// Validates that a string is a valid HTTP(S) URL for a node endpoint.
    /// Returns true if the URL can be parsed and uses http or https scheme.
    fn is_valid_url(&self, url: &str) -> bool {
        url::Url::parse(url)
            .map(|u| u.scheme() == "http" || u.scheme() == "https")
            .unwrap_or(false)
    }

    async fn add_node(&mut self, url: String) -> Result<()> {
        if self.state.nodes.contains_key(&url) {
            return Ok(());
        }

        self.state.add_node(url.clone());

        let cancel_token = CancellationToken::new();

        let start_time = Instant::now();
        if let Some(node_state) = self.state.nodes.get_mut(&url) {
            match self
                .api_client
                .get_node_info_cancellable(&url, &cancel_token)
                .await
            {
                Ok(info) => {
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
                // Extract chain height from info
                if let Some(ref info) = node_state.metrics.info {
                    if let Ok(height) = info.height.parse::<u64>() {
                        node_state.metrics.chain_height =
                            Some(crate::api::models::ChainHeight { height });
                    }
                }

                // Fetch peer list for basic connectivity info
                if let Ok(peer_response) = self
                    .api_client
                    .get_peer_list_cancellable(&url, &cancel_token)
                    .await
                {
                    node_state.peers = peer_response;
                    node_state.metrics.peer_count = node_state.peers.len();
                }

                // Don't fetch chunk_counts here - only needed for DataSync view
            }

            node_state.last_updated = chrono::Utc::now();

            if let Some(response_time) = node_state.response_time_ms {
                node_state.metrics.add_response_time(response_time);
            }
        }

        Ok(())
    }

    fn remove_node(&mut self, url: &str) {
        // IMPORTANT: Must maintain focused_node_index consistency when removing nodes
        // to prevent out-of-bounds access in UI rendering
        if matches!(self.state.current_menu, state::MenuSelection::Nodes) {
            let node_urls: Vec<String> = self.state.nodes.keys().cloned().collect();
            if let Some(removed_index) = node_urls.iter().position(|u| u == url) {
                if removed_index == self.state.focused_node_index
                    && self.state.focused_node_index > 0
                {
                    self.state.focused_node_index -= 1;
                } else if self.state.focused_node_index >= self.state.nodes.len() - 1 {
                    self.state.focused_node_index = self.state.nodes.len().saturating_sub(2);
                }
            }
        }

        self.state.remove_node(url);
    }
}
