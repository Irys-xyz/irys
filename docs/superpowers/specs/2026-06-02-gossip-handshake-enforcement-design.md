# Gossip handshake enforcement & chain_id isolation

**Date:** 2026-06-02
**Status:** Near-term fix implemented (evict-on-rejection); larger follow-up scoped, pending plan

## Problem

PR #1435 (commit `12614c191`) added a hard chain_id rejection to the V1/V2
handshake handlers: a handshake declaring a different `chain_id` is rejected
before signature verification. The intent was to prevent a node from peering
with a different network.

In testing on devnet, a node configured for a different chain **kept
communicating with the network anyway**. Investigation shows why: the chain_id
gate lives **only in the handshake handler**, but on the current stateless-HTTP
gossip plane the handshake is not on the critical path for data exchange once a
peer is known.

### Root cause (verified)

1. **Data routes gate on cache-membership, not on a handshake.** Every gossip
   data handler funnels through exactly three origin chokepoints:
   - `check_peer_v1_reason` (`crates/p2p/src/server.rs:176`)
   - `check_peer_v2` (`crates/p2p/src/server.rs:229`)
   - `compute_health_check` (`crates/p2p/src/server.rs:1067`)

   All three authorize purely on *"is this peer in my cache + does the source IP
   match its stored gossip IP (+ peer_id for v2)"*. None checks `chain_id`, and
   none requires that a handshake occurred in the current process lifetime. (Block,
   tx, commitment, ingress-proof, and chunk routes all call `check_peer_v1/v2` —
   e.g. chunk at `server.rs:134`.)

2. **Cache membership is durable across restarts.** Staked peers are persisted to
   the DB (`insert_peer_list_item`, `crates/p2p/src/peer_network_service.rs:232`)
   and reloaded at startup by `PeerList::new`
   (`crates/domain/src/models/peer_list.rs:92`, via `walk_all::<PeerListItems>`),
   with `is_online` restored verbatim (`from_inner`,
   `crates/types/src/peer_list.rs:171`).

The combination: a peer learned once (on a previous run, when both sides were on
the correct chain) returns as known + online with **zero handshakes**, and the
data plane accepts it forever. The chain_id check only guards *new* handshakes,
which never re-run for a cached peer. That is why a node whose `chain_id` was
changed kept talking to its previously-cached peers.

### Asymmetry (verified)

The chain_id check validates the chain_id of whoever **POSTs** the handshake —
the receiver checks the sender. In the outbound announce path
(`announce_yourself_to_address`, `crates/p2p/src/peer_network_service.rs:1043`)
we POST our handshake and consume their `Accepted` response, which carries **no
chain_id** (`HandshakeResponseV2`, `crates/types/src/version.rs:380`). So we
never verify the *responder's* chain. Inbound is protected; outbound pulls from
an already-cached foreign peer are not.

### What the V2 gossip protocol authenticates today

