//! Data-transaction submission helpers used by the upgrade and e2e tests to
//! prove a cluster is *actually* functional — not just that nodes booted.
//!
//! The harness speaks plain HTTP against `/v1/anchor`, `/v1/price`, `/v1/tx`,
//! and `/v1/tx/{id}`. It uses HEAD's `irys-types` for signing because we need
//! a working signer somewhere; the on-the-wire shape of `DataTransactionHeader`
//! is what determines OLD/NEW compatibility, and any drift there is a real
//! v1 wire-compat bug — exactly what these tests exist to flush out.

use crate::run_config::SchemaConfig;
use irys_types::{BoundedFee, DataLedger, DataTransaction, H256, U256, irys::IrysSigner};
use k256::ecdsa::SigningKey;
use serde::Deserialize;
use std::time::Duration;
use tokio::time::{Instant, sleep};

/// Pre-funded test signing key. Its address (`0x64f1a282…`) is in
/// `consensus.Custom.reth.alloc` for both OLD and NEW testing configs, so we
/// can pay the term + perm fees on every test cluster without setup.
pub const DEV_PRIVATE_KEY_HEX: &str =
    "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";

const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum DataTxError {
    #[error("HTTP error talking to {url}: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("hex decode error: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("invalid signing key")]
    InvalidKey,
    #[error("transaction construction failed: {0}")]
    TxBuild(String),
    #[error("/v1/tx rejected the transaction at {url} (status={status}): {body}")]
    Rejected {
        url: String,
        status: u16,
        body: String,
    },
    #[error(
        "tx {tx_id:?} did not become visible at {url} within {timeout:?}; last status={last_status:?}"
    )]
    Timeout {
        tx_id: H256,
        url: String,
        timeout: Duration,
        last_status: Option<u16>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnchorWire {
    block_hash: H256,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PriceWire {
    perm_fee: U256,
    term_fee: U256,
}

/// Builds an [`IrysSigner`] from the funded dev key for the given chain.
pub fn dev_signer(chain_id: u64, chunk_size: u64) -> Result<IrysSigner, DataTxError> {
    let bytes = hex::decode(DEV_PRIVATE_KEY_HEX)?;
    let signer = SigningKey::from_slice(&bytes).map_err(|_| DataTxError::InvalidKey)?;
    Ok(IrysSigner {
        signer,
        chain_id,
        chunk_size,
    })
}

/// POSTs a signed Publish-ledger data transaction to `api_url` and returns
/// the resulting [`DataTransaction`] (whose `header.id` is the assigned tx_id).
///
/// Steps mirror what `crates/chain-tests/src/utils.rs::post_data_tx` does in
/// integration tests, except we hit the HTTP API rather than the actor pool —
/// the harness has no in-process node handle.
pub async fn submit_data_tx(
    client: &reqwest::Client,
    api_url: &str,
    signer: &IrysSigner,
    data: Vec<u8>,
    tx_build: &crate::run_config::TxBuildConfig,
) -> Result<DataTransaction, DataTxError> {
    let data_size = data.len() as u64;

    let anchor_url = format!("{api_url}/v1/anchor");
    let anchor: AnchorWire = client
        .get(&anchor_url)
        .send()
        .await
        .map_err(|e| DataTxError::Http {
            url: anchor_url.clone(),
            source: e,
        })?
        .error_for_status()
        .map_err(|e| DataTxError::Http {
            url: anchor_url.clone(),
            source: e,
        })?
        .json()
        .await
        .map_err(|e| DataTxError::Http {
            url: anchor_url,
            source: e,
        })?;

    let price_url = format!(
        "{api_url}/v1/price/{}/{data_size}",
        DataLedger::Publish as u32
    );
    let price: PriceWire = client
        .get(&price_url)
        .send()
        .await
        .map_err(|e| DataTxError::Http {
            url: price_url.clone(),
            source: e,
        })?
        .error_for_status()
        .map_err(|e| DataTxError::Http {
            url: price_url.clone(),
            source: e,
        })?
        .json()
        .await
        .map_err(|e| DataTxError::Http {
            url: price_url,
            source: e,
        })?;

    let perm_fee = BoundedFee::new(price.perm_fee);
    let term_fee = BoundedFee::new(price.term_fee);

    let mut tx = signer
        .create_publish_transaction(data, anchor.block_hash, perm_fee, term_fee)
        .map_err(|e| DataTxError::TxBuild(e.to_string()))?;
    // Force normally-default fields to non-default sentinels before
    // signing so the on-disk `Compact` encoding actually exercises
    // non-zero payload bytes. Without this, fields whose default-value
    // encoding is a zero-byte payload trivially survive any schema
    // change because both sides emit the same zero bytes — making the
    // strict round-trip check partly vacuous.
    //
    // Some version pairs straddle a schema rename or retype where a
    // non-default value would change the canonical signature prehash
    // on one side. Those fields can be opted-out via the run config's
    // `tx_build.keep_default` list — see [`crate::run_config`].
    if !tx_build.keep_default.iter().any(|f| f == "metadata_format") {
        tx.header.metadata_format = 1;
    }
    if !tx_build.keep_default.iter().any(|f| f == "header_size") {
        tx.header.header_size = 64;
    }
    let tx = signer
        .sign_transaction(tx)
        .map_err(|e| DataTxError::TxBuild(e.to_string()))?;

    let post_url = format!("{api_url}/v1/tx");
    let response = client
        .post(&post_url)
        .json(&tx.header)
        .send()
        .await
        .map_err(|e| DataTxError::Http {
            url: post_url.clone(),
            source: e,
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(DataTxError::Rejected {
            url: post_url,
            status: status.as_u16(),
            body,
        });
    }

    Ok(tx)
}

/// POSTs every chunk of a `DataTransaction` to `/v1/chunk`. Promotion of
/// a Publish-ledger tx requires the chunks to be available so storage
/// nodes can generate ingress proofs — without this step a tx will sit
/// in the Submit ledger forever and never get a `promoted_height`.
///
/// The `/v1/chunk` route accepts an [`irys_types::UnpackedChunk`] body
/// in both OLD and NEW (same wire shape), so we don't need a per-version
/// branch here.
pub async fn upload_chunks_for_tx(
    client: &reqwest::Client,
    api_url: &str,
    tx: &DataTransaction,
) -> Result<(), DataTxError> {
    let chunks = tx
        .data_chunks()
        .map_err(|e| DataTxError::TxBuild(format!("data_chunks(): {e}")))?;
    let post_url = format!("{api_url}/v1/chunk");
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let response = client
            .post(&post_url)
            .json(&chunk)
            .send()
            .await
            .map_err(|e| DataTxError::Http {
                url: post_url.clone(),
                source: e,
            })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(DataTxError::Rejected {
                url: format!("{post_url} (chunk {idx})"),
                status: status.as_u16(),
                body,
            });
        }
    }
    Ok(())
}

/// Polls `/v1/tx/{tx_id}/promotion-status` on `api_url` until the node
/// reports a `promotion_height` (i.e. the tx has been promoted from the
/// Submit ledger to the Publish ledger). Returns the promoted height.
///
/// Promotion requires:
///   * the tx's chunks to be uploaded ([`upload_chunks_for_tx`])
///   * a storage node that holds the partition the tx lands in to
///     produce an ingress proof
///   * enough subsequent blocks to commit the promotion
///
/// `/v1/tx/{tx_id}/promotion-status` exists on both OLD (d071fc03) and
/// NEW with the same response shape (`{ promotion_height: Option<u64> }`),
/// so this works across versions without a per-version branch.
pub async fn wait_for_promotion(
    client: &reqwest::Client,
    api_url: &str,
    tx_id: H256,
    timeout: Duration,
) -> Result<u64, DataTxError> {
    let url = format!("{api_url}/v1/tx/{tx_id}/promotion-status");
    let start = Instant::now();
    let mut last_status: Option<u16> = None;

    loop {
        match client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    let body: serde_json::Value =
                        resp.json().await.map_err(|e| DataTxError::Http {
                            url: url.clone(),
                            source: e,
                        })?;
                    if let Some(height) = body
                        .get("promotion_height")
                        .or_else(|| body.get("promotionHeight"))
                        .and_then(|v| match v {
                            serde_json::Value::Number(n) => n.as_u64(),
                            serde_json::Value::String(s) => s.parse::<u64>().ok(),
                            _ => None,
                        })
                    {
                        return Ok(height);
                    }
                    // Endpoint OK but not yet promoted — keep polling.
                } else {
                    last_status = Some(status.as_u16());
                }
            }
            Err(e) => {
                tracing::trace!(url = %url, error = %e, "promotion poll: request error");
            }
        }
        if start.elapsed() >= timeout {
            return Err(DataTxError::Timeout {
                tx_id,
                url,
                timeout,
                last_status,
            });
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// Polls `/v1/tx/{tx_id}` on `api_url` until the node responds 2xx. Used to
/// assert a transaction propagated to a specific node — call once per node
/// to verify cluster-wide visibility.
///
/// **NOTE**: this only proves the node admitted the tx_id exists. It does
/// *not* prove the node returns *correct* data — a node serving up garbage
/// (e.g. an older binary mis-decoding records that a newer binary wrote)
/// can still answer 2xx with a JSON body whose contents are wrong. Use
/// [`assert_tx_matches_original`] when you need to catch silent corruption
/// across version transitions.
pub async fn wait_for_tx_visible(
    client: &reqwest::Client,
    api_url: &str,
    tx_id: H256,
    timeout: Duration,
) -> Result<(), DataTxError> {
    let url = format!("{api_url}/v1/tx/{tx_id}");
    let start = Instant::now();
    let mut last_status: Option<u16> = None;

    loop {
        match client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(());
                }
                last_status = Some(status.as_u16());
            }
            Err(e) => {
                tracing::trace!(url = %url, error = %e, "tx visibility poll: request error");
            }
        }
        if start.elapsed() >= timeout {
            return Err(DataTxError::Timeout {
                tx_id,
                url,
                timeout,
                last_status,
            });
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// Fetches `/v1/tx/{tx_id}` and asserts the returned JSON header matches
/// the originally-submitted transaction across **every** field that
/// appears on both sides — not just a hand-picked sample. Catches the
/// failure mode the status-code-only check misses: a binary mis-decoding
/// `Compact`-encoded records that another version wrote, returning 200
/// with structurally-valid but semantically-wrong content.
///
/// Comparison is on the JSON `Value` rather than through HEAD's typed
/// model so we don't impose HEAD's struct shape on responses from an
/// older binary that may have a slightly different envelope. The
/// per-run [`SchemaConfig`] supplies any rename-pair aliases or
/// skipped fields specific to the OLD↔NEW span being tested; any
/// other drift — value mismatch on a shared field, or a field present
/// on one side but absent on the other — is reported as an error.
pub async fn assert_tx_matches_original(
    client: &reqwest::Client,
    api_url: &str,
    expected: &DataTransaction,
    schema: &SchemaConfig,
) -> Result<(), DataTxError> {
    let tx_id = expected.header.id;
    let url = format!("{api_url}/v1/tx/{tx_id}");
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| DataTxError::Http {
            url: url.clone(),
            source: e,
        })?
        .error_for_status()
        .map_err(|e| DataTxError::Http {
            url: url.clone(),
            source: e,
        })?;
    let body: serde_json::Value = response.json().await.map_err(|e| DataTxError::Http {
        url: url.clone(),
        source: e,
    })?;

    // The route's response shape is `IrysTransactionResponse::Storage(header)`
    // or `Commitment(...)`. Both OLD and NEW use the same externally-tagged
    // enum at the wire level. Rather than depend on either typed model we
    // walk the JSON for the fields we want to compare.
    let header = body
        .get("storage")
        .or_else(|| body.get("Storage"))
        .or_else(|| {
            if body.is_object() && body.get("dataRoot").is_some() {
                Some(&body)
            } else {
                None
            }
        })
        .ok_or_else(|| {
            DataTxError::TxBuild(format!(
                "tx response at {url} did not contain a Storage header: {body}"
            ))
        })?;

    let expected_json =
        serde_json::to_value(&expected.header).map_err(|e| DataTxError::TxBuild(e.to_string()))?;

    compare_full_object(
        &format!("tx {tx_id} at {url}"),
        &expected_json,
        header,
        schema,
    )
}

