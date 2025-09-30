use crate::{app::state::AppState, db::{db::Database, db_queue::DatabaseWriter}};

pub struct RecordingManager;

impl RecordingManager {
    pub async fn toggle_recording(
        state: &mut AppState,
        database_writer: &mut Option<DatabaseWriter>,
    ) {
        if state.is_recording {
            state.is_recording = false;
            tracing::debug!(
                "Recording disabled - is_recording now: {}",
                state.is_recording
            );
        } else if database_writer.is_none() {
            match Database::new("irys-tui-records.db").await {
                Ok(db) => {
                    *database_writer = Some(DatabaseWriter::new(db));
                    state.is_recording = true;
                    tracing::debug!(
                        "Recording enabled (database initialized) - is_recording now: {}",
                        state.is_recording
                    );
                }
                Err(e) => {
                    tracing::error!("Failed to initialize database for recording: {}", e);
                }
            }
        } else {
            state.is_recording = true;
            tracing::debug!(
                "Recording enabled - is_recording now: {}",
                state.is_recording
            );
        }
    }

    pub async fn initialize_database(
        record_enabled: bool,
        state: &mut AppState,
    ) -> Option<Database> {
        if record_enabled {
            match Database::new("irys-tui-records.db").await {
                Ok(db) => {
                    // Truncate existing records to start fresh
                    if let Err(e) = db.truncate_records().await {
                        tracing::warn!("Failed to truncate existing records: {}", e);
                    }
                    state.is_recording = true;
                    Some(db)
                }
                Err(e) => {
                    tracing::error!("Failed to initialize database: {}", e);
                    state.is_recording = false;
                    None
                }
            }
        } else {
            None
        }
    }
}
