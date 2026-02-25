use crate::{
    app::state::{AppScreen, AppState},
    ui::{
        input::{InputHandler, InputMode},
        menu::MainMenu,
        splash::SplashScreen,
    },
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Clear, Paragraph},
};

pub struct RenderingManager;

impl RenderingManager {
    pub fn render_ui(
        frame: &mut Frame,
        state: &mut AppState,
        menu: &mut MainMenu,
        splash: &SplashScreen,
        input_handler: &InputHandler,
    ) {
        match state.screen {
            AppScreen::Splash => {
                splash.render(frame, frame.area(), state);
            }
            AppScreen::Main => {
                menu.render(frame, frame.area(), state);

                match input_handler.mode {
                    InputMode::AddNode | InputMode::SetAlias => {
                        if let Some((prompt, text, cursor)) = input_handler.get_prompt() {
                            Self::render_input_dialog(frame, frame.area(), prompt, text, cursor);
                        }
                    }
                    InputMode::ConfirmRemove => {
                        if let Some(message) = input_handler.get_confirmation_message() {
                            Self::render_confirmation_dialog(frame, frame.area(), &message);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn render_input_dialog(
        frame: &mut Frame,
        area: Rect,
        prompt: &str,
        text: &str,
        _cursor: usize,
    ) {
        let popup_area = Self::centered_rect(60, 20, area);

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

    fn render_confirmation_dialog(frame: &mut Frame, area: Rect, message: &str) {
        let popup_area = Self::centered_rect(50, 15, area);

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

    fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
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
}
