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
        "tx {tx_id:?} did not become visible at {url} within {timeout:?}; \
         last status={last_status:?}, last request error={last_request_error:?}"
    )]
    Timeout {
        tx_id: H256,
        url: String,
        timeout: Duration,
        /// Last HTTP status the server returned, if any. `None` means we
        /// never got a response back inside the window — see `last_request_error`.
        last_status: Option<u16>,
        /// Last transport-level error from `reqwest`, if any. `None` here
        /// alongside `Some(_)` in `last_status` means the server was
        /// reachable but returning non-2xx; `Some(_)` means we failed to
        /// reach it at all (connection refused, DNS, TLS, etc.).
        last_request_error: Option<String>,
    },
    #[error(
        "tx {tx_id:?} promotion-status response at {url} is missing the \
         expected `promotion_height`/`promotionHeight` field — schema drift? body={body}"
    )]
    UnexpectedPromotionResponse {
        tx_id: H256,
        url: String,
        body: String,
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
    //
    // The field names below MUST be kept in sync with
    // [`crate::run_config::SUPPORTED_KEEP_DEFAULT_FIELDS`]. `RunConfig::validate`
    // fails fast on any `keep_default` entry that doesn't appear in that
    // list, which prevents the silent failure where a typo'd opt-out
    // re-enables the override and OLD rejects every signed tx.
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
    let mut last_request_error: Option<String> = None;

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
                    // Distinguish "field present but null" (not yet promoted —
                    // keep polling) from "field absent" (schema drift — fail
                    // fast so the harness doesn't mask a renamed field as a
                    // flaky timeout).
                    match body
                        .get("promotion_height")
                        .or_else(|| body.get("promotionHeight"))
                    {
                        None => {
                            return Err(DataTxError::UnexpectedPromotionResponse {
                                tx_id,
                                url,
                                body: body.to_string(),
                            });
                        }
                        Some(serde_json::Value::Null) => { /* not yet promoted */ }
                        Some(v) => match v {
                            serde_json::Value::Number(n) => {
                                if let Some(height) = n.as_u64() {
                                    return Ok(height);
                                }
                                return Err(DataTxError::UnexpectedPromotionResponse {
                                    tx_id,
                                    url,
                                    body: body.to_string(),
                                });
                            }
                            serde_json::Value::String(s) => {
                                if let Ok(height) = s.parse::<u64>() {
                                    return Ok(height);
                                }
                                return Err(DataTxError::UnexpectedPromotionResponse {
                                    tx_id,
                                    url,
                                    body: body.to_string(),
                                });
                            }
                            _ => {
                                return Err(DataTxError::UnexpectedPromotionResponse {
                                    tx_id,
                                    url,
                                    body: body.to_string(),
                                });
                            }
                        },
                    }
                } else {
                    last_status = Some(status.as_u16());
                }
            }
            Err(e) => {
                // `debug!` (not `trace!`) so default-tracing CI logs show the
                // transport error — the alternative is a timeout message
                // with `last_status: None` and no breadcrumb of why.
                tracing::debug!(url = %url, error = %e, "promotion poll: request error");
                last_request_error = Some(e.to_string());
            }
        }
        if start.elapsed() >= timeout {
            return Err(DataTxError::Timeout {
                tx_id,
                url,
                timeout,
                last_status,
                last_request_error,
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
    let mut last_request_error: Option<String> = None;

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
                // See `wait_for_promotion` — `debug!` so the breadcrumb
                // survives to the timeout message.
                tracing::debug!(url = %url, error = %e, "tx visibility poll: request error");
                last_request_error = Some(e.to_string());
            }
        }
        if start.elapsed() >= timeout {
            return Err(DataTxError::Timeout {
                tx_id,
                url,
                timeout,
                last_status,
                last_request_error,
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
/// Compares `expected` and `actual` field-by-field. An alias pair models a
/// field that was *renamed* across versions: when the two sides use the
/// different names of the pair, presence is verified but values are not
/// compared (a rename can also retype, so values aren't comparable). When the
/// *same* name appears on both sides — no rename happened in this response —
/// values ARE compared, so the alias never silently disables a field it was
/// merely configured to tolerate. All non-aliased fields must be present on
/// both sides with equivalent values (modulo `string-vs-number` u64 wire
/// drift).
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
    // side. Fields in `skip` are ignored.
    //
    // Aliases relax the check *only* when the field was genuinely renamed —
    // i.e. the expected key is absent on the actual side and only the alias
    // is present. A rename can also retype, so values across the rename
    // aren't comparable; presence is all we can check. But when the *same*
    // key name appears on the actual side, no rename happened in this
    // response, so we compare values normally — otherwise an alias configured
    // for some other span would silently stop checking the very field it
    // names whenever both versions happen to emit the shared name.
    for (key, expected_value) in expected_obj {
        if is_skipped(key) {
            continue;
        }
        match alias_for(key) {
            Some(alias) => match actual_obj.get(key) {
                // Same key present on both sides — not a rename here, compare.
                Some(av) if !json_values_equivalent(expected_value, av) => {
                    return Err(DataTxError::TxBuild(format!(
                        "{context}: field `{key}` mismatch:\n  expected: {expected_value}\n  actual:   {av}"
                    )));
                }
                Some(_) => {}
                // Key renamed away — accept the alias's presence, don't compare.
                None if actual_obj.get(&alias).is_some() => {}
                None => {
                    return Err(DataTxError::TxBuild(format!(
                        "{context}: field `{key}` (or alias `{alias}`) present in expected but missing in actual response"
                    )));
                }
            },
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
///
/// A block-index item that doesn't yield a string `blockHash`/`block_hash`
/// is a hard error, **not** a silent skip: if the endpoint shape ever drifts
/// (the exact failure this harness exists to catch), silently dropping the
/// item would shrink the consistency sweep into a partial check — or a no-op
/// that still passes. Failing loud keeps the sweep honest.
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
    let mut hashes = Vec::with_capacity(body.len());
    for (idx, item) in body.iter().enumerate() {
        let hash = item
            .get("blockHash")
            .or_else(|| item.get("block_hash"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                DataTxError::TxBuild(format!(
                    "block-index item {idx} from {url} has no string `blockHash`/`block_hash` \
                     field — endpoint schema drift? item={item}"
                ))
            })?;
        hashes.push(hash.to_owned());
    }
    Ok(hashes)
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

/// Polls a chunk GET URL until it returns 2xx with a JSON body, or the
/// timeout elapses. A tx becomes visible via `/v1/tx/{id}` before its chunks
/// are necessarily retrievable by data-root (the storage node still has to
/// finish ingress-proof generation and land the chunk in the Publish-ledger
/// partition), so the read-back check polls rather than assuming the chunk is
/// immediately available.
async fn fetch_chunk_until_available(
    client: &reqwest::Client,
    url: &str,
    tx_id: H256,
    timeout: Duration,
) -> Result<serde_json::Value, DataTxError> {
    let start = Instant::now();
    let mut last_status: Option<u16> = None;
    let mut last_request_error: Option<String> = None;

    loop {
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return resp.json().await.map_err(|e| DataTxError::Http {
                        url: url.to_owned(),
                        source: e,
                    });
                }
                last_status = Some(status.as_u16());
            }
            Err(e) => {
                tracing::debug!(url = %url, error = %e, "chunk poll: request error");
                last_request_error = Some(e.to_string());
            }
        }
        if start.elapsed() >= timeout {
            return Err(DataTxError::Timeout {
                tx_id,
                url: url.to_owned(),
                timeout,
                last_status,
                last_request_error,
            });
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// For every chunk of `tx`, fetches it back from `api_url` via
/// `/v1/chunk/data-root/{ledger}/{data_root}/{offset}` and asserts the
/// packing-invariant fields (`dataRoot`, `dataSize`, `dataPath`, `txOffset`)
/// match the chunk we originally built and uploaded.
///
/// The endpoint serves a *packed* chunk (`ChunkFormat::Packed`), whose
/// `bytes` / `packingAddress` / `partitionOffset` / `partitionHash` are local
/// to the serving node's mining identity and partition layout — comparing
/// those across nodes, or against the unpacked original, is meaningless, so
/// they're skipped (`bytes` is the only such field that also exists on the
/// unpacked original; the rest are accepted as actual-only). `dataPath` (the
/// Merkle inclusion proof) is the version-sensitive field: a storage-module
/// migration that re-encodes chunk records would surface as a `dataPath` (or
/// `dataRoot` / `dataSize`) mismatch here — a class of corruption the
/// tx-header and block-header checks cannot see, because chunk storage lives
/// in separate MDBX tables with their own `Compact` derivations.
pub async fn assert_chunks_match_original(
    client: &reqwest::Client,
    api_url: &str,
    tx: &DataTransaction,
    timeout: Duration,
) -> Result<(), DataTxError> {
    let chunks = tx
        .data_chunks()
        .map_err(|e| DataTxError::TxBuild(format!("data_chunks(): {e}")))?;
    let ledger_id = DataLedger::Publish as u32;
    let data_root = tx.header.data_root;
    // Packing is node-local, so only the packing-invariant identity + Merkle
    // proof are comparable against the original unpacked chunk.
    let schema = SchemaConfig {
        aliases: vec![],
        skip: vec!["bytes".to_owned()],
    };

    for (offset, chunk) in chunks.into_iter().enumerate() {
        let url = format!("{api_url}/v1/chunk/data-root/{ledger_id}/{data_root}/{offset}");
        let served = fetch_chunk_until_available(client, &url, tx.header.id, timeout).await?;
        let expected =
            serde_json::to_value(&chunk).map_err(|e| DataTxError::TxBuild(e.to_string()))?;
        compare_full_object(
            &format!("chunk {offset} of tx {} at {url}", tx.header.id),
            &expected,
            &served,
            &schema,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod compare_full_object_tests {
    use super::*;
    use crate::run_config::SchemaConfig;
    use serde_json::json;

    fn empty_schema() -> SchemaConfig {
        SchemaConfig::default()
    }

    fn schema_with_alias(a: &str, b: &str) -> SchemaConfig {
        SchemaConfig {
            aliases: vec![(a.to_owned(), b.to_owned())],
            skip: vec![],
        }
    }

    fn schema_with_skip(field: &str) -> SchemaConfig {
        SchemaConfig {
            aliases: vec![],
            skip: vec![field.to_owned()],
        }
    }

    #[test]
    fn passes_on_identical_objects() {
        let v = json!({"a": 1, "b": "two", "c": [1, 2, 3]});
        compare_full_object("ctx", &v, &v, &empty_schema()).expect("identical objects must match");
    }

    #[test]
    fn fails_on_value_mismatch() {
        let expected = json!({"a": 1});
        let actual = json!({"a": 2});
        let err = compare_full_object("ctx", &expected, &actual, &empty_schema())
            .expect_err("value mismatch must fail");
        let msg = err.to_string();
        assert!(msg.contains("`a`"), "error must name the field: {msg}");
        assert!(msg.contains("mismatch"), "error must say mismatch: {msg}");
    }

    #[test]
    fn fails_on_field_missing_from_actual() {
        let expected = json!({"a": 1, "b": 2});
        let actual = json!({"a": 1});
        let err = compare_full_object("ctx", &expected, &actual, &empty_schema())
            .expect_err("missing-on-actual must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("`b`"),
            "error must name the missing field: {msg}"
        );
        assert!(msg.contains("missing"), "error must say missing: {msg}");
    }

    #[test]
    fn skip_list_exempts_field_from_comparison() {
        let expected = json!({"a": 1, "b": 2});
        // Differing values *and* missing-from-actual are both exempt when
        // the field is in the skip list.
        let actual = json!({"a": 1, "b": 999});
        compare_full_object("ctx", &expected, &actual, &schema_with_skip("b"))
            .expect("skipped field must not fail on value mismatch");

        let actual_missing = json!({"a": 1});
        compare_full_object("ctx", &expected, &actual_missing, &schema_with_skip("b"))
            .expect("skipped field must not fail when missing from actual");
    }

    #[test]
    fn alias_pair_passes_when_either_side_has_field() {
        let schema = schema_with_alias("bundleFormat", "metadataFormat");

        // OLD-name on expected, NEW-name on actual.
        let expected = json!({"bundleFormat": 0, "other": "v"});
        let actual = json!({"metadataFormat": 1, "other": "v"});
        compare_full_object("ctx", &expected, &actual, &schema)
            .expect("alias pair: OLD↔NEW name swap must pass");

        // Same name on both sides with equal values passes — no rename
        // happened in this response, so values are compared (and match).
        let same_name = json!({"bundleFormat": 0, "other": "v"});
        compare_full_object("ctx", &same_name, &same_name, &schema)
            .expect("alias pair: same-name on both sides with equal values must pass");
    }

    #[test]
    fn alias_pair_compares_values_when_same_key_on_both_sides() {
        // Regression guard: an alias must NOT silently disable value
        // comparison for a field that both sides still emit under the same
        // name. Presence-only is only correct across an actual rename.
        let schema = schema_with_alias("bundleFormat", "metadataFormat");
        let expected = json!({"metadataFormat": 0, "other": "v"});
        let actual = json!({"metadataFormat": 999, "other": "v"});
        let err = compare_full_object("ctx", &expected, &actual, &schema).expect_err(
            "aliased field present under the same name on both sides must compare values",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("metadataFormat"),
            "error must name the field: {msg}"
        );
        assert!(msg.contains("mismatch"), "error must say mismatch: {msg}");
    }

    #[test]
    fn alias_pair_fails_when_neither_side_has_field() {
        let schema = schema_with_alias("bundleFormat", "metadataFormat");
        let expected = json!({"bundleFormat": 0, "other": "v"});
        let actual = json!({"unrelated": 1, "other": "v"});
        let err = compare_full_object("ctx", &expected, &actual, &schema)
            .expect_err("alias pair: neither side has the field, must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("bundleFormat"),
            "error must name the missing field: {msg}",
        );
    }

    #[test]
    fn alias_pair_does_not_compare_values_across_rename() {
        // The whole point of an alias pair: a rename can also retype
        // (`Option<u64>` → `u8`), so values on either side of the rename
        // are not byte-comparable. The check is presence-only.
        let schema = schema_with_alias("bundleFormat", "metadataFormat");
        let expected = json!({"bundleFormat": 0});
        let actual = json!({"metadataFormat": "completely different"});
        compare_full_object("ctx", &expected, &actual, &schema).expect(
            "alias pair does NOT compare values — type drift across the rename is the design",
        );
    }

    #[test]
    fn actual_only_fields_are_accepted() {
        // The route can carry server-derived data (e.g. `promotedHeight`)
        // that the originally-signed header didn't. Extra fields on the
        // actual side are not an error.
        let expected = json!({"a": 1});
        let actual = json!({"a": 1, "promotedHeight": 42});
        compare_full_object("ctx", &expected, &actual, &empty_schema())
            .expect("server-derived extra fields must be accepted");
    }

    #[test]
    fn string_and_number_u64_are_equivalent() {
        // Some serde wrappers stringify u64 values; some don't. Both encode
        // the same logical value.
        let expected = json!({"chainId": "42"});
        let actual = json!({"chainId": 42});
        compare_full_object("ctx", &expected, &actual, &empty_schema())
            .expect("string-vs-number u64 must be tolerated");

        // But NOT when the values genuinely differ.
        let actual_wrong = json!({"chainId": 43});
        compare_full_object("ctx", &expected, &actual_wrong, &empty_schema())
            .expect_err("string `42` vs number `43` is a real mismatch");
    }

    #[test]
    fn errors_on_non_object_input() {
        let array = json!([1, 2, 3]);
        let obj = json!({"a": 1});
        compare_full_object("ctx", &array, &obj, &empty_schema())
            .expect_err("non-object expected must error");
        compare_full_object("ctx", &obj, &array, &empty_schema())
            .expect_err("non-object actual must error");
    }
}
