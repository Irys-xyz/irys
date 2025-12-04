# Finality on Irys

## Probabilistic Finality in Proof-of-Work

Irys is a Proof-of-Work (PoW) network. Block confirmation is achieved by extending the chain: each subsequent honest block exponentially reduces the probability that an earlier block can be reversed via a chain reorganization (reorg).

A block with few confirmations can still be displaced if an attacker secretly mines a longer competing chain. The probability of a successful reorg decreases rapidly with the number of confirmations.

## Finality Model

Unlike BFT-style Proof-of-Stake systems that provide deterministic finality via voting, Irys (like Bitcoin) offers **probabilistic finality**. Security is a direct function of the attacker’s fraction of total network hash power.

For a block with **z = 6 confirmations**, the exact probability of a successful attack is given by the cumulative Poisson distribution (Nakamoto formula):

$$
P(\text{success}) = 1 - \sum_{k=0}^{5} e^{-\lambda} \cdot \frac{\lambda^k}{k!} \quad
\text{where} \quad \lambda = z \cdot \frac{q}{p}, \quad p = 1 - q
$$


A widely used conservative approximation for q < 0.5 is:

$$
P(\text{success}) \approx \left( \frac{q}{p} \right)^6 \quad \text{for} \quad q < p
$$


When q ≥ 0.5 the attack succeeds with certainty.

### Reorg Probability at 6 Confirmations

| Attacker Hash Power (q) | Approx. Reorg Probability | Finality Confidence |
|--------------------------|----------------------------|---------------------|
| 10%                      | 0.00034% (~1 in 294,000)  | 99.99966%           |
| 20%                      | 0.0041%  (~1 in 24,400)   | 99.9959%            |
| 25%                      | 0.068%   (~1 in 1,470)    | 99.932%             |
| 30%                      | 0.47%    (~1 in 213)      | 99.53%              |
| 35%                      | 2.0%                       | 98.0%               |
| 40%                      | 8.3%                       | 91.7%               |
| 45%                      | 26.6%                      | 73.4%               |
| ≥50%                     | 100%                       | 0%                  |

### Key Takeaways

- 6 confirmations provide >99.99% finality against attackers controlling ≤20% of hash power.
- Confidence remains >99% against attackers up to ~28% of hash power.
- Security degrades sharply as the attacker approaches 50%.
- A majority (≥50%) attacker can reorg any depth with near-certainty; no fixed confirmation count guarantees safety in that scenario.

The **6-confirmation rule** was selected to deliver extremely high assurance under realistic attacker budgets while keeping median confirmation time low. For applications requiring stronger guarantees, simply wait for additional confirmations (e.g., 12 or 24), as the probability drops exponentially.
