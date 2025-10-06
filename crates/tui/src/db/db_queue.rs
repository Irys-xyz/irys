use crate::api::models::{MempoolStatus, MiningInfo, NodeInfo};
use crate::db::db::Database;
use eyre::Result;
use tokio::sync::mpsc;

pub enum DbCommand {
    RecordNodeInfo {
        node_url: String,
        info: NodeInfo,
        raw_json: String,
    },
    RecordMempoolStatus {
        node_url: String,
        status: MempoolStatus,
        raw_json: String,
    },
    RecordMiningInfo {
        node_url: String,
        info: MiningInfo,
        raw_json: String,
    },
    RecordForkData {
        node_url: String,
        current_tip_height: u64,
        current_tip_hash: String,
        fork_count: usize,
        max_fork_depth: u64,
        total_forked_blocks: usize,
        raw_json: String,
    },
    RecordDataSync {
        node_url: String,
        sync_height: u64,
        total_data_chunks: u64,
        total_packed_chunks: u64,
        sync_progress: f64,
        sync_speed_mbps: f64,
        raw_json: String,
    },
    RecordMetrics {
        node_url: String,
        uptime_percentage: f64,
        avg_response_time_ms: u64,
        error_count: u32,
        total_chunks: u64,
        storage_used_gb: f64,
        raw_json: String,
    },
    Shutdown,
}

pub struct DatabaseWriter {
    command_tx: mpsc::UnboundedSender<DbCommand>,
    writer_handle: Option<tokio::task::JoinHandle<()>>,
}

impl DatabaseWriter {
    pub fn new(db: Database) -> Self {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();

        let writer_handle = tokio::spawn(async move {
            while let Some(cmd) = command_rx.recv().await {
                match cmd {
                    DbCommand::RecordNodeInfo {
                        node_url,
                        info,
                        raw_json,
                    } => {
                        if let Err(e) = db.record_node_info(&node_url, &info, &raw_json).await {
                            tracing::error!("Failed to record node info: {}", e);
                        }
                    }
                    DbCommand::RecordMempoolStatus {
                        node_url,
                        status,
                        raw_json,
                    } => {
                        if let Err(e) = db
                            .record_mempool_status(&node_url, &status, &raw_json)
                            .await
                        {
                            tracing::error!("Failed to record mempool status: {}", e);
                        }
                    }
                    DbCommand::RecordMiningInfo {
                        node_url,
                        info,
                        raw_json,
                    } => {
                        if let Err(e) = db.record_mining_info(&node_url, &info, &raw_json).await {
                            tracing::error!("Failed to record mining info: {}", e);
                        }
                    }
                    DbCommand::RecordForkData {
                        node_url,
                        current_tip_height,
                        current_tip_hash,
                        fork_count,
                        max_fork_depth,
                        total_forked_blocks,
                        raw_json,
                    } => {
                        if let Err(e) = db
                            .record_fork_data(
                                &node_url,
                                current_tip_height,
                                &current_tip_hash,
                                fork_count,
                                max_fork_depth,
                                total_forked_blocks,
                                &raw_json,
                            )
                            .await
                        {
                            tracing::error!("Failed to record fork data: {}", e);
                        }
                    }
                    DbCommand::RecordDataSync {
                        node_url,
                        sync_height,
                        total_data_chunks,
                        total_packed_chunks,
                        sync_progress,
                        sync_speed_mbps,
                        raw_json,
                    } => {
                        if let Err(e) = db
                            .record_data_sync(
                                &node_url,
                                sync_height,
                                total_data_chunks,
                                total_packed_chunks,
                                sync_progress,
                                sync_speed_mbps,
                                &raw_json,
                            )
                            .await
                        {
                            tracing::error!("Failed to record data sync: {}", e);
                        }
                    }
                    DbCommand::RecordMetrics {
                        node_url,
                        uptime_percentage,
                        avg_response_time_ms,
                        error_count,
                        total_chunks,
                        storage_used_gb,
                        raw_json,
                    } => {
                        if let Err(e) = db
                            .record_metrics(
                                &node_url,
                                uptime_percentage,
                                avg_response_time_ms,
                                error_count,
                                total_chunks,
                                storage_used_gb,
                                &raw_json,
                            )
                            .await
                        {
                            tracing::error!("Failed to record metrics: {}", e);
                        }
                    }
                    DbCommand::Shutdown => break,
                }
            }

            if let Err(e) = db.close().await {
                tracing::error!("Failed to close database: {}", e);
            }
        });

        Self {
            command_tx,
            writer_handle: Some(writer_handle),
        }
    }

    pub fn record_node_info(
        &self,
        node_url: String,
        info: NodeInfo,
        raw_json: String,
    ) -> Result<()> {
        self.command_tx
            .send(DbCommand::RecordNodeInfo {
                node_url,
                info,
                raw_json,
            })
            .map_err(|_| eyre::eyre!("Database writer task has terminated"))?;
        Ok(())
    }

    pub fn record_mempool_status(
        &self,
        node_url: String,
        status: MempoolStatus,
        raw_json: String,
    ) -> Result<()> {
        self.command_tx
            .send(DbCommand::RecordMempoolStatus {
                node_url,
                status,
                raw_json,
            })
            .map_err(|_| eyre::eyre!("Database writer task has terminated"))?;
        Ok(())
    }

    pub fn record_mining_info(
        &self,
        node_url: String,
        info: MiningInfo,
        raw_json: String,
    ) -> Result<()> {
        self.command_tx
            .send(DbCommand::RecordMiningInfo {
                node_url,
                info,
                raw_json,
            })
            .map_err(|_| eyre::eyre!("Database writer task has terminated"))?;
        Ok(())
    }

    pub fn record_fork_data(
        &self,
        node_url: String,
        current_tip_height: u64,
        current_tip_hash: String,
        fork_count: usize,
        max_fork_depth: u64,
        total_forked_blocks: usize,
        raw_json: String,
    ) -> Result<()> {
        self.command_tx
            .send(DbCommand::RecordForkData {
                node_url,
                current_tip_height,
                current_tip_hash,
                fork_count,
                max_fork_depth,
                total_forked_blocks,
                raw_json,
            })
            .map_err(|_| eyre::eyre!("Database writer task has terminated"))?;
        Ok(())
    }

    pub fn record_data_sync(
        &self,
        node_url: String,
        sync_height: u64,
        total_data_chunks: u64,
        total_packed_chunks: u64,
        sync_progress: f64,
        sync_speed_mbps: f64,
        raw_json: String,
    ) -> Result<()> {
        self.command_tx
            .send(DbCommand::RecordDataSync {
                node_url,
                sync_height,
                total_data_chunks,
                total_packed_chunks,
                sync_progress,
                sync_speed_mbps,
                raw_json,
            })
            .map_err(|_| eyre::eyre!("Database writer task has terminated"))?;
        Ok(())
    }

    pub fn record_metrics(
        &self,
        node_url: String,
        uptime_percentage: f64,
        avg_response_time_ms: u64,
        error_count: u32,
        total_chunks: u64,
        storage_used_gb: f64,
        raw_json: String,
    ) -> Result<()> {
        self.command_tx
            .send(DbCommand::RecordMetrics {
                node_url,
                uptime_percentage,
                avg_response_time_ms,
                error_count,
                total_chunks,
                storage_used_gb,
                raw_json,
            })
            .map_err(|_| eyre::eyre!("Database writer task has terminated"))?;
        Ok(())
    }

    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.command_tx.send(DbCommand::Shutdown);

        if let Some(handle) = self.writer_handle.take() {
            handle.await?;
        }

        Ok(())
    }
}
