# Defect detection list

Flag code that matches a line below. Then read `classes/<id>.md` to confirm each lead and to get the
correct form. Do not flag an item under "Do not flag"; `README.md` has its full entry under
"Known non-issues", by the same id.

## Classes

- `hash-iteration-order-in-consensus` (fork). Scope: consensus code: block validation, block production, shadow transactions, epoch and ledger state. Flag: a result that must be the same on every node depends on HashMap or HashSet iteration order (first match or `break`, last write wins, a tie-break, collect into a Vec, hash or serialize).
- `uncapped-peer-response-body` (dos). Scope: code that reads an HTTP response body from a peer, error paths included. Flag: `.json()`, `.text()` or `.bytes()` on a peer response, with no byte cap before the full body is read.
- `wall-clock-for-internal-timing` (dos). Scope: code that measures time: timeouts, sleeps, backoff, rate limits, windows, expiry. Flag: a wall-clock read (`SystemTime::now()`, `UnixTimestamp(Ms)::now()`, `now_ms()`) that feeds a duration, timeout, expiry or last-seen value; or a `tokio` sleep trusted to move the wall clock.

## Do not flag

- `reth-irys-state-mismatch-is-fatal`: `RethService` stops, with no retry, when a fork-choice update to Reth fails (Reth/Irys state mismatch). This is intended.
