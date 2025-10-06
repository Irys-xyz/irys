use chrono::{DateTime, Utc};
use eyre::{Context as _, Result};
use sqlx::sqlite::SqlitePool;
use std::path::Path;

use crate::api::models::{MempoolStatus, MiningInfo, NodeInfo};

pub struct Database {
    pool: SqlitePool,
}

impl Database {
    pub async fn new(db_path: &str) -> Result<Self> {
        if !Path::new(db_path).exists() {
            std::fs::File::create(db_path).context("Failed to create database file")?;
        }

        let connection_string = format!("sqlite://{}?mode=rwc&cache=shared", db_path);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(5)
            .min_connections(1)
            .connect(&connection_string)
            .await
            .context("Failed to connect to SQLite database")?;

        let db = Self { pool };
        db.initialize_schema().await?;
        db.optimize_database().await?;
        Ok(db)
    }

    async fn optimize_database(&self) -> Result<()> {
        // SQLite optimization profile for TUI recording workload:
        //
        // WAL mode: Allows concurrent reads during writes (critical for UI responsiveness)
        // NORMAL sync: 2x faster writes vs FULL, still crash-safe (not power-loss safe)
        // 10MB cache: Reduces I/O for typical recording sessions (100+ nodes)
        // 256MB mmap: Leverages OS page cache for read-heavy workload

        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&self.pool)
            .await
            .context("Failed to enable WAL mode")?;

        sqlx::query("PRAGMA synchronous = NORMAL")
            .execute(&self.pool)
            .await
            .context("Failed to set synchronous mode")?;

        sqlx::query("PRAGMA cache_size = -10000")
            .execute(&self.pool)
            .await
            .context("Failed to set cache size")?;

        sqlx::query("PRAGMA mmap_size = 268435456")
            .execute(&self.pool)
            .await
            .context("Failed to set mmap size")?;

