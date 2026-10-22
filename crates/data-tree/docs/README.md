# Data tree documents — concept branch

This branch holds CONCEPTS for the data tree: designs and integration plans that are NOT built. Nothing here is ratified or implemented. Do not merge this branch; designs move to a working branch only when they are built.

- [integration-proposal.md](integration-proposal.md) — planned L1 and gateway integration: tx header and chunk message, PoA mapping and activation, rollout, the implementation map, ingress proofs, gateway receipts, and `tx_root` v2 (its Appendix C). Not ratified, not implemented.
- [designs-not-adopted.md](designs-not-adopted.md) — the compact multi-segment proof and the tree salt. Not adopted, not implemented.
- [spec.md](spec.md) — the data-tree specification as it stood when this branch was cut. The built and ratified version lives with the code on the implementation branch; on any conflict, that version wins.

Domain tags `0x05` to `0x07` (TX_LEAF, TX_NODE, TX_COUNT) are used by `tx_root` v2 in integration-proposal.md Appendix C. The implementation's tag registry lists only the tags it uses and notes that these three are taken here.
