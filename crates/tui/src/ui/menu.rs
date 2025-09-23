use crate::app::state::{AppState, MenuSelection};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
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
                MenuSelection::PartitionSync,
                "Partition Sync",
                "Partition data synchronization status",
            ),
            (
                MenuSelection::Mempool,
                "Mempool",
                "Memory pool status and metrics",
            ),
            (MenuSelection::Metrics, "Metrics", "Performance metrics"),
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
            MenuSelection::PartitionSync => "Partition Sync",
            MenuSelection::Mempool => "Mempool",
            MenuSelection::Metrics => "Metrics",
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

        let mode_info = Paragraph::new(vec![
            Line::from(vec![
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
            ]),
            Line::from(vec![Span::styled(
                format!("Nodes: {}", app_state.nodes.len()),
                Style::default().fg(Color::Cyan),
            )]),
        ])
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
            MenuSelection::PartitionSync => self.render_partition_sync_view(frame, area, app_state),
            MenuSelection::Mempool => self.render_mempool_view(frame, area, app_state),
            MenuSelection::Metrics => self.render_metrics_view(frame, area, app_state),
            MenuSelection::Logs => self.render_logs_view(frame, area, app_state),
            MenuSelection::Settings => self.render_settings_view(frame, area, app_state),
        }
    }

    fn render_nodes_view(&self, frame: &mut Frame, area: Rect, app_state: &mut AppState) {
        // Split the area vertically: small banner at top, nodes grid below
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(0)])
            .split(area);

        // Render cluster health banner at the top
        self.render_cluster_health_banner(frame, chunks[0], app_state);

        // Render full nodes grid below with all the original node details
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
        // Handle case where there are no nodes
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

        // Render the original full nodes view with all details
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

        let node_urls: Vec<String> = app_state.nodes.keys().cloned().collect();

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
                        self.render_node_card(frame, cols[col_idx], url, node, is_focused);
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

    fn render_partition_sync_view(&self, frame: &mut Frame, area: Rect, app_state: &AppState) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        let title = Paragraph::new(vec![Line::from(vec![Span::styled(
            "Cluster Partition Data Sync Monitor",
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

        let constraints: Vec<Constraint> = (0..node_count)
            .map(|_| Constraint::Length(8))
            .chain(std::iter::once(Constraint::Min(1)))
            .collect();

        let node_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(chunks[1]);

        for (i, (_node_url, node_state)) in app_state.nodes.iter().enumerate() {
            let display_name = node_state.display_name();

            let mut node_lines = Vec::new();

            if node_state.is_reachable {
                let mut info_spans = vec![
                    Span::styled(
                        "Node:",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(display_name.clone(), Style::default().fg(Color::Cyan)),
                ];

                if let Some(chain_height) = &node_state.metrics.chain_height {
                    info_spans.extend(vec![
                        Span::raw("  Height: "),
                        Span::styled(
                            chain_height.height.to_string(),
                            Style::default().fg(Color::Cyan),
                        ),
                    ]);
                }

                if let Some(info) = &node_state.metrics.info {
                    info_spans.extend(vec![
                        Span::raw("  Peers: "),
                        Span::styled(
                            info.peer_count.to_string(),
                            Style::default().fg(Color::Cyan),
                        ),
                    ]);
                }
                node_lines.push(Line::from(info_spans));

                let chunk_counts = &node_state.metrics.chunk_counts;

                node_lines.push(Line::from(vec![
                    Span::styled(
                        "Publish:",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        "  Data={:3} Packed={:3}",
                        chunk_counts.publish_0.data, chunk_counts.publish_0.packed
                    )),
                ]));

                node_lines.push(Line::from(vec![
                    Span::styled(
                        "Submit(0):",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        " Data={:3} Packed={:3}",
                        chunk_counts.submit_0.data, chunk_counts.submit_0.packed
                    )),
                ]));

                node_lines.push(Line::from(vec![
                    Span::styled(
                        "Submit(1):",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        " Data={:3} Packed={:3}",
                        chunk_counts.submit_1.data, chunk_counts.submit_1.packed
                    )),
                ]));
            } else {
                node_lines.push(Line::from(vec![
                    Span::styled(
                        "Node:",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(display_name.clone(), Style::default().fg(Color::Red)),
                ]));
                node_lines.push(Line::from(vec![Span::styled(
                    "ERROR - Node offline or unreachable",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )]));
            }

            let node_widget = Paragraph::new(node_lines)
                .block(
                    Block::default()
                        .title(display_name)
                        .borders(Borders::ALL)
                        .border_style(if node_state.is_reachable {
                            Style::default().fg(Color::Green)
                        } else {
                            Style::default().fg(Color::Red)
                        }),
                )
                .wrap(ratatui::widgets::Wrap { trim: true });

            if i < node_chunks.len() {
                frame.render_widget(node_widget, node_chunks[i]);
            }
        }

        if let Some(help_chunk) = node_chunks.get(node_count) {
            let help_widget = Paragraph::new("Press 'r' to refresh immediately, 'q' to quit")
                .alignment(ratatui::layout::Alignment::Center);
            frame.render_widget(help_widget, *help_chunk);
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

        // Calculate height needed for each node (approximately 12 lines per node)
        let node_height = 12u16;
        let constraints: Vec<Constraint> = (0..node_count)
            .map(|_| Constraint::Length(node_height))
            .chain(std::iter::once(Constraint::Min(1)))
            .collect();

        let node_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(chunks[1]);

        for (i, (_node_url, node_state)) in app_state.nodes.iter().enumerate() {
            let display_name = node_state.display_name();

            let mut node_lines = Vec::new();

            if node_state.is_reachable {
                // Node header with status
                node_lines.push(Line::from(vec![
                    Span::styled(
                        "Node: ",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(display_name.clone(), Style::default().fg(Color::Cyan)),
                    Span::raw("  "),
                    Span::styled(
                        " ONLINE ",
                        Style::default()
                            .fg(Color::White)
                            .bg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));

                if let Some(mempool_status) = &node_state.mempool_status {
                    // Transaction counts
                    node_lines.push(Line::from(vec![
                        Span::styled(
                            "Transactions: ",
                            Style::default()
                                .fg(Color::Blue)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!(
                            "Data={} Commitment={}",
                            mempool_status.data_tx_count, mempool_status.commitment_tx_count
                        )),
                    ]));

                    // Pending items
                    node_lines.push(Line::from(vec![
                        Span::styled(
                            "Pending: ",
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!(
                            "Chunks={} Pledges={}",
                            mempool_status.pending_chunks_count, mempool_status.pending_pledges_count
                        )),
                    ]));

                    // Recent transactions
                    node_lines.push(Line::from(vec![
                        Span::styled(
                            "Recent: ",
                            Style::default()
                                .fg(Color::Magenta)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!(
                            "Valid={} Invalid={}",
                            mempool_status.recent_valid_tx_count, mempool_status.recent_invalid_tx_count
                        )),
                    ]));

                    // Total data size
                    let size_mb = mempool_status.data_tx_total_size as f64 / (1024.0 * 1024.0);
                    node_lines.push(Line::from(vec![
                        Span::styled(
                            "Total Data Size: ",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!("{:.2} MB", size_mb)),
                    ]));

                    // Pool utilization indicator
                    let utilization = if mempool_status.data_tx_count > 100 {
                        ("HIGH", Color::Red)
                    } else if mempool_status.data_tx_count > 50 {
                        ("MEDIUM", Color::Yellow)
                    } else {
                        ("LOW", Color::Green)
                    };

                    node_lines.push(Line::from(vec![
                        Span::styled(
                            "Pool Utilization: ",
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(" {} ", utilization.0),
                            Style::default()
                                .fg(Color::White)
                                .bg(utilization.1)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]));

                    // Last update time
                    let elapsed = chrono::Utc::now()
                        .signed_duration_since(node_state.last_updated)
                        .num_seconds();
                    node_lines.push(Line::from(vec![
                        Span::styled(
                            "Last Updated: ",
                            Style::default().fg(Color::Gray),
                        ),
                        Span::styled(
                            format!("{}s ago", elapsed),
                            Style::default().fg(Color::Gray),
                        ),
                    ]));
                } else {
                    node_lines.push(Line::from(vec![Span::styled(
                        "No mempool data available",
                        Style::default().fg(Color::Yellow),
                    )]));
                    node_lines.push(Line::from(vec![Span::styled(
                        "Press 'r' to refresh",
                        Style::default().fg(Color::Gray),
                    )]));
                }
            } else {
                // Offline node
                node_lines.push(Line::from(vec![
                    Span::styled(
                        "Node: ",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(display_name.clone(), Style::default().fg(Color::Red)),
                    Span::raw("  "),
                    Span::styled(
                        " OFFLINE ",
                        Style::default()
                            .fg(Color::White)
                            .bg(Color::Red)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                node_lines.push(Line::from(vec![Span::styled(
                    "ERROR - Node offline or unreachable",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )]));
            }

            let node_widget = Paragraph::new(node_lines)
                .block(
                    Block::default()
                        .title(display_name)
                        .borders(Borders::ALL)
                        .border_style(if node_state.is_reachable {
                            Style::default().fg(Color::Green)
                        } else {
                            Style::default().fg(Color::Red)
                        }),
                )
                .wrap(ratatui::widgets::Wrap { trim: true });

            if i < node_chunks.len() {
                frame.render_widget(node_widget, node_chunks[i]);
            }
        }

        if let Some(help_chunk) = node_chunks.get(node_count) {
            let help_widget = Paragraph::new("Press 'r' to refresh immediately, 't' to toggle auto-refresh, 'q' to quit")
                .alignment(ratatui::layout::Alignment::Center);
            frame.render_widget(help_widget, *help_chunk);
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
            Line::from(""),
            Line::from("Press 'q' to quit"),
            Line::from("Press 'r' to refresh"),
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
                " Nodes: {} | Focus: {} | Tab:Switch ↑↓:Scroll | a:Add e:Alias d:Delete | Refresh: {} t:Toggle +/-:Adjust r:Manual | q:Quit ",
                app_state.nodes.len(),
                app_state.focused_node_index + 1,
                refresh_status
            )
        } else {
            format!(
                " Nodes: {} | Selected: {} | a:Add e:Alias d:Delete | Refresh: {} t:Toggle +/-:Adjust r:Manual | q:Quit ",
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