        Ok(())
    }

    async fn initialize_schema(&self) -> Result<()> {
        sqlx::query(
            "
            CREATE TABLE IF NOT EXISTS node_info_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp DATETIME NOT NULL,
                node_url TEXT NOT NULL,
                node_type TEXT,
                height TEXT,
                block_hash TEXT,
                block_index_height TEXT,
                block_index_hash TEXT,
                pending_blocks TEXT,
                peer_count INTEGER,
                current_sync_height INTEGER,
                raw_json TEXT
            )
            ",
        )
        .execute(&self.pool)
        .await
        .context("Failed to create node_info_records table")?;

        sqlx::query(
            "
            CREATE TABLE IF NOT EXISTS mempool_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp DATETIME NOT NULL,
                node_url TEXT NOT NULL,
                data_tx_count INTEGER,
                commitment_tx_count INTEGER,
                pending_chunks_count INTEGER,
                pending_pledges_count INTEGER,
                recent_valid_tx_count INTEGER,
                recent_invalid_tx_count INTEGER,
                data_tx_total_size INTEGER,
                raw_json TEXT
            )
            ",
        )
        .execute(&self.pool)
        .await
        .context("Failed to create mempool_records table")?;

        sqlx::query(
            "
            CREATE TABLE IF NOT EXISTS mining_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp DATETIME NOT NULL,
                node_url TEXT NOT NULL,
                block_height INTEGER,
                block_hash TEXT,
                block_timestamp INTEGER,
                current_difficulty TEXT,
                cumulative_difficulty TEXT,
                last_diff_adjustment_timestamp INTEGER,
                hashrate_1h REAL,
                hashrate_24h REAL,
                hashrate_7d REAL,
                raw_json TEXT
            )
            ",
        )
        .execute(&self.pool)
        .await
        .context("Failed to create mining_records table")?;

        sqlx::query(
            "
            CREATE TABLE IF NOT EXISTS fork_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp DATETIME NOT NULL,
                node_url TEXT NOT NULL,
                current_tip_height INTEGER,
                current_tip_hash TEXT,
                fork_count INTEGER,
                max_fork_depth INTEGER,
                total_forked_blocks INTEGER,
                raw_json TEXT
            )
            ",
        )
        .execute(&self.pool)
        .await
        .context("Failed to create fork_records table")?;

        sqlx::query(
            "
            CREATE TABLE IF NOT EXISTS data_sync_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp DATETIME NOT NULL,
                node_url TEXT NOT NULL,
                sync_height INTEGER,
                total_data_chunks INTEGER,
                total_packed_chunks INTEGER,
                sync_progress REAL,
                sync_speed_mbps REAL,
                raw_json TEXT
            )
            ",
        )
        .execute(&self.pool)
        .await
        .context("Failed to create data_sync_records table")?;

        sqlx::query(
            "
            CREATE TABLE IF NOT EXISTS metrics_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp DATETIME NOT NULL,
                node_url TEXT NOT NULL,
                uptime_percentage REAL,
                avg_response_time_ms INTEGER,
                error_count INTEGER,
                total_chunks INTEGER,
                storage_used_gb REAL,
                raw_json TEXT
            )
            ",
        )
        .execute(&self.pool)
        .await
        .context("Failed to create metrics_records table")?;

        let tables = vec![
            "node_info_records",
            "mempool_records",
            "mining_records",
            "fork_records",
            "data_sync_records",
            "metrics_records",
        ];

        for table in tables {
            let query = format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_timestamp ON {} (timestamp)",
                table, table
            );
            sqlx::query(&query)
                .execute(&self.pool)
                .await
                .with_context(|| format!("Failed to create timestamp index for {}", table))?;

            let query = format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_node_url ON {} (node_url)",
                table, table
            );
            sqlx::query(&query)
                .execute(&self.pool)
                .await
                .with_context(|| format!("Failed to create node_url index for {}", table))?;
        }

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_height ON node_info_records (height)")
            .execute(&self.pool)
            .await
            .context("Failed to create height index")?;

        Ok(())
    }

    pub async fn record_node_info(
        &self,
        node_url: &str,
        info: &NodeInfo,
        raw_json: &str,
    ) -> Result<()> {
        let timestamp = Utc::now();

        sqlx::query(
            "
            INSERT INTO node_info_records (
                timestamp, node_url, node_type, height, block_hash,
                block_index_height, block_index_hash, pending_blocks,
                peer_count, current_sync_height, raw_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(timestamp)
        .bind(node_url)
        .bind(None::<String>) // node_type is not available in NodeInfo
        .bind(&info.height)
        .bind(&info.block_hash)
        .bind(&info.block_index_height)
        .bind(&info.block_index_hash)
        .bind(&info.pending_blocks)
        .bind(info.peer_count as i64)
        .bind(info.current_sync_height as i64)
        .bind(raw_json)
        .execute(&self.pool)
        .await
        .context("Failed to insert node info record")?;

        Ok(())
    }

    pub async fn record_mempool_status(
        &self,
        node_url: &str,
        status: &MempoolStatus,
        raw_json: &str,
    ) -> Result<()> {
        let timestamp = Utc::now();

        sqlx::query(
            "
            INSERT INTO mempool_records (
                timestamp, node_url, data_tx_count, commitment_tx_count,
                pending_chunks_count, pending_pledges_count, recent_valid_tx_count,
                recent_invalid_tx_count, data_tx_total_size, raw_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(timestamp)
        .bind(node_url)
        .bind(status.data_tx_count as i64)
        .bind(status.commitment_tx_count as i64)
        .bind(status.pending_chunks_count as i64)
        .bind(status.pending_pledges_count as i64)
        .bind(status.recent_valid_tx_count as i64)
        .bind(status.recent_invalid_tx_count as i64)
        .bind(status.data_tx_total_size as i64)
        .bind(raw_json)
        .execute(&self.pool)
        .await
        .context("Failed to insert mempool record")?;

        Ok(())
    }

    pub async fn record_mining_info(
        &self,
        node_url: &str,
        info: &MiningInfo,
        raw_json: &str,
    ) -> Result<()> {
        let timestamp = Utc::now();

        sqlx::query(
            "
            INSERT INTO mining_records (
                timestamp, node_url, block_height, block_hash, block_timestamp,
                current_difficulty, cumulative_difficulty, last_diff_adjustment_timestamp,
                hashrate_1h, hashrate_24h, hashrate_7d, raw_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(timestamp)
        .bind(node_url)
        .bind(info.block_height as i64)
        .bind(&info.block_hash)
        .bind(info.block_timestamp as i64)
        .bind(&info.current_difficulty)
        .bind(&info.cumulative_difficulty)
        .bind(info.last_diff_adjustment_timestamp as i64)
        .bind(0.0_f64) // hashrate_1h - not available in current API
        .bind(0.0_f64) // hashrate_24h - not available in current API
        .bind(0.0_f64) // hashrate_7d - not available in current API
        .bind(raw_json)
        .execute(&self.pool)
        .await
        .context("Failed to insert mining record")?;

        Ok(())
    }

    pub async fn record_fork_data(
        &self,
        node_url: &str,
        current_tip_height: u64,
        current_tip_hash: &str,
        fork_count: usize,
        max_fork_depth: u64,
        total_forked_blocks: usize,
        raw_json: &str,
    ) -> Result<()> {
        let timestamp = Utc::now();

        sqlx::query(
            "
            INSERT INTO fork_records (
                timestamp, node_url, current_tip_height, current_tip_hash,
                fork_count, max_fork_depth, total_forked_blocks, raw_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(timestamp)
        .bind(node_url)
        .bind(current_tip_height as i64)
        .bind(current_tip_hash)
        .bind(fork_count as i64)
        .bind(max_fork_depth as i64)
        .bind(total_forked_blocks as i64)
        .bind(raw_json)
        .execute(&self.pool)
        .await
        .context("Failed to insert fork record")?;

        Ok(())
    }

    pub async fn record_data_sync(
        &self,
        node_url: &str,
        sync_height: u64,
        total_data_chunks: u64,
        total_packed_chunks: u64,
        sync_progress: f64,
        sync_speed_mbps: f64,
        raw_json: &str,
    ) -> Result<()> {
        let timestamp = Utc::now();

        sqlx::query(
            "
            INSERT INTO data_sync_records (
                timestamp, node_url, sync_height, total_data_chunks,
                total_packed_chunks, sync_progress, sync_speed_mbps, raw_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(timestamp)
        .bind(node_url)
        .bind(sync_height as i64)
        .bind(total_data_chunks as i64)
        .bind(total_packed_chunks as i64)
        .bind(sync_progress)
        .bind(sync_speed_mbps)
        .bind(raw_json)
        .execute(&self.pool)
        .await
        .context("Failed to insert data sync record")?;

        Ok(())
    }

    pub async fn record_metrics(
        &self,
        node_url: &str,
        uptime_percentage: f64,
        avg_response_time_ms: u64,
        error_count: u32,
        total_chunks: u64,
        storage_used_gb: f64,
        raw_json: &str,
    ) -> Result<()> {
        let timestamp = Utc::now();

        sqlx::query(
            "
            INSERT INTO metrics_records (
                timestamp, node_url, uptime_percentage, avg_response_time_ms,
                error_count, total_chunks, storage_used_gb, raw_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(timestamp)
        .bind(node_url)
        .bind(uptime_percentage)
        .bind(avg_response_time_ms as i64)
        .bind(error_count as i64)
        .bind(total_chunks as i64)
        .bind(storage_used_gb)
        .bind(raw_json)
        .execute(&self.pool)
        .await
        .context("Failed to insert metrics record")?;

        Ok(())
    }

    pub async fn get_latest_records(&self, limit: usize) -> Result<Vec<NodeInfoRecord>> {
        let records = sqlx::query_as::<_, NodeInfoRecord>(
            "
            SELECT * FROM node_info_records
            ORDER BY timestamp DESC
            LIMIT ?
            ",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("Failed to fetch latest records")?;

        Ok(records)
    }

    pub async fn get_records_for_node(
        &self,
        node_url: &str,
        limit: usize,
    ) -> Result<Vec<NodeInfoRecord>> {
        let records = sqlx::query_as::<_, NodeInfoRecord>(
            "
            SELECT * FROM node_info_records
            WHERE node_url = ?
            ORDER BY timestamp DESC
            LIMIT ?
            ",
        )
        .bind(node_url)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("Failed to fetch records for node")?;

        Ok(records)
    }

    pub async fn truncate_records(&self) -> Result<()> {
        sqlx::query("DELETE FROM node_info_records")
            .execute(&self.pool)
            .await
            .context("Failed to truncate node_info_records table")?;

        // Reset the autoincrement counter
        sqlx::query("DELETE FROM sqlite_sequence WHERE name='node_info_records'")
            .execute(&self.pool)
            .await
            .context("Failed to reset autoincrement counter")?;

        Ok(())
    }

    pub async fn close(self) -> Result<()> {
        self.pool.close().await;
        Ok(())
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NodeInfoRecord {
    pub id: i64,
    pub timestamp: DateTime<Utc>,
    pub node_url: String,
    pub node_type: Option<String>,
    pub height: Option<String>,
    pub block_hash: Option<String>,
    pub block_index_height: Option<String>,
    pub block_index_hash: Option<String>,
    pub pending_blocks: Option<String>,
    pub peer_count: Option<i64>,
    pub current_sync_height: Option<i64>,
    pub raw_json: Option<String>,
}
