use crate::{
    api::client::ApiClient,
    app::{
        node_manager::NodeManager,
        recording::RecordingManager,
        refresh::RefreshManager,
        state::{AppScreen, AppState, MenuSelection},
    },
    db::db_queue::DatabaseWriter,
    types::NodeUrl,
    ui::{
        input::{InputHandler, InputMode, InputResult},
        menu::MainMenu,
    },
};
use crossterm::event::{KeyCode, KeyEvent};
use eyre::Result;

pub struct EventHandlers;

impl EventHandlers {
    pub async fn handle_key_event(
        key: KeyEvent,
        state: &mut AppState,
        api_client: &ApiClient,
        input_handler: &mut InputHandler,
        menu: &mut MainMenu,
        database_writer: &mut Option<DatabaseWriter>,
    ) -> Result<bool> {
        if let Some(result) = input_handler.handle_input(key)? {
            match result {
                InputResult::AddNode(url) => {
                    if NodeManager::is_valid_url(&url)
                        && let Ok(node_url) = NodeUrl::new(&url)
                    {
                        NodeManager::add_node(node_url, state, api_client, database_writer).await?;
                    }
                }
                InputResult::SetNodeAlias { url, alias } => {
                    if let Ok(node_url) = NodeUrl::new(&url) {
                        state.set_node_alias(&node_url, alias);
                    }
                }
                InputResult::RemoveNode(url) => {
                    if let Ok(node_url) = NodeUrl::new(&url) {
                        NodeManager::remove_node(&node_url, state);
                    }
                }
            }
            return Ok(false);
        }

        if matches!(state.screen, AppScreen::Splash) {
            state.transition_to_main();
            // Refresh will happen in the main loop after transition
            return Ok(false);
        }

        if input_handler.mode == InputMode::Normal {
            Self::handle_normal_mode_keys(
                key,
                state,
                api_client,
                input_handler,
                menu,
                database_writer,
            )
            .await
        } else {
            Ok(false)
        }
    }

    async fn handle_normal_mode_keys(
        key: KeyEvent,
        state: &mut AppState,
        api_client: &ApiClient,
        input_handler: &mut InputHandler,
        menu: &mut MainMenu,
        database_writer: &mut Option<DatabaseWriter>,
    ) -> Result<bool> {
        match key.code {
            KeyCode::Char('q') => {
                state.should_quit = true;
                return Ok(true);
            }
            KeyCode::Char('r') => {
                Self::handle_refresh(state, api_client, database_writer).await;
            }
            KeyCode::Char('t') => {
                state.toggle_auto_refresh();
            }
            KeyCode::Char('+' | '=') => {
                state.increase_refresh_interval();
            }
            KeyCode::Char('-' | '_') => {
                state.decrease_refresh_interval();
            }
            KeyCode::Char('c') => {
                RecordingManager::toggle_recording(state, database_writer).await;
            }
            KeyCode::Char('a') => {
                input_handler.start_add_node();
            }
            KeyCode::Char('e') => {
                Self::handle_edit_alias(state, input_handler);
            }
            KeyCode::Char('d') => {
                Self::handle_delete_node(state, input_handler);
            }
            KeyCode::Up => Self::handle_up_arrow(state, menu),
            KeyCode::Down => Self::handle_down_arrow(state, menu),
            KeyCode::Left => {
                menu.previous();
            }
            KeyCode::Right => {
                menu.next();
            }
            KeyCode::Enter => {
                Self::handle_enter(state, api_client, menu, database_writer).await;
            }
            KeyCode::Tab => Self::handle_tab(state, menu),
            KeyCode::BackTab => Self::handle_shift_tab(state, menu),
            KeyCode::PageUp => Self::handle_page_up(state),
            KeyCode::PageDown => Self::handle_page_down(state),
            _ => {}
        }
        Ok(false)
    }

    async fn handle_refresh(
        state: &mut AppState,
        api_client: &ApiClient,
        database_writer: &Option<DatabaseWriter>,
    ) {
        // Spawn refresh in background with short timeout to avoid blocking UI
        let mut refresh_cancel_token = tokio_util::sync::CancellationToken::new();
        let refresh_future = RefreshManager::refresh_data(
            state,
            api_client,
            &mut refresh_cancel_token,
            database_writer,
        );

        // Run with short timeout to avoid blocking UI
        let _ = tokio::time::timeout(tokio::time::Duration::from_millis(50), refresh_future).await;

        // Also refresh view-specific data
        match state.current_menu {
            MenuSelection::DataSync => {
                let _ = tokio::time::timeout(
                    tokio::time::Duration::from_millis(50),
                    RefreshManager::refresh_observability_data(state, api_client),
                )
                .await;
            }
            MenuSelection::Mempool => {
                let _ = tokio::time::timeout(
                    tokio::time::Duration::from_millis(50),
                    RefreshManager::refresh_mempool_data(state, api_client, database_writer),
                )
                .await;
            }
            MenuSelection::Mining => {
                let _ = tokio::time::timeout(
                    tokio::time::Duration::from_millis(50),
                    RefreshManager::refresh_mining_data(state, api_client, database_writer),
                )
                .await;
            }
            MenuSelection::Forks => {
                let _ = tokio::time::timeout(
                    tokio::time::Duration::from_millis(50),
                    RefreshManager::refresh_forks_data(state, api_client, database_writer),
                )
                .await;
            }
            MenuSelection::Config => {
                let _ = tokio::time::timeout(
                    tokio::time::Duration::from_millis(50),
                    RefreshManager::refresh_config_data(state, api_client),
                )
                .await;
            }
            _ => {}
        }
    }