/// Strict whole-object diff with rename-aware exemptions. Used by both
/// the tx-header check and the block-header cluster-consistency check.
///
/// Compares `expected` and `actual` field-by-field. A field that appears
/// in either alias pair is treated as belonging to a single logical slot:
/// having either one is fine, missing both is still a miss. All other
/// fields must be present on both sides with equivalent values (modulo
/// `string-vs-number` u64 wire drift).
pub fn compare_full_object(
    context: &str,
    expected: &serde_json::Value,
    actual: &serde_json::Value,
    schema: &SchemaConfig,
) -> Result<(), DataTxError> {
    let expected_obj = expected.as_object().ok_or_else(|| {
        DataTxError::TxBuild(format!("{context}: expected value is not a JSON object"))
    })?;
    let actual_obj = actual.as_object().ok_or_else(|| {
        DataTxError::TxBuild(format!("{context}: actual value is not a JSON object"))
    })?;

    let alias_for = |key: &str| -> Option<String> {
        for (a, b) in &schema.aliases {
            if key == a {
                return Some(b.clone());
            }
            if key == b {
                return Some(a.clone());
            }
        }
        None
    };
    let is_skipped = |key: &str| -> bool { schema.skip.iter().any(|s| s == key) };

    // Forward sweep — every expected key must be reachable on the actual
    // side. For aliased rename pairs we only verify the field exists;
    // values are inherently type-incompatible across the rename so byte
    // comparison would always fail. Fields in `skip` are ignored.
    for (key, expected_value) in expected_obj {
        if is_skipped(key) {
            continue;
        }
        match alias_for(key) {
            Some(alias) => {
                if actual_obj.get(key).is_none() && actual_obj.get(&alias).is_none() {
                    return Err(DataTxError::TxBuild(format!(
                        "{context}: field `{key}` (or alias `{alias}`) present in expected but missing in actual response"
                    )));
                }
            }
            None => match actual_obj.get(key) {
                None => {
                    return Err(DataTxError::TxBuild(format!(
                        "{context}: field `{key}` present in expected but missing in actual response"
                    )));
                }
                Some(av) if !json_values_equivalent(expected_value, av) => {
                    return Err(DataTxError::TxBuild(format!(
                        "{context}: field `{key}` mismatch:\n  expected: {expected_value}\n  actual:   {av}"
                    )));
                }
                _ => {}
            },
        }
    }

    // Reverse sweep — catch value drift on fields that exist on both
    // sides but the forward sweep didn't see. Aliased and skipped fields
    // are exempt. We don't error on actual-only fields: the route can
    // legitimately carry server-derived data (e.g. `promotedHeight`)
    // that the signed header didn't.
    for (key, actual_value) in actual_obj {
        if is_skipped(key) || alias_for(key).is_some() {
            continue;
        }
        if let Some(expected_value) = expected_obj.get(key)
            && !json_values_equivalent(expected_value, actual_value)
        {
            return Err(DataTxError::TxBuild(format!(
                "{context}: field `{key}` mismatch (reverse sweep):\n  expected: {expected_value}\n  actual:   {actual_value}"
            )));
        }
    }

    Ok(())
}