| Message | Authenticated? | Carries chain_id? |
|---|---|---|
| `HandshakeRequestV2` (`version.rs:125`) | Yes — signed over RLP of all fields incl. `chain_id`, `peer_id`, `consensus_config_hash`; verified against `mining_address` | Yes (hard-checked, #1435) |
| `HandshakeResponseV2` (`version.rs:380`) | **No** — no signature, no responder identity, no `chain_id` | **No** |
| `GossipRequestV2<T>` envelope (`gossip.rs:480`) | Envelope: source-IP-bound only. Payload: self-signed downstream (block/tx sigs) | n/a |

The protocol authenticates the **initiator to the responder, but never the
responder to the initiator**.

## Near-term fix (chosen): evict peers on chain-rejection

Scope: close the chain_id-isolation bug with no persistence and no wire change.
Decision basis: the devnet under test is **fully on the #1435 build**, so we can
rely on peers enforcing the chain_id check — a foreign node's announce is rejected
by every (upgraded) peer.

### Why "just re-handshake on startup" is not enough by itself

The node already re-handshakes every cached peer at startup
(`spawn_announce_yourself_to_all_peers_task`,
`crates/p2p/src/peer_network_service.rs:300`). The gap is that the rejection is
discarded: a terminal chain rejection (`ChainIdMismatch` → `NetworkMismatch`)
lands in `handle_announcement_finished`'s terminal branch
(`peer_network_service.rs:604`), which only inserts into `failed_announcements` —
a map that is **written but never read** (`:605`, single usage). The data plane
(`check_peer_v1/v2`) never consults handshake outcome; it trusts cache membership
+ IP. So a rejected peer stays fully trusted. The fix is to make the rejection
*evict* the peer.

### Design

1. Add an in-memory eviction primitive that removes a peer from the cache,
   mirroring the existing purgatory-eviction map cleanup
   (`crates/domain/src/models/peer_list.rs:976-986`): drop from
   `persistent_peers_cache`/purgatory and the
   `gossip`/`api`/`miner`/`peer_id`/`known_peers` index maps; emit
   `PeerEvent::PeerRemoved`. Expose `PeerList::remove_peer_by_api_address`.
2. Add `delete_peer_list_item(tx, peer_id)` to `irys-database` (mirror of
   `insert_peer_list_item`, using `tx.delete::<PeerListItems>`).
3. At the outbound-announce rejection site (`announce_yourself_to_address`, the
   `PeerResponse::Rejected` arm, `peer_network_service.rs:1084`): when
   `rejected_response.reason == version::RejectionReason::NetworkMismatch`, evict
   the peer from the cache (`remove_peer_by_api_address`) and delete it from the
   DB (via `inner`'s `DatabaseProvider`, using the `flush`-style
   `db.update_scoped`), then return the existing error.
4. Inbound ("we reject them") is already correct: #1435 returns `ChainIdMismatch`
   *before* `add_or_update_peer`, so a foreign peer is never added. A cached
   foreign peer cannot occur on a fully-upgraded network, so inbound eviction is
   omitted as unnecessary.

### Behavior

- Foreign-chain node restart: re-announces to all cached peers → every upgraded
  peer rejects with `ChainIdMismatch` → node evicts each from cache + DB → empty
  peer set → isolated. Subsequent gossip/pulls have no peers to talk to.

### Limitations (accepted)

- Relies on peers enforcing #1435. Against an unupgraded peer (none on this
  devnet) there is no rejection to trigger eviction. The self-enforcing variants
  (per-peer stored `chain_id`, or `chain_id` in the handshake response) remain
  available if the network is ever mixed-version — see the follow-up section.
- Small startup window: between DB load and the announce round completing, the
  data plane trusts cached peers. Accepted; cross-chain data fails consensus
  validation downstream regardless.

### Size

~50–70 LOC (eviction primitive + DB delete + one wiring arm) + tests. No wire
change, no persistence.

### Limitations (why this is "for now")

- Does **not** make the protocol "always require a handshake to set up a
  connection." A still-running peer that has us cached from before keeps serving
  us until *it* restarts; the data plane still trusts cache membership.
- Does not close the outbound asymmetry in general (only the persisted-peer
  instance of it).

## Follow-up (larger): enforce a session handshake + authenticate the response

This is the real fix for the invariant *"our networking protocol should always
require a handshake to set up a connection"*, and the bridge to the planned
stateful-socket model (where the handshake literally establishes the socket and
must mutually authenticate both ends). Because Part B changes the
`HandshakeResponseV2` wire format, this is the right window to fix the response's
authentication gap at the same time — retrofitting it later is another
coordinated protocol change.

### Part A — session-scoped handshake gate

- Add an in-memory `session_handshaked: HashSet<IrysPeerId>` to
  `PeerListDataInner` — **never persisted** (a restart must force re-handshake;
  that is the point).
