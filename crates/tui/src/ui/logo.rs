use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IrysLogoSize {
    Small,
    #[default]
    Medium,
    Large,
}

pub struct IrysLogo {
    size: IrysLogoSize,
    style: Style,
    with_title: bool,
}

impl IrysLogo {
    pub fn new(size: IrysLogoSize) -> Self {
        Self {
            size,
            // Irys brand color from official design guidelines (aqua/cyan)
            style: Style::default().fg(Color::Rgb(81, 255, 214)),
            with_title: true,
        }
    }

    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    pub fn with_title(mut self, with_title: bool) -> Self {
        self.with_title = with_title;
        self
    }

    fn get_logo_lines(&self) -> Vec<&'static str> {
        match self.size {
            IrysLogoSize::Small => vec![
                "██╗██████╗ ██╗   ██╗███████╗",
                "██║██╔══██╗╚██╗ ██╔╝██╔════╝",
                "██║██████╔╝ ╚████╔╝ ███████╗",
                "██║██╔══██╗  ╚██╔╝  ╚════██║",
                "██║██║  ██║   ██║   ███████║",
                "╚═╝╚═╝  ╚═╝   ╚═╝   ╚══════╝",
            ],
            IrysLogoSize::Medium => vec![
                "██╗██████╗ ██╗   ██╗███████╗",
                "██║██╔══██╗╚██╗ ██╔╝██╔════╝",
                "██║██████╔╝ ╚████╔╝ ███████╗",
                "██║██╔══██╗  ╚██╔╝  ╚════██║",
                "██║██║  ██║   ██║   ███████║",
                "╚═╝╚═╝  ╚═╝   ╚═╝   ╚══════╝",
            ],
            IrysLogoSize::Large => vec![
                "██╗██████╗ ██╗   ██╗███████╗",
                "██║██╔══██╗╚██╗ ██╔╝██╔════╝",
                "██║██████╔╝ ╚████╔╝ ███████╗",
                "██║██╔══██╗  ╚██╔╝  ╚════██║",
                "██║██║  ██║   ██║   ███████║",
                "╚═╝╚═╝  ╚═╝   ╚═╝   ╚══════╝",
            ],
        }
    }
}

impl Widget for IrysLogo {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let logo_lines = self.get_logo_lines();

        let mut lines = Vec::new();

        if self.with_title {
            lines.push(Line::from(vec![Span::styled(
                "Welcome to",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::ITALIC),
            )]));
            lines.push(Line::from(""));
        }

        for logo_line in logo_lines {
            if logo_line.is_empty() {
                lines.push(Line::from(""));
            } else {
                lines.push(Line::from(vec![Span::styled(
                    logo_line,
                    self.style.add_modifier(Modifier::BOLD),
                )]));
            }
        }

        if self.with_title {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled(
                "The Programmable Datachain",
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::ITALIC),
            )]));
        }

        let paragraph = Paragraph::new(lines).alignment(Alignment::Center);

        paragraph.render(area, buf);
    }
}

impl IrysLogo {
    pub fn small() -> Self {
        Self::new(IrysLogoSize::Small)
    }

    pub fn medium() -> Self {
        Self::new(IrysLogoSize::Medium)
    }

    pub fn large() -> Self {
        Self::new(IrysLogoSize::Large)
    }

    pub fn splash() -> Self {
        Self::new(IrysLogoSize::Large).with_title(true)
    }

    pub fn header() -> Self {
        Self::new(IrysLogoSize::Small).with_title(false)
    }
}
