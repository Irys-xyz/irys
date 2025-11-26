use chrono::{DateTime, Utc};
use eyre::{Context as _, bail, eyre};
use irys_types::storage_pricing::{
    Amount,
    phantoms::{IrysPrice, Usd},
};
use reqwest::Client;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr as _;
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct CoinMarketCapOracle {
    client: Client,
    api_key: String,
    id: String,
}

impl CoinMarketCapOracle {
    /// Create a new CoinMarketCap oracle client
    #[must_use]
    pub fn new(api_key: String, id: String) -> Self {
        let client = Client::new();
        Self {
            client,
            api_key,
            id,
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub async fn current_price(&self) -> eyre::Result<CoinMarketCapQuote> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            "X-CMC_PRO_API_KEY",
            HeaderValue::from_str(&self.api_key).context("invalid API key header value")?,
        );

        let params: Vec<(String, String)> = vec![
            ("id".to_string(), self.id.clone()),
            ("convert".to_string(), "USD".to_string()),
        ];

        let resp = self
            .client
            .get("https://pro-api.coinmarketcap.com/v2/cryptocurrency/quotes/latest")
            .headers(headers)
            .query(&params)
            .send()
            .await
            .context("failed to call CoinMarketCap API")?;

        if !resp.status().is_success() {
            bail!("CoinMarketCap API returned HTTP status {}", resp.status());
        }

        let body: CoinMarketCapResponse =
            resp.json().await.context("failed to parse CMC JSON body")?;

        if let Some(status) = &body.status
            && status.error_code != 0
        {
            let msg = status.error_message.as_deref().unwrap_or("unknown error");
            bail!("CoinMarketCap API error {}: {}", status.error_code, msg);
        }

        let quote = extract_quote_from_cmc(&body, "USD", &self.id)
            .context("failed to extract USD quote from CMC response")?;

        let amount = Amount::<(IrysPrice, Usd)>::token(quote.price)
            .context("failed to convert price to Amount")?;
        let last_updated = chrono_to_system_time(quote.last_updated);

        Ok(CoinMarketCapQuote {
            amount,
            last_updated,
        })
    }
}

#[derive(Debug, Clone)]
pub struct CoinMarketCapQuote {
    /// Latest price converted into an `Amount`.
    pub amount: Amount<(IrysPrice, Usd)>,
    /// Timestamp reported by CoinMarketCap.
    pub last_updated: SystemTime,
}

#[derive(Debug, Deserialize)]
struct CoinMarketCapResponse {
    status: Option<CoinMarketCapStatus>,
    data: HashMap<String, CmcAsset>,
}

#[derive(Debug, Deserialize)]
struct CoinMarketCapStatus {
    error_code: i64,
    error_message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CmcAsset {
    #[serde(default)]
    quote: HashMap<String, CmcQuoteCurrency>,
}

#[derive(Debug, Deserialize)]
struct CmcQuoteCurrency {
    #[serde(deserialize_with = "deserialize_decimal")]
    price: Decimal,
    #[serde(rename = "last_updated", deserialize_with = "deserialize_datetime")]
    last_updated: DateTime<Utc>,
}

#[derive(Debug)]
struct ExtractedQuote {
    price: Decimal,
    last_updated: DateTime<Utc>,
}

fn extract_quote_from_cmc(
    response: &CoinMarketCapResponse,
    convert: &str,
    id: &str,
) -> eyre::Result<ExtractedQuote> {
    let convert_key = if convert.is_empty() {
        "USD".to_string()
    } else {
        convert.to_uppercase()
    };

    let asset = response
        .data
        .get(id)
        .ok_or_else(|| eyre!("missing CoinMarketCap id {}", id))?;

    let quote = asset
        .quote
        .get(&convert_key)
        .ok_or_else(|| eyre!("missing quote for currency {}", convert_key))?;

    Ok(ExtractedQuote {
        price: quote.price,
        last_updated: quote.last_updated,
    })
}

fn chrono_to_system_time(dt: DateTime<Utc>) -> SystemTime {
    dt.into()
}

fn deserialize_decimal<'de, D>(deserializer: D) -> Result<Decimal, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Number(n) => Decimal::from_str(&n.to_string()).map_err(serde::de::Error::custom),
        Value::String(s) => Decimal::from_str(&s).map_err(serde::de::Error::custom),
        other => Err(serde::de::Error::custom(format!(
            "expected number or string for Decimal, found {other}"
        ))),
    }
}

fn deserialize_datetime<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    DateTime::parse_from_rfc3339(&s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use rust_decimal_macros::dec;

    #[test]
    fn parse_sample_response() {
        let sample = r#"{
"data": {
"1": {
"id": 1,
"name": "Bitcoin",
"symbol": "BTC",
"slug": "bitcoin",
"is_active": 1,
"is_fiat": 0,
"circulating_supply": 17199862,
"total_supply": 17199862,
"max_supply": 21000000,
"date_added": "2013-04-28T00:00:00.000Z",
"num_market_pairs": 331,
"cmc_rank": 1,
"last_updated": "2018-08-09T21:56:28.000Z",
"tags": [
"mineable"
],
"platform": null,
"self_reported_circulating_supply": null,
"self_reported_market_cap": null,
"quote": {
"USD": {
"price": 6602.60701122,
"volume_24h": 4314444687.5194,
"volume_change_24h": -0.152774,
"percent_change_1h": 0.988615,
"percent_change_24h": 4.37185,
"percent_change_7d": -12.1352,
"percent_change_30d": -12.1352,
"market_cap": 852164659250.2758,
"market_cap_dominance": 51,
"fully_diluted_market_cap": 952835089431.14,
"last_updated": "2018-08-09T21:56:28.000Z"
}
}
}
},
"status": {
"timestamp": "2025-10-13T00:00:34.078Z",
"error_code": 0,
"error_message": "",
"elapsed": 10,
"credit_count": 1,
"notice": ""
}
}"#;

        let response: CoinMarketCapResponse = serde_json::from_str(sample).unwrap();
        let quote =
            extract_quote_from_cmc(&response, "USD", "1").expect("quote should be available");
        assert_eq!(quote.price, dec!(6602.60701122));
        let expected_dt = DateTime::parse_from_rfc3339("2018-08-09T21:56:28.000Z")
            .unwrap()
            .with_timezone(&Utc);
        let expected = chrono_to_system_time(expected_dt);
        let actual = chrono_to_system_time(quote.last_updated);
        assert_eq!(actual, expected);
    }
}
