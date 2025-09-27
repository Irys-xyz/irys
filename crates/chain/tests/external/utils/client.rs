use super::utils::parse_url_to_socket_addr;
use eyre::Result;
use irys_api_client::IrysApiClient;
use reqwest::Client;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, Clone)]
pub(crate) struct RemoteNodeClient {
    pub url: String,
    pub http_client: Client,
    pub api_client: IrysApiClient,
}

impl RemoteNodeClient {
    pub(crate) fn new(url: String, http_client: Client) -> Result<Self> {
        // Validate URL is parseable
        let _ = parse_url_to_socket_addr(&url)?;

        Ok(Self {
            url,
            http_client,
            api_client: IrysApiClient::new(),
        })
    }

    pub(crate) async fn is_ready(&self) -> bool {
        self.http_client
            .get(format!("{}/v1/genesis", self.url))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    pub(crate) fn socket_addr(&self) -> Result<SocketAddr> {
        parse_url_to_socket_addr(&self.url)
    }
}

pub(crate) fn make_http_client() -> Result<Client> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(30))
        .build()?)
}
