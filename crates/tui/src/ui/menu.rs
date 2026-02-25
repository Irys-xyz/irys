use crate::app::state::{AppState, MenuSelection};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
};

/// Fixed height ensures consistent grid layout across different terminal sizes.
/// 22 lines determined through UX testing to fit all node info without requiring
/// scrolling in most cases while maintaining readability.
const NODE_CARD_HEIGHT: u16 = 22;

pub struct MainMenu {
    list_state: ListState,
    items: Vec<(MenuSelection, &'static str, &'static str)>,
}

/// Truncates a hash string to the specified length, adding ellipsis if truncated.
fn truncate_hash(hash: &str, max_len: usize) -> String {
    if hash.len() > max_len {
        format!("{}...", &hash[..max_len])
    } else {
        hash.to_string()
    }
}

impl Default for MainMenu {
    fn default() -> Self {
        let items = vec![
            (
                MenuSelection::Nodes,
                "Nodes",
                "Node monitoring and management",
            ),
            (
                MenuSelection::DataSync,
                "Data Sync",
                "Data synchronization status",
            ),
            (
                MenuSelection::Mempool,
                "Mempool",
                "Memory pool status and metrics",
            ),
            (
                MenuSelection::Mining,
                "Mining",
                "Mining status and difficulty",
            ),
            (
                MenuSelection::Forks,
                "Forks",
                "Block tree forks and competing blocks",
            ),
            (MenuSelection::Metrics, "Metrics", "Performance metrics"),
            (MenuSelection::Config, "Config", "Node configuration"),
            (MenuSelection::Logs, "Logs", "System logs and events"),
            (
                MenuSelection::Settings,
                "Settings",
                "Configuration settings",
            ),
        ];

        let mut list_state = ListState::default();
        list_state.select(Some(0));

        Self { list_state, items }
    }
}

impl MainMenu {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next(&mut self) {
        let i = match self.list_state.selected() {
            Some(i) => (i + 1) % self.items.len(),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    pub fn previous(&mut self) {
        let i = match self.list_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.items.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    pub fn get_selected(&self) -> Option<MenuSelection> {
        self.list_state.selected().map(|i| self.items[i].0.clone())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, app_state: &mut AppState) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
            ])
            .split(area);

        self.render_title(frame, chunks[0], app_state);

        let content_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
            .split(chunks[1]);

        self.render_menu_list(frame, content_chunks[0]);

        self.render_content(frame, content_chunks[1], app_state);

        self.render_status_bar(frame, chunks[2], app_state);
    }

