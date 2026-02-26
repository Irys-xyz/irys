pub mod config;
pub mod handlers;
pub mod node_manager;
pub mod recording;
pub mod refresh;
pub mod rendering;
pub mod state;

use crate::{
    api::client::ApiClient,
    db::db::Database,
    db::db_queue::DatabaseWriter,
    types::{NodeUrl, NotRecording, Recording},
    ui::{input::InputHandler, menu::MainMenu, splash::SplashScreen},
    utils::{
        events::{AppEvent, EventHandler},
        terminal::Tui,
    },
};
use config::TuiConfig;
use eyre::Result;
use handlers::EventHandlers;
use refresh::RefreshManager;
use rendering::RenderingManager;
use state::{AppScreen, AppState, MenuSelection};
use std::{marker::PhantomData, time::Instant};
use tokio_util::sync::CancellationToken;

pub struct App<State = NotRecording> {
    state: AppState,
    api_client: ApiClient,
    menu: MainMenu,
    splash: SplashScreen,
    input_handler: InputHandler,
    event_handler: EventHandler,
    last_refresh: Instant,
    refresh_cancel_token: CancellationToken,
    database_writer: Option<DatabaseWriter>,
    initial_refresh_done: bool,
    _phantom: PhantomData<State>,
}

impl App<NotRecording> {
    pub fn new(node_urls: Vec<String>, config_path: Option<String>) -> Result<Self> {
        let config = TuiConfig::load(config_path)?;
        let mut all_nodes: Vec<(NodeUrl, Option<String>)> = Vec::new();

        // Validate command-line URLs
        for url in node_urls {
            let validated_url =
                NodeUrl::new(&url).map_err(|e| eyre::eyre!("Invalid node URL '{}': {}", url, e))?;
            all_nodes.push((validated_url, None));
        }

        // Validate config URLs
        for node in config.nodes.iter() {
            let validated_url = NodeUrl::new(&node.url)
                .map_err(|e| eyre::eyre!("Invalid config URL '{}': {}", node.url, e))?;
            all_nodes.push((validated_url, node.alias.clone()));
        }

        if all_nodes.is_empty() {
            return Err(eyre::eyre!(
                "No nodes configured. Please specify nodes via command line arguments or config file."
            ));
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
            database_writer: None,
            initial_refresh_done: false,
            _phantom: PhantomData,
        })
    }

    pub async fn start_recording(self) -> Result<App<Recording>> {
        let db = Database::new("irys-tui-records.db").await?;
        if let Err(e) = db.truncate_records().await {
            tracing::warn!("Failed to truncate existing records: {}", e);
        }

        let mut state = self.state;
        state.is_recording = true;

        Ok(App {
            state,
            api_client: self.api_client,
            menu: self.menu,
            splash: self.splash,
            input_handler: self.input_handler,
            event_handler: self.event_handler,
            last_refresh: self.last_refresh,
            refresh_cancel_token: self.refresh_cancel_token,
            database_writer: Some(DatabaseWriter::new(db)),
            initial_refresh_done: self.initial_refresh_done,
            _phantom: PhantomData,
        })
    }
}

impl App<Recording> {
    pub async fn stop_recording(self) -> Result<App<NotRecording>> {
        if let Some(writer) = self.database_writer {
            writer.shutdown().await?;
        }

        let mut state = self.state;
        state.is_recording = false;

        Ok(App {
            state,
            api_client: self.api_client,
            menu: self.menu,
            splash: self.splash,
            input_handler: self.input_handler,
            event_handler: self.event_handler,
            last_refresh: self.last_refresh,
            refresh_cancel_token: self.refresh_cancel_token,
            database_writer: None,
            initial_refresh_done: self.initial_refresh_done,
            _phantom: PhantomData,
        })
    }

    pub fn database_writer(&self) -> &DatabaseWriter {
        self.database_writer
            .as_ref()
            .expect("Recording state always has database_writer")
    }
}

impl<State> App<State> {
    pub async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        self.state.splash_start_time = Some(std::time::Instant::now());

        loop {
            if matches!(self.state.screen, AppScreen::Splash) && self.state.is_splash_finished() {
                self.state.transition_to_main();
            }

            if matches!(self.state.screen, AppScreen::Main) && !self.initial_refresh_done {
                let _ = RefreshManager::refresh_data(
                    &mut self.state,
                    &self.api_client,
                    &mut self.refresh_cancel_token,
                    &self.database_writer,
                )
                .await;
                self.initial_refresh_done = true;
                self.last_refresh = std::time::Instant::now();
            }

            self.state.time_since_last_refresh = self.last_refresh.elapsed();

            terminal.draw(|frame| {
                RenderingManager::render_ui(
                    frame,
                    &mut self.state,
                    &mut self.menu,
                    &self.splash,
                    &self.input_handler,
                );
            })?;

            match self.event_handler.next_event()? {
                AppEvent::Key(key) => {
                    if EventHandlers::handle_key_event(
                        key,
                        &mut self.state,
                        &self.api_client,
                        &mut self.input_handler,
                        &mut self.menu,
                        &mut self.database_writer,
                    )
                    .await?
                    {
                        break;
                    }
                }
                AppEvent::Tick => {}
                AppEvent::Quit => break,
                AppEvent::Refresh => {
                    if matches!(self.state.screen, AppScreen::Main) && self.state.auto_refresh {
                        self.last_refresh = Instant::now();
                        RefreshManager::refresh_data(
                            &mut self.state,
                            &self.api_client,
                            &mut self.refresh_cancel_token,
                            &self.database_writer,
                        )
                        .await?;

                        // Also refresh view-specific data
                        match self.state.current_menu {
                            MenuSelection::DataSync => {
                                RefreshManager::refresh_observability_data(
                                    &mut self.state,
                                    &self.api_client,
                                )
                                .await?;
                            }
                            MenuSelection::Mempool => {
                                RefreshManager::refresh_mempool_data(
                                    &mut self.state,
                                    &self.api_client,
                                    &self.database_writer,
                                )
                                .await?;
                            }
                            MenuSelection::Mining => {
                                RefreshManager::refresh_mining_data(
                                    &mut self.state,
                                    &self.api_client,
                                    &self.database_writer,
                                )
                                .await?;
                            }
                            MenuSelection::Forks => {
                                RefreshManager::refresh_forks_data(
                                    &mut self.state,
                                    &self.api_client,
                                    &self.database_writer,
                                )
                                .await?;
                            }
                            MenuSelection::Config => {
                                RefreshManager::refresh_config_data(
                                    &mut self.state,
                                    &self.api_client,
                                )
                                .await?;
                            }
                            _ => {}
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
}
