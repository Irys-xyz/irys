use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;

/// Manages a pool of HTTP clients with different configurations
#[derive(Debug, Clone)]
pub(super) struct HttpClientPool {
    clients: Arc<DashMap<String, reqwest::Client>>,
}

impl HttpClientPool {
    /// Create a new HTTP client pool
    pub(super) fn new() -> Self {
        Self {
            clients: Arc::new(DashMap::new()),
        }
    }

    /// Get or create a reusable HTTP client for the given URL and timeout
    pub(super) fn get_or_create_client(
        &self,
        url: &str,
        timeout: Option<&Duration>,
    ) -> reqwest::Client {
        let cache_key = match timeout {
            Some(t) => format!("{}:{}", url, t.as_millis()),
            None => url.to_string(),
        };

        self.clients
            .entry(cache_key)
            .or_insert_with(|| Self::build_client(timeout))
            .clone()
    }

    /// Build a new HTTP client with the specified configuration
    fn build_client(timeout: Option<&Duration>) -> reqwest::Client {
        let mut builder = reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90));

        if let Some(t) = timeout {
            builder = builder.connect_timeout(*t).read_timeout(*t);
        }

        builder.build().unwrap_or_else(|e| {
            tracing::warn!(
                target: "irys::packing::client_pool",
                error = %e,
                "Failed to build custom reqwest client, falling back to default"
            );
            reqwest::Client::new()
        })
    }
}