    fn render_title(&self, frame: &mut Frame, area: Rect, app_state: &AppState) {
        let title_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(30), Constraint::Min(20)])
            .split(area);

        let header_text = Paragraph::new(vec![Line::from(vec![Span::styled(
            "IRYS",
            Style::default()
                .fg(Color::Rgb(81, 255, 214))
                .add_modifier(Modifier::BOLD),
        )])])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(81, 255, 214))),
        )
        .alignment(ratatui::layout::Alignment::Center);

        frame.render_widget(header_text, title_chunks[0]);

        let menu_name = match app_state.current_menu {
            MenuSelection::Nodes => "Nodes",
            MenuSelection::DataSync => "Data Sync",
            MenuSelection::Mempool => "Mempool",
            MenuSelection::Mining => "Mining",
            MenuSelection::Forks => "Forks",
            MenuSelection::Metrics => "Metrics",
            MenuSelection::Config => "Config",
            MenuSelection::Logs => "Logs",
            MenuSelection::Settings => "Settings",
        };

        let refresh_display = if app_state.is_refreshing {
            "↻ Refreshing...".to_string()
        } else if app_state.auto_refresh {
            let elapsed_secs = app_state.time_since_last_refresh.as_secs();
            let remaining_secs = app_state.refresh_interval_secs.saturating_sub(elapsed_secs);
            format!("↻ {remaining_secs}s")
        } else {
            "↻ OFF".to_string()
        };

        let mut first_line = vec![
            Span::styled(
                menu_name,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                refresh_display,
                Style::default().fg(if app_state.is_refreshing {
                    Color::Yellow
                } else if app_state.auto_refresh {
                    Color::Green
                } else {
                    Color::Gray
                }),
            ),
        ];

        if app_state.is_recording {
            first_line.push(Span::raw("  "));
            first_line.push(Span::styled(
                "● REC",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }

        let mut lines = vec![Line::from(first_line)];

        let second_line = vec![Span::styled(
            format!("Nodes: {}", app_state.nodes.len()),
            Style::default().fg(Color::Cyan),
        )];

        lines.push(Line::from(second_line));

        let mode_info = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL))
            .alignment(ratatui::layout::Alignment::Center);

        frame.render_widget(mode_info, title_chunks[1]);
    }

    fn render_menu_list(&mut self, frame: &mut Frame, area: Rect) {
        let menu_items: Vec<ListItem> = self
            .items
            .iter()
            .map(|(_, name, _description)| {
                ListItem::new(Line::from(vec![Span::styled(
                    *name,
                    Style::default().fg(Color::White),
                )]))
            })
            .collect();

        let menu = List::new(menu_items)
            .block(Block::default().title("Menu").borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(menu, area, &mut self.list_state);
    }

    fn render_content(&self, frame: &mut Frame, area: Rect, app_state: &mut AppState) {
        match app_state.current_menu {
            MenuSelection::Nodes => self.render_nodes_view(frame, area, app_state),
            MenuSelection::DataSync => self.render_data_sync_view(frame, area, app_state),
            MenuSelection::Mempool => self.render_mempool_view(frame, area, app_state),
            MenuSelection::Mining => self.render_mining_view(frame, area, app_state),
            MenuSelection::Forks => self.render_forks_view(frame, area, app_state),
            MenuSelection::Metrics => self.render_metrics_view(frame, area, app_state),
            MenuSelection::Config => self.render_config_view(frame, area, app_state),
            MenuSelection::Logs => self.render_logs_view(frame, area, app_state),
            MenuSelection::Settings => self.render_settings_view(frame, area, app_state),
        }
    }

    fn render_nodes_view(&self, frame: &mut Frame, area: Rect, app_state: &mut AppState) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(0)])
            .split(area);

        self.render_cluster_health_banner(frame, chunks[0], app_state);
        self.render_nodes_grid(frame, chunks[1], app_state);
    }

    fn render_cluster_health_banner(&self, frame: &mut Frame, area: Rect, app_state: &AppState) {
        let total_nodes = app_state.nodes.len();
        let online_nodes = app_state.nodes.values().filter(|n| n.is_reachable).count();
        let offline_nodes = total_nodes - online_nodes;
        let health_percentage = self.calculate_cluster_health_percentage(online_nodes, total_nodes);
        let health_color = self.get_health_color(health_percentage);

        let mut spans = vec![
            Span::styled(
                "CLUSTER HEALTH: ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{health_percentage:.1}%"),
                Style::default()
                    .fg(health_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  │  "),
            Span::raw("Total: "),
            Span::styled(
                total_nodes.to_string(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  │  "),
            Span::raw("Online: "),
            Span::styled(
                online_nodes.to_string(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  │  "),
            Span::raw("Offline: "),
            Span::styled(
                offline_nodes.to_string(),
                Style::default()
                    .fg(if offline_nodes > 0 {
                        Color::Red
                    } else {
                        Color::Gray
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ];

        // Add average response time if available
        let avg_response_times: Vec<f64> = app_state
            .nodes
            .values()
            .filter_map(|n| n.metrics.average_response_time())
            .collect();

        if !avg_response_times.is_empty() {
            let overall_avg =
                avg_response_times.iter().sum::<f64>() / avg_response_times.len() as f64;
            spans.push(Span::raw("  │  "));
            spans.push(Span::raw("Avg Response: "));
            spans.push(Span::styled(
                format!("{overall_avg:.1}ms"),
                Style::default()
                    .fg(if overall_avg < 1000.0 {
                        Color::Green
                    } else {
                        Color::Yellow
                    })
                    .add_modifier(Modifier::BOLD),
            ));
        }

        let banner = Paragraph::new(Line::from(spans))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(health_color)),
            )
            .alignment(ratatui::layout::Alignment::Center);

        frame.render_widget(banner, area);
    }

    fn render_nodes_grid(&self, frame: &mut Frame, area: Rect, app_state: &mut AppState) {
        if app_state.nodes.is_empty() {
            let no_nodes_msg = Paragraph::new(vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    "No nodes configured",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )]),
                Line::from(""),
                Line::from("Press 'a' to add a node or 'q' to quit"),
            ])
            .block(Block::default().title("Nodes").borders(Borders::ALL))
            .alignment(ratatui::layout::Alignment::Center);

            frame.render_widget(no_nodes_msg, area);
            return;
        }

        let nodes_per_row = 3;
        let node_count = app_state.nodes.len();
        let rows_needed = node_count.div_ceil(nodes_per_row);

        let mut row_constraints = vec![];
        for _ in 0..rows_needed {
            row_constraints.push(Constraint::Length(NODE_CARD_HEIGHT));
        }
        row_constraints.push(Constraint::Min(0));

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(row_constraints)
            .split(area);

        let node_urls: Vec<_> = app_state.nodes.keys().cloned().collect();

        let mut node_index = 0;
        for row_idx in 0..rows_needed {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(33),
                    Constraint::Percentage(33),
                    Constraint::Percentage(34),
                ])
                .split(rows[row_idx]);

            for col_idx in 0..nodes_per_row {
                if node_index < node_urls.len() {
                    let url = &node_urls[node_index];
                    if let Some(node) = app_state.nodes.get_mut(url) {
                        let is_focused = node_index == app_state.focused_node_index;
                        self.render_node_card(frame, cols[col_idx], url.as_str(), node, is_focused);
                    }
                    node_index += 1;
                }
            }
        }
    }

    fn render_node_card(
        &self,
        frame: &mut Frame,
        area: Rect,
        url: &str,
        node: &mut crate::app::state::NodeState,
        is_focused: bool,
    ) {
        let (status_color, status_text, status_bg) = if node.is_reachable {
            (Color::White, "Online", Color::Green)
        } else {
            (Color::White, "Offline", Color::Red)
        };

        let mut lines = vec![];

        lines.push(Line::from(vec![
            Span::styled(
                url,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!(" {status_text} "),
                Style::default()
                    .fg(status_color)
                    .bg(status_bg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        lines.push(Line::from(""));

        lines.push(Line::from(vec![Span::styled(
            "Node Info:",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        )]));

        if let Some(info) = &node.metrics.info {
            lines.push(Line::from(vec![
                Span::styled("version: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(&info.version),
            ]));
            lines.push(Line::from(vec![
                Span::styled("peerCount: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(info.peer_count.to_string()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("chainId: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(&info.chain_id),
            ]));
            lines.push(Line::from(vec![
                Span::styled("height: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(&info.height),
            ]));
            lines.push(Line::from(vec![
                Span::styled("blockHash: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(truncate_hash(&info.block_hash, 20)),
            ]));
            lines.push(Line::from(vec![
                Span::styled(
                    "blockIndexHeight: ",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(&info.block_index_height),
            ]));
            lines.push(Line::from(vec![
                Span::styled(
                    "blockIndexHash: ",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(truncate_hash(&info.block_index_hash, 20)),
            ]));
            lines.push(Line::from(vec![
                Span::styled(
                    "pendingBlocks: ",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(&info.pending_blocks),
            ]));
            lines.push(Line::from(vec![
                Span::styled("isSyncing: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(info.is_syncing.to_string()),
            ]));
            lines.push(Line::from(vec![
                Span::styled(
                    "currentSyncHeight: ",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(info.current_sync_height.to_string()),
            ]));
        } else {
            lines.push(Line::from("No node info available"));
        }

        lines.push(Line::from(""));

        lines.push(Line::from(vec![Span::styled(
            "Peer Details:",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        )]));

        if !node.peers.is_empty() {
            if let Some(peer) = node.peers.first() {
                lines.push(Line::from(vec![
                    Span::styled("Gossip: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(&peer.gossip),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("API: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(&peer.api),
                ]));
                lines.push(Line::from(vec![
                    Span::styled(
                        "Peering TCP: ",
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(&peer.execution.peering_tcp_addr),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("Peer ID: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(truncate_hash(&peer.execution.peer_id, 20)),
                ]));
            }
        } else {
            lines.push(Line::from("No peer details available"));
        }

        node.content_height = lines.len() as u16;

        let visible_height = area.height.saturating_sub(2);

        if node.content_height <= visible_height {
            node.scroll_offset = 0;
        } else {
            let max_scroll = node.content_height.saturating_sub(visible_height);
            if node.scroll_offset > max_scroll {
                node.scroll_offset = max_scroll;
            }
        }

        let start_line = node.scroll_offset as usize;
        let end_line = ((node.scroll_offset + visible_height) as usize).min(lines.len());
        let visible_lines: Vec<Line> = lines[start_line..end_line].to_vec();

        let border_style = if is_focused {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if node.is_reachable {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red)
        };

        let node_card = Paragraph::new(visible_lines)
            .block(
                Block::default()
                    .title(node.display_name())
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .wrap(ratatui::widgets::Wrap { trim: false });

        frame.render_widget(node_card, area);

        if node.content_height > visible_height {
            let scrollbar_area = Rect {
                x: area.x + area.width - 1,
                y: area.y + 1,
                width: 1,
                height: visible_height,
            };

            let mut scrollbar_state = ScrollbarState::default()
                .content_length(node.content_height as usize)
                .position(node.scroll_offset as usize)
                .viewport_content_length(visible_height as usize);

            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"))
                .track_symbol(Some("│"))
                .thumb_symbol("█");

            frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
        }
    }

    /// Renders detailed node information in a card-style format
    /// Used by detail views (DataSync, Mempool, Mining, etc.) to match the Nodes page style
    fn render_node_detail_card(
        &self,
        frame: &mut Frame,
        area: Rect,
        node_state: &crate::app::state::NodeState,
        title: &str,
        custom_lines: Vec<Line>,
    ) {
        let (status_color, status_text, status_bg) = if node_state.is_reachable {
            (Color::White, "Online", Color::Green)
        } else {
            (Color::White, "Offline", Color::Red)
        };

        let mut lines = vec![];

        lines.push(Line::from(vec![
            Span::styled(
                node_state.url.as_ref(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!(" {status_text} "),
                Style::default()
                    .fg(status_color)
                    .bg(status_bg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        lines.push(Line::from(""));

        // Node Info section
        lines.push(Line::from(vec![Span::styled(
            "Node Info:",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        )]));

        if let Some(info) = &node_state.metrics.info {
            lines.push(Line::from(vec![
                Span::styled("height: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(&info.height),
            ]));
            lines.push(Line::from(vec![
                Span::styled("peerCount: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(info.peer_count.to_string()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("chainId: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(&info.chain_id),
            ]));
            lines.push(Line::from(vec![
                Span::styled("isSyncing: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(info.is_syncing.to_string()),
            ]));
        } else {
            lines.push(Line::from("No node info available"));
        }

        lines.push(Line::from(""));

        // Custom content section (specific to each view)
        if !custom_lines.is_empty() {
            lines.push(Line::from(vec![Span::styled(
                title,
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )]));
            lines.extend(custom_lines);
        }

        let border_style = if node_state.is_reachable {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red)
        };

        let node_card = Paragraph::new(lines)
            .block(
                Block::default()
                    .title(node_state.display_name())
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .wrap(ratatui::widgets::Wrap { trim: false });

        frame.render_widget(node_card, area);
    }

    fn render_data_sync_view(&self, frame: &mut Frame, area: Rect, app_state: &AppState) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        let title = Paragraph::new(vec![Line::from(vec![Span::styled(
            "Cluster Data Sync Monitor",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )])])
        .block(Block::default().borders(Borders::ALL))
        .alignment(ratatui::layout::Alignment::Center);

        frame.render_widget(title, chunks[0]);

        let node_count = app_state.nodes.len();
        if node_count == 0 {
            let no_nodes = Paragraph::new("No nodes configured")
                .block(Block::default().title("Nodes").borders(Borders::ALL))
                .alignment(ratatui::layout::Alignment::Center);
            frame.render_widget(no_nodes, chunks[1]);
            return;
        }

        // Use grid layout like Nodes view
        let nodes_per_row = 3;
        let rows_needed = node_count.div_ceil(nodes_per_row);
        let node_height = 14_u16; // Height for data sync cards

        let mut row_constraints = vec![];
        for _ in 0..rows_needed {
            row_constraints.push(Constraint::Length(node_height));
        }
        row_constraints.push(Constraint::Min(0));

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(row_constraints)
            .split(chunks[1]);

        let node_list: Vec<_> = app_state.nodes.iter().collect();

        let mut node_index = 0;
        for row_idx in 0..rows_needed {
            if row_idx >= rows.len() {
                break;
            }

            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(33),
                    Constraint::Percentage(33),
                    Constraint::Percentage(34),
                ])
                .split(rows[row_idx]);

            for col_idx in 0..nodes_per_row {
                if node_index < node_list.len() {
                    let (_url, node_state) = node_list[node_index];

                    // Prepare custom lines for data sync details
                    let mut custom_lines = Vec::new();

                    if node_state.is_reachable {
                        let chunk_counts = &node_state.metrics.chunk_counts;

                        custom_lines.push(Line::from(vec![
                            Span::styled(
                                "Publish: ",
                                Style::default().add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(format!(
                                "Data={:3} Packed={:3}",
                                chunk_counts.publish_0.data, chunk_counts.publish_0.packed
                            )),
                        ]));

                        custom_lines.push(Line::from(vec![
                            Span::styled(
                                "Submit(0): ",
                                Style::default().add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(format!(
                                "Data={:3} Packed={:3}",
                                chunk_counts.submit_0.data, chunk_counts.submit_0.packed
                            )),
                        ]));

                        custom_lines.push(Line::from(vec![
                            Span::styled(
                                "Submit(1): ",
                                Style::default().add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(format!(
                                "Data={:3} Packed={:3}",
                                chunk_counts.submit_1.data, chunk_counts.submit_1.packed
                            )),
                        ]));

                        // Display total chunk offsets (from blockchain)
                        let total_offsets = &node_state.metrics.total_chunk_offsets;
                        custom_lines.push(Line::from(vec![
                            Span::styled(
                                "Max Offsets: ",
                                Style::default().add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(format!(
                                "Pub={} Sub={}",
                                total_offsets
                                    .publish
                                    .map(|o| o.to_string())
                                    .unwrap_or_else(|| "None".to_string()),
                                total_offsets
                                    .submit
                                    .map(|o| o.to_string())
                                    .unwrap_or_else(|| "None".to_string())
                            )),
                        ]));
                    }

                    self.render_node_detail_card(
                        frame,
                        cols[col_idx],
                        node_state,
                        "Data Sync Details:",
                        custom_lines,
                    );

                    node_index += 1;
                }
            }
        }
    }

    fn render_mempool_view(&self, frame: &mut Frame, area: Rect, app_state: &AppState) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        let title = Paragraph::new(vec![Line::from(vec![Span::styled(
            "Cluster Mempool Status Monitor",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )])])
        .block(Block::default().borders(Borders::ALL))
        .alignment(ratatui::layout::Alignment::Center);

        frame.render_widget(title, chunks[0]);

        let node_count = app_state.nodes.len();
        if node_count == 0 {
            let no_nodes = Paragraph::new("No nodes configured")
                .block(Block::default().title("Nodes").borders(Borders::ALL))
                .alignment(ratatui::layout::Alignment::Center);
            frame.render_widget(no_nodes, chunks[1]);
            return;
        }

        // Use grid layout like Nodes view
        let nodes_per_row = 3;
        let rows_needed = node_count.div_ceil(nodes_per_row);
        let node_height = 14_u16; // Height for mempool cards

        let mut row_constraints = vec![];
        for _ in 0..rows_needed {
            row_constraints.push(Constraint::Length(node_height));
        }
        row_constraints.push(Constraint::Min(0));

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(row_constraints)
            .split(chunks[1]);

        let node_list: Vec<_> = app_state.nodes.iter().collect();

        let mut node_index = 0;
        for row_idx in 0..rows_needed {
            if row_idx >= rows.len() {
                break;
            }

            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(33),
                    Constraint::Percentage(33),
                    Constraint::Percentage(34),
                ])
                .split(rows[row_idx]);

            for col_idx in 0..nodes_per_row {
                if node_index < node_list.len() {
                    let (_url, node_state) = node_list[node_index];

                    // Prepare custom lines for mempool details
                    let mut custom_lines = Vec::new();

                    if node_state.is_reachable {
                        if let Some(mempool_status) = &node_state.mempool_status {
                            // Transaction counts
                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Transactions: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(format!(
                                    "Data={} Commitment={}",
                                    mempool_status.data_tx_count,
                                    mempool_status.commitment_tx_count
                                )),
                            ]));

                            // Pending items
                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Pending: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(format!(
                                    "Chunks={} Pledges={}",
                                    mempool_status.pending_chunks_count,
                                    mempool_status.pending_pledges_count
                                )),
                            ]));

                            // Recent transactions
                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Recent: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(format!(
                                    "Valid={} Invalid={}",
                                    mempool_status.recent_valid_tx_count,
                                    mempool_status.recent_invalid_tx_count
                                )),
                            ]));

                            // Total data size
                            let size_mb =
                                mempool_status.data_tx_total_size as f64 / (1024.0 * 1024.0);
                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Total Size: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(format!("{size_mb:.2} MB")),
                            ]));

                            // Pool utilization indicator
                            let utilization = if mempool_status.data_tx_count > 100 {
                                ("HIGH", Color::Red)
                            } else if mempool_status.data_tx_count > 50 {
                                ("MEDIUM", Color::Yellow)
                            } else {
                                ("LOW", Color::Green)
                            };

                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Utilization: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(
                                    format!(" {} ", utilization.0),
                                    Style::default()
                                        .fg(Color::White)
                                        .bg(utilization.1)
                                        .add_modifier(Modifier::BOLD),
                                ),
                            ]));
                        } else {
                            custom_lines.push(Line::from(vec![Span::styled(
                                "No mempool data available",
                                Style::default().fg(Color::Yellow),
                            )]));
                        }
                    }

                    self.render_node_detail_card(
                        frame,
                        cols[col_idx],
                        node_state,
                        "Mempool Details:",
                        custom_lines,
                    );

                    node_index += 1;
                }
            }
        }
    }

    fn render_mining_view(&self, frame: &mut Frame, area: Rect, app_state: &AppState) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        let title = Paragraph::new(vec![Line::from(vec![Span::styled(
            "Cluster Mining Status Monitor",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )])])
        .block(Block::default().borders(Borders::ALL))
        .alignment(ratatui::layout::Alignment::Center);

        frame.render_widget(title, chunks[0]);

        let node_count = app_state.nodes.len();
        if node_count == 0 {
            let no_nodes = Paragraph::new("No nodes configured")
                .block(Block::default().title("Nodes").borders(Borders::ALL))
                .alignment(ratatui::layout::Alignment::Center);
            frame.render_widget(no_nodes, chunks[1]);
            return;
        }

        // Use grid layout like Nodes view
        let nodes_per_row = 3;
        let rows_needed = node_count.div_ceil(nodes_per_row);
        let node_height = 16_u16; // Height for mining cards (slightly taller due to more content)

        let mut row_constraints = vec![];
        for _ in 0..rows_needed {
            row_constraints.push(Constraint::Length(node_height));
        }
        row_constraints.push(Constraint::Min(0));

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(row_constraints)
            .split(chunks[1]);

        let node_list: Vec<_> = app_state.nodes.iter().collect();

        let mut node_index = 0;
        for row_idx in 0..rows_needed {
            if row_idx >= rows.len() {
                break;
            }

            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(33),
                    Constraint::Percentage(33),
                    Constraint::Percentage(34),
                ])
                .split(rows[row_idx]);

            for col_idx in 0..nodes_per_row {
                if node_index < node_list.len() {
                    let (_url, node_state) = node_list[node_index];

                    // Prepare custom lines for mining details
                    let mut custom_lines = Vec::new();

                    if node_state.is_reachable {
                        if let Some(mining_info) = &node_state.mining_info {
                            // Block information
                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Block Height: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(mining_info.block_height.to_string()),
                            ]));

                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Block Hash: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(truncate_hash(&mining_info.block_hash, 20)),
                            ]));

                            // Difficulty information
                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Difficulty: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(truncate_hash(&mining_info.current_difficulty, 20)),
                            ]));

                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Cumulative: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(truncate_hash(&mining_info.cumulative_difficulty, 20)),
                            ]));

                            // Mining rewards
                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Miner: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(truncate_hash(&mining_info.miner_address, 16)),
                            ]));

                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Reward: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(&mining_info.reward_amount),
                            ]));

                            // VDF information if available
                            if let Some(vdf_obj) = mining_info.vdf_limiter_info.as_object() {
                                if let Some(global_step) = vdf_obj.get("globalStepNumber") {
                                    custom_lines.push(Line::from(vec![
                                        Span::styled(
                                            "VDF Step: ",
                                            Style::default().add_modifier(Modifier::BOLD),
                                        ),
                                        Span::raw(global_step.to_string()),
                                    ]));
                                }
                                if let Some(vdf_diff) = vdf_obj.get("vdfDifficulty") {
                                    if !vdf_diff.is_null() {
                                        custom_lines.push(Line::from(vec![
                                            Span::styled(
                                                "VDF Difficulty: ",
                                                Style::default().add_modifier(Modifier::BOLD),
                                            ),
                                            Span::raw(vdf_diff.to_string()),
                                        ]));
                                    }
                                }
                            }

                            // Block timestamp
                            let timestamp_secs = mining_info.block_timestamp / 1000;
                            let datetime =
                                chrono::DateTime::from_timestamp(timestamp_secs as i64, 0)
                                    .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                                    .unwrap_or_else(|| "Invalid timestamp".to_string());

                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Block Time: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(datetime),
                            ]));
                        } else {
                            custom_lines.push(Line::from(vec![Span::styled(
                                "Mining info not available",
                                Style::default().fg(Color::Yellow),
                            )]));
                        }
                    }

                    self.render_node_detail_card(
                        frame,
                        cols[col_idx],
                        node_state,
                        "Mining Details:",
                        custom_lines,
                    );

                    node_index += 1;
                }
            }
        }
    }

    fn render_forks_view(&self, frame: &mut Frame, area: Rect, app_state: &AppState) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        let title = Paragraph::new(vec![Line::from(vec![Span::styled(
            "Block Tree Forks - Competing Blocks at Same Height",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )])])
        .block(Block::default().borders(Borders::ALL))
        .alignment(ratatui::layout::Alignment::Center);

        frame.render_widget(title, chunks[0]);

        let node_count = app_state.nodes.len();
        if node_count == 0 {
            let no_nodes = Paragraph::new("No nodes configured")
                .block(Block::default().title("Nodes").borders(Borders::ALL))
                .alignment(ratatui::layout::Alignment::Center);
            frame.render_widget(no_nodes, chunks[1]);
            return;
        }

        // Use grid layout like other views
        let nodes_per_row = 3;
        let rows_needed = node_count.div_ceil(nodes_per_row);
        let node_height = 18_u16; // Height for fork cards (taller due to fork details)

        let mut row_constraints = vec![];
        for _ in 0..rows_needed {
            row_constraints.push(Constraint::Length(node_height));
        }
        row_constraints.push(Constraint::Min(0));

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(row_constraints)
            .split(chunks[1]);

        let node_list: Vec<_> = app_state.nodes.iter().collect();

        let mut node_index = 0;
        for row_idx in 0..rows_needed {
            if row_idx >= rows.len() {
                break;
            }

            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(33),
                    Constraint::Percentage(33),
                    Constraint::Percentage(34),
                ])
                .split(rows[row_idx]);

            for col_idx in 0..nodes_per_row {
                if node_index < node_list.len() {
                    let (_url, node_state) = node_list[node_index];

                    // Prepare custom lines for fork details
                    let mut custom_lines = Vec::new();

                    if node_state.is_reachable {
                        if let Some(fork_info) = &node_state.fork_info {
                            // Tip information
                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Tip Height: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(fork_info.current_tip_height.to_string()),
                            ]));

                            custom_lines.push(Line::from(vec![
                                Span::styled(
                                    "Tip Hash: ",
                                    Style::default().add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(truncate_hash(&fork_info.current_tip_hash, 20)),
                            ]));

                            // Fork status
                            if fork_info.total_fork_count == 0 {
                                custom_lines.push(Line::from(vec![Span::styled(
                                    "✓ No forks detected",
                                    Style::default()
                                        .fg(Color::Green)
                                        .add_modifier(Modifier::BOLD),
                                )]));
                            } else {
                                custom_lines.push(Line::from(vec![Span::styled(
                                    format!("⚠ {} fork(s) detected", fork_info.total_fork_count),
                                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                                )]));

                                // Show first fork details if available
                                if let Some(first_fork) = fork_info.forks.first() {
                                    custom_lines.push(Line::from(""));
                                    custom_lines.push(Line::from(vec![
                                        Span::styled(
                                            "Fork at height: ",
                                            Style::default().add_modifier(Modifier::BOLD),
                                        ),
                                        Span::styled(
                                            first_fork.height.to_string(),
                                            Style::default().fg(Color::Yellow),
                                        ),
                                    ]));
                                    custom_lines.push(Line::from(vec![
                                        Span::styled(
                                            "Competing blocks: ",
                                            Style::default().add_modifier(Modifier::BOLD),
                                        ),
                                        Span::styled(
                                            first_fork.block_count.to_string(),
                                            Style::default().fg(Color::Red),
                                        ),
                                    ]));

                                    // Show up to 2 competing blocks
                                    for (idx, block) in first_fork.blocks.iter().take(2).enumerate()
                                    {
                                        let block_marker = if block.is_tip { "★" } else { "" };
                                        custom_lines.push(Line::from(vec![
                                            Span::raw(format!(
                                                "  {}Block {}: ",
                                                block_marker,
                                                idx + 1
                                            )),
                                            Span::raw(truncate_hash(&block.block_hash, 12)),
                                        ]));
                                    }

                                    if first_fork.blocks.len() > 2 {
                                        custom_lines.push(Line::from(vec![Span::styled(
                                            format!(
                                                "  ... and {} more",
                                                first_fork.blocks.len() - 2
                                            ),
                                            Style::default().fg(Color::Gray),
                                        )]));
                                    }
                                }
                            }
                        } else {
                            custom_lines.push(Line::from(vec![Span::styled(
                                "Loading fork data...",
                                Style::default().fg(Color::Gray),
                            )]));
                        }
                    }

                    self.render_node_detail_card(
                        frame,
                        cols[col_idx],
                        node_state,
                        "Fork Details:",
                        custom_lines,
                    );

                    node_index += 1;
                }
            }
        }
    }

    fn render_metrics_view(&self, frame: &mut Frame, area: Rect, _app_state: &AppState) {
        let metrics = Paragraph::new("Metrics view - to be implemented")
            .block(Block::default().title("Metrics").borders(Borders::ALL));
        frame.render_widget(metrics, area);
    }

    fn render_logs_view(&self, frame: &mut Frame, area: Rect, _app_state: &AppState) {
        let logs = Paragraph::new("Logs view - to be implemented")
            .block(Block::default().title("Logs").borders(Borders::ALL));
        frame.render_widget(logs, area);
    }

    fn render_config_view(&self, frame: &mut Frame, area: Rect, app_state: &AppState) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        let title = Paragraph::new(vec![Line::from(vec![Span::styled(
            "Node Configurations",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )])])
        .block(Block::default().borders(Borders::ALL))
        .alignment(ratatui::layout::Alignment::Center);

        frame.render_widget(title, chunks[0]);

        let node_count = app_state.nodes.len();
        if node_count == 0 {
            let no_nodes = Paragraph::new("No nodes configured")
                .block(Block::default().title("Nodes").borders(Borders::ALL))
                .alignment(ratatui::layout::Alignment::Center);
            frame.render_widget(no_nodes, chunks[1]);
            return;
        }

        // Use grid layout like Nodes view
        let nodes_per_row = 3;
        let rows_needed = node_count.div_ceil(nodes_per_row);
        let node_height = 14_u16; // Height for config cards

        let mut row_constraints = vec![];
        for _ in 0..rows_needed {
            row_constraints.push(Constraint::Length(node_height));
        }
        row_constraints.push(Constraint::Min(0));

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(row_constraints)
            .split(chunks[1]);

        let node_list: Vec<_> = app_state.nodes.iter().collect();

        let mut node_index = 0;
        for row_idx in 0..rows_needed {
            if row_idx >= rows.len() {
                break;
            }

            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(33),
                    Constraint::Percentage(33),
                    Constraint::Percentage(34),
                ])
                .split(rows[row_idx]);

            for col_idx in 0..nodes_per_row {
                if node_index < node_list.len() {
                    let (_url, node_state) = node_list[node_index];

                    // Prepare custom lines for config details
                    let mut custom_lines = Vec::new();

                    if let Some(config) = &node_state.config {
                        // Mempool Config section
                        custom_lines.push(Line::from(vec![Span::styled(
                            "Mempool:",
                            Style::default().add_modifier(Modifier::BOLD),
                        )]));

                        custom_lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled("Anchor Expiry: ", Style::default()),
                            Span::styled(
                                format!("{}", config.mempool.tx_anchor_expiry_depth),
                                Style::default().fg(Color::Yellow),
                            ),
                        ]));

                        custom_lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled("Migration Depth: ", Style::default()),
                            Span::styled(
                                format!("{}", config.block_migration_depth),
                                Style::default().fg(Color::Yellow),
                            ),
                        ]));

                        custom_lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled("Max Data Txs: ", Style::default()),
                            Span::styled(
                                format!("{}", config.mempool.max_data_txs_per_block),
                                Style::default().fg(Color::Yellow),
                            ),
                        ]));
                    } else {
                        custom_lines.push(Line::from(vec![Span::styled(
                            "Config not available",
                            Style::default().fg(Color::Yellow),
                        )]));
                    }

                    self.render_node_detail_card(
                        frame,
                        cols[col_idx],
                        node_state,
                        "Configuration:",
                        custom_lines,
                    );

                    node_index += 1;
                }
            }
        }
    }

    fn render_settings_view(&self, frame: &mut Frame, area: Rect, app_state: &AppState) {
        let lines = vec![
            Line::from(vec![Span::styled(
                "Settings",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::raw("Auto Refresh: "),
                Span::styled(
                    app_state.auto_refresh.to_string(),
                    Style::default().fg(Color::Cyan),
                ),
            ]),
            Line::from(vec![
                Span::raw("Refresh Interval: "),
                Span::styled(
                    format!("{}s", app_state.refresh_interval_secs),
                    Style::default().fg(Color::Cyan),
                ),
            ]),
            Line::from(vec![
                Span::raw("Recording: "),
                Span::styled(
                    if app_state.is_recording { "ON" } else { "OFF" },
                    Style::default().fg(if app_state.is_recording {
                        Color::Green
                    } else {
                        Color::Gray
                    }),
                ),
            ]),
            Line::from(""),
            Line::from("Press 'q' to quit"),
            Line::from("Press 'r' to refresh"),
            Line::from("Press 'c' to toggle recording"),
            Line::from("Press '↑/↓' to navigate menu"),
        ];

        let settings = Paragraph::new(lines)
            .block(Block::default().title("Settings").borders(Borders::ALL))
            .wrap(ratatui::widgets::Wrap { trim: true });

        frame.render_widget(settings, area);
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect, app_state: &AppState) {
        let selected_name = app_state
            .get_selected_node()
            .map(|node| {
                let name = node.display_name();
                if name.len() > 20 {
                    format!("{}...", &name[..17])
                } else {
                    name
                }
            })
            .unwrap_or_else(|| "None".to_string());

        let refresh_status = if app_state.auto_refresh {
            format!("{}s", app_state.refresh_interval_secs)
        } else {
            "OFF".to_string()
        };

        let status_text = if matches!(app_state.current_menu, MenuSelection::Nodes) {
            format!(
                " Nodes: {} | Focus: {} | Tab:Switch ↑↓:Scroll | a:Add e:Alias d:Delete | Refresh: {} t:Toggle +/-:Adjust r:Manual | c:Record | q:Quit ",
                app_state.nodes.len(),
                app_state.focused_node_index + 1,
                refresh_status
            )
        } else {
            format!(
                " Nodes: {} | Selected: {} | a:Add e:Alias d:Delete | Refresh: {} t:Toggle +/-:Adjust r:Manual | c:Record | q:Quit ",
                app_state.nodes.len(),
                selected_name,
                refresh_status
            )
        };

        let status = Paragraph::new(status_text)
            .style(Style::default().fg(Color::Gray))
            .block(Block::default().borders(Borders::ALL));

        frame.render_widget(status, area);
    }

    fn calculate_cluster_health_percentage(&self, online_nodes: usize, total_nodes: usize) -> f64 {
        if total_nodes == 0 {
            0.0
        } else {
            (online_nodes as f64 / total_nodes as f64) * 100.0
        }
    }

    fn get_health_color(&self, health_percentage: f64) -> Color {
        if health_percentage >= 80.0 {
            Color::Green
        } else if health_percentage >= 50.0 {
            Color::Yellow
        } else {
            Color::Red
        }
    }
}
