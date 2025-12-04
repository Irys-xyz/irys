//! CoinGecko oracle integration.
//!
//! API reference: https://docs.coingecko.com/reference/simple-price
//! Coin ids can be retrieved from https://docs.coingecko.com/reference/coins-list
//! Demo API key behaviour is documented at https://docs.coingecko.com/v3.0.1/reference/introduction

use eyre::{Context as _, bail, eyre};
use irys_types::UnixTimestamp;
use irys_types::storage_pricing::{
    Amount,
    phantoms::{IrysPrice, Usd},
};
use reqwest::Client;
use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};

#[derive(Debug, Clone)]
pub struct CoinGeckoOracle {
    client: Client,
    api_key: String,
    coin_id: String,
    url: Arc<String>,
}

impl CoinGeckoOracle {
    #[must_use]
    pub fn new(api_key: String, coin_id: String, use_demo_api: bool) -> Self {
        let client = Client::new();
        let base_url = if use_demo_api {
            "https://api.coingecko.com/api/v3"
        } else {
            "https://pro-api.coingecko.com/api/v3"
        };
        let url = format!("{base_url}/simple/price");

        Self {
            client,
            api_key,
            coin_id,
            url: Arc::new(url),
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub async fn current_price(&self) -> eyre::Result<CoinGeckoQuote> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let header_name = if self.url.starts_with("https://api.coingecko.com") {
            HeaderName::from_static("x-cg-demo-api-key")
        } else {
            HeaderName::from_static("x-cg-pro-api-key")
        };
        headers.insert(
            header_name,
            HeaderValue::from_str(&self.api_key).context("invalid API key header value")?,
        );

        let resp = self
            .client
            .get(&*self.url)
            .headers(headers)
            .query(&[
                ("ids", self.coin_id.clone()),
                ("vs_currencies", "usd".to_string()),
                ("include_last_updated_at", "true".to_string()),
            ])
            .send()
            .await
            .context("failed to call CoinGecko API")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "CoinGecko API returned HTTP status {} {} {}",
                self.url,
                status,
                body
            );
        }

        let body: HashMap<String, SimplePriceData> = resp
            .json()
            .await
            .context("failed to parse CoinGecko JSON body")?;

        let entry = body
            .get(&self.coin_id)
            .ok_or_else(|| eyre!("coin id {} missing in CoinGecko response", self.coin_id))?;

        let amount = Amount::<(IrysPrice, Usd)>::token(entry.usd)
            .context("failed to convert price to Amount")?;

        let last_updated = entry
            .last_updated_at
            .ok_or_else(|| eyre!("updated_at field missing"))?;
        let last_updated = UnixTimestamp::from_secs(last_updated);

        Ok(CoinGeckoQuote {
            amount,
            last_updated,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CoinGeckoQuote {
    /// Latest price converted into an `Amount`.
    pub amount: Amount<(IrysPrice, Usd)>,
    /// Timestamp reported by CoinGecko (seconds since UNIX epoch).
    pub last_updated: UnixTimestamp,
}

#[derive(Debug, Deserialize)]
struct SimplePriceData {
    #[serde(with = "rust_decimal::serde::float")]
    usd: Decimal,
    #[serde(default)]
    last_updated_at: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::collections::HashMap;

    #[test]
    fn parse_sample_response() {
        let sample = r#"{
  "bitcoin": {
    "usd": 67187.3358936566,
    "last_updated_at": 1711356300
  }
}"#;
        let parsed: HashMap<String, SimplePriceData> =
            serde_json::from_str(sample).expect("valid sample");
        let entry = parsed.get("bitcoin").unwrap();
        assert_eq!(entry.usd, dec!(67187.3358936566));
        assert_eq!(entry.last_updated_at, Some(1711356300));
        let amount = Amount::<(IrysPrice, Usd)>::token(entry.usd).unwrap();
        assert_eq!(amount.token_to_decimal().unwrap(), dec!(67187.3358936566));
    }
}