- Set it in `handle_handshake_v1`/`v2` right after `add_or_update_peer`
  (`server.rs:1279` / V2 equivalent) — i.e. *after* chain_id + signature pass, so
  only valid same-chain peers are flagged.
- Gate the three origin chokepoints (`check_peer_v1_reason`, `check_peer_v2`,
  `compute_health_check`): require cache-membership **and** session-handshaked,
  else return `HandshakeRequired(SessionHandshakeRequired)` (new diagnostic
  variant).
- **No new client recovery needed.** Clients already react to `HandshakeRequired`
  with `initiate_handshake(force=true)` + retry (`gossip_client.rs:952, 1846,
  2037, 2378`; wait-then-retry loop `:2165/2465`). A restart yields one
  `HandshakeRequired` per peer, a re-handshake, then resumption.

Part A alone fully closes the realistic threat (a genuinely foreign node trying
to join a patched network) in **both** directions, because the gate forces the
joining node to handshake — proving its chain — before any data flows.

### Part B′ — authenticate the handshake response (subsumes the chain_id fix)

The narrow "add chain_id to the response" fix is a symptom of the response being
anonymous and unauthenticated. Fix the larger gap:

- Add to `HandshakeResponseV2`: `mining_address`, `peer_id`, `chain_id`, and a
  `signature` — and bind it to *this* handshake by signing over the initiator's
  request `signature_hash` (or a request nonce) to prevent replay by an on-path
  attacker.
- Responder signs with its mining key (same scheme as the request).
- Announcer (`peer_network_service.rs:1043`) verifies: signature valid → responder
  identity matches what we expected for that address → `chain_id == ours`. Any
  failure ⇒ don't mark success, drop/blocklist.
- **Back-compat:** old V2 peers send the legacy (unsigned, no-chain_id) response;
  treat absent fields as "legacy, unverifiable" and fall back to today's advisory
  behavior — the same posture #1435 used for `ChainIdMismatch`. Stays
  additive-within-V2 with graceful degradation, unless we decide to *require*
  authenticated responses (then it becomes a V3 negotiation gate).

This yields mutual authentication — the primitive the stateful-socket migration
needs — and independently closes the outbound chain asymmetry regardless of the
counterparty's patch state.

### Size (follow-up)

- Part A: ~60–90 LOC + ~100 LOC tests. Contained.
- Part B′: ~80–140 LOC — response fields + an `encode_for_signing` for the
  response + sign/verify + back-compat-absent decode + announce-side verify/drop +
  request-binding nonce + round-trip/back-compat tests. The only part with
  wire-format + signing risk; the part most needing care since the protocol window
  is rare.

## Tier-3 hardening (cheap, fold into whichever change ships)

- **Origin-check `handle_stake_and_pledge_whitelist`** (`server.rs:1134` →
  `gossip_data_handler.rs:986`). It currently returns the node's stake/pledge
  whitelist to any unauthenticated caller with no `check_peer` gate and no rate
  limit. **Low severity** — the data is largely on-chain public (the only nuance:
  it may include mempool-pending entries). The fix is about *consistency* (every
  other `/gossip/*` route requires a handshaked peer; this one silently does not)
  and removing a free unauthenticated probe/DoS surface. Non-wire, trivial.
- **`consensus_config_hash` policy.** Today a mismatch is advisory (logged only,
  `peer_network_service.rs:1047`). Recommendation: at most a *soft* gate (score
  penalty), **not** a hard reject — a hard reject risks partitioning the network on
  benign config drift during rolling upgrades.

## Deferred / explicitly speculative (not now)

- Session-id / nonce negotiation for the socket layer — wait until the socket work
  defines its needs; adding unused fields now is speculative.
- Per-message gossip-envelope signatures — payloads are already self-authenticating
  and relay identity is IP-bound; not worth the cost.
- A genesis/network-fingerprint field beyond `chain_id` (two networks can share a
  `chain_id`) — revisit only if real deployments collide on `chain_id`.
