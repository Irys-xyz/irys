use crate::{app::state::AppState, ui::logo::IrysLogo};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

pub struct SplashScreen;

impl Default for SplashScreen {
    fn default() -> Self {
        Self
    }
}

impl SplashScreen {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, app_state: &AppState) {
        frame.render_widget(Clear, area);

        let main_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(15),
                Constraint::Min(25),
                Constraint::Length(8),
                Constraint::Percentage(15),
            ])
            .split(area);

        let center_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(5),
                Constraint::Percentage(90),
                Constraint::Percentage(5),
            ])
            .split(main_chunks[1]);

        let logo = IrysLogo::large()
            .with_title(false)
            .style(Style::default().fg(Color::Rgb(81, 255, 214)));

        frame.render_widget(logo, center_chunks[1]);

        let mut status_lines = Vec::new();

        if let Some(start_time) = app_state.splash_start_time {
            let elapsed = start_time.elapsed().as_secs();
            let remaining = 5_u64.saturating_sub(elapsed);

            let progress = (elapsed as f32 / 5.0 * 20.0) as usize;
            let bar = "█".repeat(progress) + &"░".repeat(20 - progress);
            status_lines.push(Line::from(vec![Span::styled(
                format!("[{bar}] {remaining}s"),
                Style::default().fg(Color::White),
            )]));
        }

        status_lines.push(Line::from(""));
        status_lines.push(Line::from(vec![Span::styled(
            "Press any key to continue...",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::ITALIC),
        )]));

        let status = Paragraph::new(status_lines).alignment(Alignment::Center);

        frame.render_widget(status, main_chunks[2]);
    }
}