    fn handle_edit_alias(state: &AppState, input_handler: &mut InputHandler) {
        let url_to_edit = if matches!(state.current_menu, MenuSelection::Nodes) {
            let node_urls: Vec<String> = state
                .nodes
                .keys()
                .map(std::string::ToString::to_string)
                .collect();
            if state.focused_node_index < node_urls.len() {
                Some(node_urls[state.focused_node_index].clone())
            } else {
                None
            }
        } else {
            state.get_selected_node_url()
        };

        if let Some(url) = url_to_edit
            && let Ok(node_url) = NodeUrl::new(&url)
        {
            let current_alias = state
                .nodes
                .get(&node_url)
                .and_then(|node| node.alias.as_ref().map(std::string::ToString::to_string));
            input_handler.start_set_alias(url, current_alias);
        }
    }

    fn handle_delete_node(state: &AppState, input_handler: &mut InputHandler) {
        let url_to_delete = if matches!(state.current_menu, MenuSelection::Nodes) {
            let node_urls: Vec<String> = state
                .nodes
                .keys()
                .map(std::string::ToString::to_string)
                .collect();
            if state.focused_node_index < node_urls.len() {
                Some(node_urls[state.focused_node_index].clone())
            } else {
                None
            }
        } else {
            state.get_selected_node_url()
        };

        if let Some(url) = url_to_delete
            && state.nodes.len() > 1
        {
            input_handler.start_remove_node(url);
        }
    }

    fn handle_up_arrow(state: &mut AppState, menu: &mut MainMenu) {
        match state.current_menu {
            MenuSelection::Nodes => {
                let node_urls: Vec<NodeUrl> = state.nodes.keys().cloned().collect();
                if state.focused_node_index < node_urls.len() {
                    let url = &node_urls[state.focused_node_index];
                    if let Some(node) = state.nodes.get_mut(url)
                        && node.scroll_offset > 0
                    {
                        node.scroll_offset = node.scroll_offset.saturating_sub(1);
                    }
                }
            }
            MenuSelection::DataSync => {
                state.select_previous_node();
            }
            _ => {
                menu.previous();
            }
        }
    }

    fn handle_down_arrow(state: &mut AppState, menu: &mut MainMenu) {
        match state.current_menu {
            MenuSelection::Nodes => {
                let node_urls: Vec<NodeUrl> = state.nodes.keys().cloned().collect();
                if state.focused_node_index < node_urls.len() {
                    let url = &node_urls[state.focused_node_index];
                    if let Some(node) = state.nodes.get_mut(url) {
                        node.scroll_offset = node.scroll_offset.saturating_add(1);
                    }
                }
            }
            MenuSelection::DataSync => {
                state.select_next_node();
            }
            _ => {
                menu.next();
            }
        }
    }

    async fn handle_enter(
        state: &mut AppState,
        api_client: &ApiClient,
        menu: &mut MainMenu,
        database_writer: &Option<DatabaseWriter>,
    ) {
        if let Some(selection) = menu.get_selected() {
            state.current_menu = selection.clone();

            // Fetch view-specific data when entering view
            match selection {
                MenuSelection::DataSync => {
                    let _ = RefreshManager::refresh_observability_data(state, api_client).await;
                }
                MenuSelection::Mempool => {
                    let _ =
                        RefreshManager::refresh_mempool_data(state, api_client, database_writer)
                            .await;
                }
                MenuSelection::Mining => {
                    let _ = RefreshManager::refresh_mining_data(state, api_client, database_writer)
                        .await;
                }
                _ => {}
            }
        }
    }

    // Tab behavior differs by view:
    // - Nodes view: cycles through node cards for detailed inspection
    // - Other views: navigates menu items
    // This split UX tested better than uniform behavior across all views
    fn handle_tab(state: &mut AppState, menu: &mut MainMenu) {
        if matches!(state.current_menu, MenuSelection::Nodes) {
            let node_count = state.nodes.len();
            if node_count > 0 {
                state.focused_node_index = (state.focused_node_index + 1) % node_count;
            }
        } else {
            menu.next();
        }
    }

    fn handle_shift_tab(state: &mut AppState, menu: &mut MainMenu) {
        if matches!(state.current_menu, MenuSelection::Nodes) {
            let node_count = state.nodes.len();
            if node_count > 0 {
                if state.focused_node_index == 0 {
                    state.focused_node_index = node_count - 1;
                } else {
                    state.focused_node_index -= 1;
                }
            }
        } else {
            menu.previous();
        }
    }

    fn handle_page_up(state: &mut AppState) {
        if matches!(state.current_menu, MenuSelection::Nodes) {
            let node_urls: Vec<NodeUrl> = state.nodes.keys().cloned().collect();
            if state.focused_node_index < node_urls.len() {
                let url = &node_urls[state.focused_node_index];
                if let Some(node) = state.nodes.get_mut(url) {
                    node.scroll_offset = node.scroll_offset.saturating_sub(5);
                }
            }
        }
    }

    fn handle_page_down(state: &mut AppState) {
        if matches!(state.current_menu, MenuSelection::Nodes) {
            let node_urls: Vec<NodeUrl> = state.nodes.keys().cloned().collect();
            if state.focused_node_index < node_urls.len() {
                let url = &node_urls[state.focused_node_index];
                if let Some(node) = state.nodes.get_mut(url) {
                    node.scroll_offset = node.scroll_offset.saturating_add(5);
                }
            }
        }
    }
}
