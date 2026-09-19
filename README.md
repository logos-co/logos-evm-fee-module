# logos-evm-fee-module

EIP-1559 fee suggestion and gas estimation for EVM chains: slow/normal/fast tiers derived
from `eth_feeHistory`, a gas limit for one call or for a whole bundle of them, and every
ceiling answered in wei and in the native unit.

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
| normal | 30 | 2x |
| fast | 60 | 3x |

A block's reward percentiles are its included tips ordered by gas used, so the
marginal tip that still got in sits low in the distribution and the top of it is
MEV. Measured on mainnet on 2026-09-13 at a 0.07 gwei base fee: p50 was 0.009
gwei, p90 was 1.0 gwei — a "fast" tier read off p90 tipped fourteen base fees
for the same next-block inclusion, which is why the columns are 10/30/60 rather
than 10/50/90.

Two rules sit on top of the medians:

- **The tiers are monotone.** Each column drops its zero rewards before the
  median, so a thin low column can median above a fuller higher one (eight
  blocks of ten had a zero p10 that day). A "slow" that costs more than
  "normal" is lifted to it, never the other way round.
- **Blank reward rows with a base fee fall back to the node's own tip.** A 1559
  chain whose recent blocks were empty, or a proxy dialect that returns no
  rewards, still reports `baseFeePerGas`; the tiers are then priced off
  `eth_maxPriorityFeePerGas` (`source: "maxPriorityFee"`). Only a chain with no
  base fee at all falls back to `eth_gasPrice` with a **zero tip** — on a
  pre-1559 chain a priority fee is not meaningful, and emitting the gas price
  there is the original bug.
- **A read that fails is an error, never a guess.** A fee history or a node tip
  that cannot be read makes the answer `ok: false`. Swallowed, a failed history
  looked like a chain with no base fee: measured on mainnet, every tier went out
  as a type-2 transaction with a zero tip.

## API

```
suggest_fees(chain_id)                   -> { ok, chainId, baseFeePerGas, source, tiers: { slow, normal, fast } }
estimate(chain_id, request_json)         -> { ok, chainId, maxFeePerGas, maxPriorityFeePerGas, gasLimit, gasSource,
                                              feeCeilingWei(+Display/Exact), totalWei, baseFeePerGas, source }
estimate_bundle(chain_id, request_json)  -> { ok, chainId, maxFeePerGas, maxPriorityFeePerGas, gasLimit,
                                              feeCeilingWei(+Display/Exact), calls: [ { gasLimit, gasSource,
                                              feeCeilingWei(+Display/Exact) } ], assumptions, baseFeePerGas,
                                              source, nativeDecimals }
```

`source` is `"feeHistory"`, `"maxPriorityFee"`, `"gasPrice"` (legacy fallback) or
`"custom"`. All **wei** values are decimal strings — JSON numbers cannot carry
256 bits. `gasLimit` is a JSON **number**: it is a bounded count, not a wei value.
`gasSource` is `"given"` (the caller's own limit), `"estimated"` (`eth_estimateGas`
against the chain as it is) or `"simulated"` (estimated under an earlier call's
approve, see below).

Every ceiling is `maxFeePerGas × gasLimit`, and every `feeCeilingWei` comes with
`feeCeilingWeiDisplay` (at most five places, truncated, `"<0.00001"` for dust)
and `feeCeilingWeiExact` (every digit) in the native unit, so a wallet, a sender
and a dapp all show the one figure this module computed rather than three of
their own. `totalWei` equals `feeCeilingWei` and stays for older callers.

`estimate` accepts, in order of precedence:

```jsonc
{ "maxFeePerGas": "...", "maxPriorityFeePerGas": "..." }  // obeyed verbatim
{ "tier": "slow" | "normal" | "fast" }                    // default: normal
{ "gasLimit": "..." }                                     // else estimated from "tx"
{ "tx": { ... } }                                         // eth_estimateGas
{ "deadlineMs": 5000 }                                    // bound this call
```

This module advises; it does not overrule. An explicit fee override is used as
given.

### Bundles

```jsonc
{ "from": "0x…",
  "calls": [ { "to": "0x…USDC", "data": "0x095ea7b3…", "label": "Approve USDC" },
             { "to": "0x…Router", "data": "0x5ae401dc…", "label": "Swap" } ],
  "tier": "normal", "deadlineMs": 12000 }
```

The calls leave in order from one account, and each one is estimated **as the
chain will find it**. `eth_estimateGas` on a swap behind its approval reverts
(`STF`), because the allowance does not exist yet — so an ERC-20
`approve(spender, amount)` in an earlier call becomes a state override on that
token's allowance slot for every later call. The slot is not assumed: Solidity
hashes `spender ‖ keccak(owner ‖ base)`, Vyper hashes `keccak(base ‖ owner) ‖
spender`, and `base` is wherever the mapping landed in that contract's layout.
One `eth_call` of `allowance(owner, spender)` with all 64 candidate slots set to
distinct sentinels answers which one the token reads (USDC: Solidity slot 10),
and only that slot is overridden. A USDT-style reset-then-set is estimated with
the reset in place.

Measured on a mainnet fork on 2026-09-13: the swap leg went from
`execution reverted: STF` to a 131,087-gas estimate under the override.

What the estimate took for granted is reported, per call, in `assumptions`:
`{ call, after, token, spender, allowance }` — "call 2 was estimated as if call 1
had set this allowance". A call with its own `gasLimit` is taken as given and
never estimated. The first call that cannot be estimated refuses the whole
bundle: `{ ok: false, error, call }`, the error naming the call and, if a
token's allowance slot could not be located, saying so. A call that depends on
an earlier effect that is not an ERC-20 approve must carry its own `gasLimit`.

Through the verified proxy no state override can be expressed, so a call behind
an approval refuses there exactly as it did before; `eth_simulateV1` would model
any sequence on nodes that serve it and is the next step if that ever matters.

## Design notes

- **`concurrency: "multi"` with `&self` from the first commit.** Every method
  makes blocking calls out to `eth_rpc_module`, so one slow chain must not
  stall a suggestion for another. The module holds no mutable state, so it is
  trivially `Sync`.
- **No network of its own.** All RPC goes through `eth_rpc_module`, which owns
  the single fail-closed proxy chokepoint. State overrides travel through its
  `raw_rpc`, as the three-parameter forms of `eth_call` and `eth_estimateGas`.
- **Bounded as a whole.** A bundle of N calls is up to N estimates plus a probe
  per token; the request's `deadlineMs` bounds their sum (20 s when absent, and
  a caller may only shorten it), and each round trip gets at most 8 s.
- **The `tx` sent to `eth_estimateGas` carries a zero fee cap.** Given a `tx`
  with no `gas`, the nimbus verified proxy prices the whole block gas limit
  against the sender's balance and refuses anything under ~0.27 ETH. A zero cap
  makes that check vacuous. Any fee field on the submitted `tx` is zeroed or
  dropped for the estimate only — the returned fee comes from the request's
  top-level `maxFeePerGas`/`maxPriorityFeePerGas` or the tier, untouched.
- **No alloy.** Fee arithmetic tops out near `maxFee * gasLimit` ~ 1e19 wei,
  which fits `u128`; only balances need 256 bits and this module never touches
  one. The dep set is `serde` + `serde_json` + `sha3` (keccak-256 for the slot
  arithmetic, nothing else).

## Tests

```bash
cd rust-lib && cargo test --no-default-features   # pure algorithm, slots and bundle rules, no Logos runtime
```
