use super::types::ChunkInterval;
use eyre::Result;
use std::net::{SocketAddr, ToSocketAddrs as _};

pub(crate) fn generate_test_data(size: usize) -> Vec<u8> {
    let mut data = vec![0u8; size];
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = (i % 256) as u8;
    }
    data
}

pub(crate) fn count_chunks_in_intervals(intervals: &[ChunkInterval]) -> usize {
    intervals
        .iter()
        .map(|i| (i.end - i.start + 1) as usize)
        .sum()
}

pub(crate) fn parse_url_to_socket_addr(url_str: &str) -> Result<SocketAddr> {
    let url_str = url_str.trim_end_matches('/');

    // Parse URL to extract host and port
    if let Some(host_port) = url_str.strip_prefix("http://") {
        parse_host_port(host_port)
    } else if let Some(host_port) = url_str.strip_prefix("https://") {
        parse_host_port(host_port)
    } else {
        parse_host_port(url_str)
    }
}

fn parse_host_port(host_port: &str) -> Result<SocketAddr> {
    // Try to parse as socket address directly
    if let Ok(addr) = host_port.parse::<SocketAddr>() {
        return Ok(addr);
    }

    // Try to resolve hostname
    let addrs: Vec<SocketAddr> = host_port.to_socket_addrs()?.collect();
    addrs
        .into_iter()
        .next()
        .ok_or_else(|| eyre::eyre!("Could not resolve address"))
}

pub(crate) fn parse_node_urls() -> Result<Vec<String>> {
    let node_urls = std::env::var("NODE_URLS")
        .expect("NODE_URLS environment variable must be set (comma-separated list)");

    let urls: Vec<String> = node_urls
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if urls.is_empty() {
        return Err(eyre::eyre!("No node URLs provided"));
    }

    Ok(urls)
}

pub(crate) fn get_env_duration(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .unwrap_or_else(|_| default.to_string())
        .parse::<u64>()
        .unwrap_or(default)
}

#[expect(dead_code)]
pub(crate) fn get_env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .unwrap_or_else(|_| default.to_string())
        .parse::<usize>()
        .unwrap_or(default)
}

pub(crate) fn get_env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .unwrap_or_else(|_| default.to_string())
        .parse::<bool>()
        .unwrap_or(default)
}

pub(crate) fn create_consensus_config_from_response(
    resp: &super::types::NetworkConfigResponse,
) -> irys_types::ConsensusConfig {
    // Use testing config as base and override with response values
    let mut config = irys_types::NodeConfig::testing().consensus_config();
    config.chain_id = resp.chain_id.parse().unwrap_or(config.chain_id);
    config.chunk_size = resp.chunk_size.parse().unwrap_or(config.chunk_size);
    config.num_partitions_per_slot = resp
        .num_partitions_per_slot
        .parse()
        .unwrap_or(config.num_partitions_per_slot);
    config.num_chunks_in_partition = resp
        .num_chunks_in_partition
        .parse()
        .unwrap_or(config.num_chunks_in_partition);
    config.num_chunks_in_recall_range = resp
        .num_chunks_in_recall_range
        .parse()
        .unwrap_or(config.num_chunks_in_recall_range);
    config.entropy_packing_iterations = resp.entropy_packing_iterations;
    config
}
