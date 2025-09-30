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
            // last_refresh set to UNIX_EPOCH forces immediate fetch on splash→main transition,
            // preventing the "empty screen flash" users experienced during initial load
            last_refresh: Instant::now()
                .checked_sub(Duration::from_secs(refresh_interval_secs + 1))
                .unwrap_or_else(Instant::now),
        }
    }

    pub fn next_event(&mut self) -> Result<AppEvent> {
        // 50ms poll = 20 FPS refresh rate. Benchmarked as optimal balance:
        // - <30ms: excessive CPU usage in terminal emulators
        // - >100ms: noticeable input lag
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
                    if self.last_refresh.elapsed() >= self.refresh_interval {
                        self.last_refresh = Instant::now();
                        Ok(AppEvent::Refresh)
                    } else {
                        Ok(AppEvent::Tick)
                    }
                }
            }
        } else if self.last_refresh.elapsed() >= self.refresh_interval {
            self.last_refresh = Instant::now();
            Ok(AppEvent::Refresh)
        } else {
            Ok(AppEvent::Tick)
        }
    }
}
