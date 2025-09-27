use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use eyre::Result;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum AppEvent {
    Key(KeyEvent),
    Tick,
    Refresh,
    Quit,
}

#[derive(Debug)]
pub struct EventHandler {
    pub refresh_interval: Duration,
    last_refresh: Instant,
}

impl EventHandler {
    pub fn new(refresh_interval_secs: u64) -> Self {
        Self {
            refresh_interval: Duration::from_secs(refresh_interval_secs),
            // Set last_refresh far in the past to trigger immediate refresh when main screen loads
            last_refresh: Instant::now()
                .checked_sub(Duration::from_secs(refresh_interval_secs + 1))
                .unwrap_or_else(Instant::now),
        }
    }

    pub fn next_event(&mut self) -> Result<AppEvent> {
        // Use shorter poll duration for more responsive UI
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        return Ok(AppEvent::Quit);
                    }
                    Ok(AppEvent::Key(key))
                }
                _ => {
                    // Check if it's time for a refresh
                    if self.last_refresh.elapsed() >= self.refresh_interval {
                        self.last_refresh = Instant::now();
                        Ok(AppEvent::Refresh)
                    } else {
                        Ok(AppEvent::Tick)
                    }
                }
            }
        } else {
            // Check refresh time
            if self.last_refresh.elapsed() >= self.refresh_interval {
                self.last_refresh = Instant::now();
                Ok(AppEvent::Refresh)
            } else {
                Ok(AppEvent::Tick)
            }
        }
    }
}
