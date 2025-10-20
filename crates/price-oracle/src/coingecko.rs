use eyre::{Context as _, bail, eyre};
use irys_types::storage_pricing::{
    Amount,
    phantoms::{IrysPrice, Usd},
};
use reqwest::Client;
use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue};
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr as _;

#[derive(Debug, Clone)]
pub struct CoinGeckoOracle {
    client: Client,
    api_key: String,
    coin_id: String,
}

impl CoinGeckoOracle {
    #[must_use]
    pub fn new(api_key: String, coin_id: String) -> Self {
        let client = Client::new();
        Self {
            client,
            api_key,
            coin_id,
        }
    }

    #[tracing::instrument(skip_all, err)]
    pub async fn current_price(&self) -> eyre::Result<Amount<(IrysPrice, Usd)>> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        // CoinGecko pro header is lowercase: x-cg-pro-api-key
        headers.insert(
            HeaderName::from_static("x-cg-pro-api-key"),
            HeaderValue::from_str(&self.api_key).context("invalid API key header value")?,
        );

        let resp = self
            .client
            .get("https://pro-api.coingecko.com/api/v3/simple/price")
            .headers(headers)
            .query(&[
                ("ids", self.coin_id.clone()),
                ("vs_currencies", "usd".to_string()),
            ])
            .send()
            .await
            .context("failed to call CoinGecko API")?;

        if !resp.status().is_success() {
            bail!("CoinGecko API returned HTTP status {}", resp.status());
        }

        let body: Value = resp
            .json()
            .await
            .context("failed to parse CoinGecko JSON body")?;

        let price_dec = extract_price_from_coingecko(&body, &self.coin_id)
            .context("failed to extract price from CoinGecko response")?;

        let amount = Amount::<(IrysPrice, Usd)>::token(price_dec)
            .context("failed to convert price to Amount")?;
        Ok(amount)
    }
}

#[tracing::instrument(skip_all, err)]
fn extract_price_from_coingecko(v: &Value, coin_id: &str) -> eyre::Result<Decimal> {
    let obj = v
        .as_object()
        .ok_or_else(|| eyre!("root is not an object"))?;
    let coin = obj
        .get(coin_id)
        .and_then(|x| x.as_object())
        .ok_or_else(|| eyre!("missing coin id in response"))?;
    let usd = coin.get("usd").ok_or_else(|| eyre!("missing usd field"))?;
    if let Some(n) = usd.as_f64() {
        if let Some(d) = Decimal::from_f64_retain(n) {
            return Ok(d);
        }
    }
    let s = usd.to_string();
    let d = Decimal::from_str(&s).context("failed to parse decimal")?;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn parse_sample_response() {
        let sample = r#"{
  "bitcoin": {
    "usd": 67187.3358936566,
    "usd_market_cap": 1317802988326.25,
    "usd_24h_vol": 31260929299.5248,
    "usd_24h_change": 3.63727894677354,
    "last_updated_at": 1711356300
  }
}"#;
        let v: Value = serde_json::from_str(sample).unwrap();
        let d = extract_price_from_coingecko(&v, "bitcoin").unwrap();
        assert_eq!(d, dec!(67187.3358936566));
    }
}
