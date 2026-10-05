# Pricing → List price

2 theses.

## All theses

### sk_0001

T=0.7 · minimax/MiniMax-M3 · replica 0 · index 1

Publish list prices from the supplier feed, rounded to the cent.

- Supplier feed is the source
- Round to the cent

<details>
<summary>Details</summary>

**Outline.** Import the feed, apply one rule, publish.

**Assumptions.**

- The feed is daily

**Strengths.**

- Simple

**Weaknesses.**

- Feed errors reach the shop

**Constraint check.** C1 ✓ · C2 ✗ · C10 ✗ · legal ✗

**Expected validation.** Compare 50 published prices with the feed.

</details>

### sk_0002

Let the sales team set | list prices by hand.
