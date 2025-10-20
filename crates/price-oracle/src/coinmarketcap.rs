use eyre::{Context as _, bail, eyre};
use irys_types::storage_pricing::{
    Amount,
    phantoms::{IrysPrice, Usd},
};
use reqwest::Client;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr as _;

#[derive(Debug, Clone)]
pub struct CoinMarketCapOracle {
    client: Client,
    api_key: String,
    symbol: String,
}

impl CoinMarketCapOracle {
    /// Create a new CoinMarketCap oracle client
    #[must_use]
    pub fn new(api_key: String, symbol: String) -> Self {
        let client = Client::new();
        Self {
            client,
            api_key,
            symbol,
        }
    }

    #[tracing::instrument(skip_all, err)]
    pub async fn current_price(&self) -> eyre::Result<Amount<(IrysPrice, Usd)>> {
        // Build headers
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            "X-CMC_PRO_API_KEY",
            HeaderValue::from_str(&self.api_key).context("invalid API key header value")?,
        );

        // Build query params
        let mut params: Vec<(String, String)> = Vec::new();
        params.push(("symbol".to_string(), self.symbol.clone()));
        params.push(("convert".to_string(), "USD".to_string()));

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

        let body: Value = resp.json().await.context("failed to parse CMC JSON body")?;

        // check status
        if let Some(status_obj) = body.get("status").and_then(|v| v.as_object()) {
            let error_code = status_obj
                .get("error_code")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            if error_code != 0 {
                let msg = status_obj
                    .get("error_message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                bail!("CoinMarketCap API error {}: {}", error_code, msg);
            }
        }

        let price_dec = extract_price_from_cmc(&body, "USD")
            .context("failed to extract price from CMC response")?;

        let amount = Amount::<(IrysPrice, Usd)>::token(price_dec)
            .context("failed to convert price to Amount")?;
        Ok(amount)
    }
}

#[tracing::instrument(skip_all, err)]
fn extract_price_from_cmc(v: &Value, convert: &str) -> eyre::Result<Decimal> {
    let data = v
        .get("data")
        .and_then(|d| d.as_object())
        .ok_or_else(|| eyre!("missing or invalid 'data' field"))?;

    // Normalize convert key (expect uppercase like "USD")
    let convert_key = if convert.is_empty() { "USD" } else { convert };

    for (_k, entry) in data.iter() {
        // Some responses wrap entry in an array when using symbol
        let items: Vec<&Value> = if let Some(arr) = entry.as_array() {
            arr.iter().collect()
        } else {
            vec![entry]
        };

        for item in items {
            if let Some(quote_obj) = item.get("quote").and_then(|q| q.as_object()) {
                if let Some(currency_obj) = quote_obj.get(convert_key).and_then(|c| c.as_object()) {
                    if let Some(price_val) = currency_obj.get("price") {
                        let decimal = match price_val {
                            Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
                            Value::String(s) => Decimal::from_str(s).ok(),
                            _ => None,
                        };
                        if let Some(d) = decimal {
                            return Ok(d);
                        }
                    }
                }
            }
        }
    }

    Err(eyre!("could not find price in CMC response"))
}

#[cfg(test)]
mod tests {
    use super::*;
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

        let v: Value = serde_json::from_str(sample).unwrap();
        let d = extract_price_from_cmc(&v, "USD").unwrap();
        assert_eq!(d, dec!(6602.60701122));
    }
}
