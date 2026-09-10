# logos-evm-fee-module

EIP-1559 fee suggestion for EVM chains: slow/normal/fast tiers derived from
`eth_feeHistory`, with custom overrides.

## Why it is its own module

Two wallets need fee estimation and neither should own a copy. The previous
arrangement — the wallet backend deriving fees inline — is how the derivation
drifted into **overpaying 2x on every send** without anyone noticing:

```rust
max_fee_per_gas:          gas_price * 2
max_priority_fee_per_gas: gas_price      // <- the bug
```

`eth_gasPrice` is roughly `baseFee + tip`, so using it AS the tip tips
approximately the whole base fee. EIP-1559 caps the effective tip at
`maxFee - baseFee`, so it does not cost 500x — it costs exactly 2x, every time.
Benign on Anvil and mock nodes, which is why the doctests stayed green.

## The algorithm

The standard fee-history strategy (Nethereum's
`MedianPriorityFeeHistorySuggestionStrategy`; ethers and alloy implement the
same shape):

```
tip    = median of the reward percentile over the last 10 blocks
base   = next block's base fee (eth_feeHistory already returns it)
maxFee = base * headroom + tip
```

Headroom multiplies the **base fee**, never `gasPrice` — doubling a number that
already contains the tip gives the least headroom exactly when the base fee is
climbing, which is when headroom is the point.

| tier | reward percentile | base headroom |
|---|---|---|
| slow | 10 | 2x |
| normal | 50 | 2x |
| fast | 90 | 3x |

A chain with no usable fee history falls back to `eth_gasPrice` with a
**zero tip** — on a pre-1559 chain a priority fee is not meaningful, and
emitting the gas price there is the original bug.

## API

```
suggest_fees(chain_id) -> { ok, chainId, baseFeePerGas, source, tiers: { slow, normal, fast } }
estimate(chain_id, request_json) -> { ok, maxFeePerGas, maxPriorityFeePerGas, gasLimit, totalWei, source }
```

`source` is `"feeHistory"`, `"gasPrice"` (legacy fallback) or `"custom"`.
All **wei** values are decimal strings — JSON numbers cannot carry 256 bits.
`gasLimit` is a JSON **number**: it is a bounded count, not a wei value.

`estimate` accepts, in order of precedence:

```jsonc
{ "maxFeePerGas": "...", "maxPriorityFeePerGas": "..." }  // obeyed verbatim
{ "tier": "slow" | "normal" | "fast" }                    // default: normal
{ "gasLimit": "..." }                                     // else estimated from "tx"
{ "tx": { ... } }                                         // eth_estimateGas
```

This module advises; it does not overrule. An explicit fee override is used as
given.

## Design notes

- **`concurrency: "multi"` with `&self` from the first commit.** Every method
  makes a blocking call out to `eth_rpc_module`, so one slow chain must not
  stall a suggestion for another. The module holds no mutable state, so it is
  trivially `Sync`. Retrofitting `multi` onto a `&mut self` module is a
  refactor, not a flag — see `wallet_backend_module`.
- **No network of its own.** All RPC goes through `eth_rpc_module`, which owns
  the single fail-closed proxy chokepoint.
- **The `tx` sent to `eth_estimateGas` carries a zero fee cap.** Given a `tx`
  with no `gas`, the nimbus verified proxy prices the whole block gas limit
  against the sender's balance and refuses anything under ~0.27 ETH. A zero cap
  makes that check vacuous. Any fee field on the submitted `tx` is zeroed or
  dropped for the estimate only — the returned fee comes from the request's
  top-level `maxFeePerGas`/`maxPriorityFeePerGas` or the tier, untouched.
- **No alloy.** Fee arithmetic tops out near `maxFee * gasLimit` ~ 1e19 wei,
  which fits `u128`; only balances need 256 bits and this module never touches
  one. The dep set is `serde` + `serde_json`.

## Tests

```bash
cd rust-lib && cargo test --no-default-features   # pure algorithm, no Logos runtime
```