/// JSON-level equivalence with one tolerance: u64-ish fields like
/// `dataSize` / `headerSize` / `chainId` may be serialized as either a
/// JSON string (`"42"`) or a JSON number (`42`) depending on which serde
/// wrapper the producer uses. Both encode the same logical value, so we
/// coerce to a canonical string before comparing. All other JSON values
/// use strict equality.
fn json_values_equivalent(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a, b) {
        (serde_json::Value::String(s), serde_json::Value::Number(n))
        | (serde_json::Value::Number(n), serde_json::Value::String(s)) => *s == n.to_string(),
        _ => a == b,
    }
}

/// Fetches `/v1/block-index?height=0&limit=N` and returns the list of
/// block hashes the node knows about. Used by the cluster-consistency
/// check to enumerate every block that ought to be byte-identical across
/// running nodes — works on both OLD and NEW since the endpoint shape
/// hasn't drifted (same `BlockIndexItem` struct in both versions).
pub async fn fetch_block_index_hashes(
    client: &reqwest::Client,
    api_url: &str,
    limit: u64,
) -> Result<Vec<String>, DataTxError> {
    let url = format!("{api_url}/v1/block-index?height=0&limit={limit}");
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| DataTxError::Http {
            url: url.clone(),
            source: e,
        })?
        .error_for_status()
        .map_err(|e| DataTxError::Http {
            url: url.clone(),
            source: e,
        })?;
    let body: Vec<serde_json::Value> = response.json().await.map_err(|e| DataTxError::Http {
        url: url.clone(),
        source: e,
    })?;
    Ok(body
        .iter()
        .filter_map(|item| {
            item.get("blockHash")
                .or_else(|| item.get("block_hash"))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .collect())
}

/// Fetches a block header (as raw JSON) from `/v1/block/{block_hash}`.
/// We deliberately don't deserialize through HEAD's typed model because
/// that would impose HEAD's struct shape on responses from an OLDer node;
/// callers compare the JSON object directly via [`compare_full_object`].
pub async fn fetch_block_header(
    client: &reqwest::Client,
    api_url: &str,
    block_hash: &str,
) -> Result<serde_json::Value, DataTxError> {
    let url = format!("{api_url}/v1/block/{block_hash}");
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| DataTxError::Http {
            url: url.clone(),
            source: e,
        })?
        .error_for_status()
        .map_err(|e| DataTxError::Http {
            url: url.clone(),
            source: e,
        })?;
    response.json().await.map_err(|e| DataTxError::Http {
        url: url.clone(),
        source: e,
    })
}
