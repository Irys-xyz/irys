# Defect detection list

Flag code that matches a line below. Then read `classes/<id>.md` to confirm each lead and to get the
correct form. Do not flag an item under "Do not flag".

## Classes

- `hash-iteration-order-in-consensus` (fork). Scope: consensus code: block validation, block production, shadow transactions, epoch and ledger state. Flag: a result that must be the same on every node depends on HashMap or HashSet iteration order (first match or `break`, last write wins, a tie-break, collect into a Vec, hash or serialize).
- `uncapped-peer-response-body` (dos). Scope: code that reads an HTTP response body from a peer, error paths included. Flag: `.json()`, `.text()` or `.bytes()` on a peer response, with no byte cap before the full body is read.

## Do not flag

- `RethService` stops, with no retry, when a fork-choice update to Reth fails (Reth/Irys state mismatch). This is intended: see "Known non-issues" in `README.md`.
