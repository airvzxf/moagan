# Pricing → List price

3 theses in 2 groups.

## Supplier feed

The feed sets the price; members differ on rounding.

### ★ sk_0001

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

<details>
<summary>1 duplicate of sk_0001</summary>

### sk_0004

_Provenance not recorded (model, temperature, replica, index)._

Publish the supplier feed price as the list price.

- Supplier feed is the source

</details>

## Hand-set by sales

People set the price.

### ★ sk_0002

_Provenance not recorded (model, temperature, replica, index)._

Let the sales team set | list prices by hand.

## Tensions

- **sk_0001** ↔ **sk_0002** — The feed and the sales team cannot both own the list price.
