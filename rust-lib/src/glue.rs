//! Logos module glue for `fee_module` (rust-first authoring).
//!
//! The builder derives the `.lidl` from the `FeeModule` trait below
//! (`codegen.rust = { trait, source: "src/glue.rs" }`). Compiled only with the
//! default `logos_module` feature; `cargo test --no-default-features` exercises
//! the pure modules without the Logos runtime.
//!
//! `concurrency: "multi"` (metadata.json): every method here makes blocking
//! calls out to `eth_rpc_module`, so the module opts into concurrent dispatch —
//! one slow chain cannot stall a suggestion for another. The multi contract
//! makes the generated trait take `&self` + `Send + Sync`, which is why this
//! module holds NO mutable state at all: it is a pure function of what
//! `eth_rpc_module` reports.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::budget::Budget;
use crate::bundle::{self, Call};
use crate::estimator::{self, FeeHistory, FeeSuggestion, Pricing, TIERS};
use crate::slots::{self, Approval};
use crate::tx::for_estimate;
use crate::units;

/// Blocks of history to sample. Long enough that a single empty block does not
/// swing the median, short enough to still track a moving base fee.
const HISTORY_BLOCKS: i64 = 10;

pub trait FeeModule: Send + Sync + 'static {
    /// Slow/normal/fast suggestions for a chain, derived from `eth_feeHistory`.
    ///
    /// `{ ok, chainId, baseFeePerGas, source, tiers: { slow, normal, fast } }`
    /// where each tier is `{ maxFeePerGas, maxPriorityFeePerGas }` as decimal
    /// wei strings. `source` is `"feeHistory"`, `"maxPriorityFee"` (a 1559 chain
    /// whose reward rows were blank, priced off the node's own tip) or
    /// `"gasPrice"` (a chain with no base fee). A fee history that cannot be read
    /// is `ok: false`.
    fn suggest_fees(&self, chain_id: i64) -> String;

    /// Resolve a concrete fee for ONE call, honouring any override.
    ///
    /// `request_json` accepts:
    ///   `{ "tier": "slow"|"normal"|"fast" }`            — pick a suggested tier
    ///   `{ "maxFeePerGas": "...", "maxPriorityFeePerGas": "..." }` — override
    ///   `{ "gasLimit": "..." }`                          — override the limit
    ///   `{ "tx": { ... } }`                              — estimate the limit
    ///   `{ "deadlineMs": 5000 }`                         — bound this call
    ///
    /// Returns `{ ok, chainId, maxFeePerGas, maxPriorityFeePerGas, gasLimit,
    /// gasSource, feeCeilingWei(+Display/Exact), totalWei, baseFeePerGas,
    /// source }`. `totalWei` equals `feeCeilingWei` and stays for older callers.
    /// An explicit fee override is used verbatim — this module advises, it does
    /// not overrule the user.
    fn estimate(&self, chain_id: i64, request_json: String) -> String;

    /// Price a bundle of calls that will leave in order, from one account.
    ///
    /// `request_json`: `{ from, calls: [{ to, value?, data?, gasLimit?, label? }],
    /// tier? | maxFeePerGas? + maxPriorityFeePerGas?, deadlineMs? }`.
    ///
    /// Each call is estimated as the chain will find it: an ERC-20 `approve` in an
    /// earlier call becomes a state override on that token's allowance slot for
    /// every later call, so a swap behind its approval gets a real estimate and a
    /// USDT-style reset-then-set is estimated with the reset applied. A call with
    /// its own `gasLimit` is taken as given. The first call that cannot be
    /// estimated refuses the bundle, naming it.
    ///
    /// Returns `{ ok, chainId, source, baseFeePerGas, maxFeePerGas,
    /// maxPriorityFeePerGas, gasLimit, feeCeilingWei(+Display/Exact),
    /// calls: [{ gasLimit, gasSource: "given"|"estimated"|"simulated",
    /// feeCeilingWei(+Display/Exact) }], assumptions: [{ call, after, token,
    /// spender, allowance }], nativeDecimals }`. One fee for the bundle; every
    /// ceiling is `maxFeePerGas × gasLimit`, in wei and in the native unit.
    fn estimate_bundle(&self, chain_id: i64, request_json: String) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct FeeModuleImpl;

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

/// eth_rpc wraps every reply as `{ ok, result }`. Unwrap to the inner result.
fn inner(reply: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(reply).map_err(|e| e.to_string())?;
    if v.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(v.get("error").and_then(Value::as_str).unwrap_or("rpc failed").to_string());
    }
    Ok(v.get("result").cloned().unwrap_or(v))
}

/// Hex quantity (`0x…`) to u128. Fee history is all hex quantities.
fn hex_u128(v: &Value) -> u128 {
    v.as_str()
        .and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0)
}

/// Decimal-or-hex wei string to u128 — callers may pass either.
fn any_u128(v: &Value) -> Option<u128> {
    v.as_str().and_then(bundle::parse_quantity)
}

fn is_address(s: &str) -> bool {
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or("");
    h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The fee a request resolves to, and where it came from.
struct Priced {
    fee: FeeSuggestion,
    source: &'static str,
    base: u128,
}

/// One call's gas limit and how it was obtained.
struct Gas {
    limit: u64,
    source: &'static str,
}

type SlotCache = HashMap<(String, String), Option<String>>;

impl FeeModuleImpl {
    fn grant(b: &Budget, what: &str) -> Result<Duration, String> {
        b.take().ok_or_else(|| format!("no time left to {what}"))
    }

    /// Pull fee history for every tier percentile in ONE call.
    fn history(&self, chain_id: i64, b: &Budget) -> Result<FeeHistory, String> {
        let t = Self::grant(b, "read the fee history")?;
        let percentiles: Vec<f64> = TIERS.iter().map(|t| t.reward_percentile).collect();
        let reply = modules()
            .eth_rpc_module
            .fee_history_with_timeout(chain_id, HISTORY_BLOCKS, &json!(percentiles).to_string(), t)
            .map_err(|e| format!("{e:?}"))?;
        let r = inner(&reply)?;
        Ok(FeeHistory {
            base_fee_per_gas: r["baseFeePerGas"].as_array().map(|a| a.iter().map(hex_u128).collect()).unwrap_or_default(),
            reward: r["reward"]
                .as_array()
                .map(|rows| rows.iter().map(|row| row.as_array().map(|c| c.iter().map(hex_u128).collect()).unwrap_or_default()).collect())
                .unwrap_or_default(),
            gas_used_ratio: r["gasUsedRatio"].as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default(),
        })
    }

    fn gas_price(&self, chain_id: i64, b: &Budget) -> Result<u128, String> {
        let t = Self::grant(b, "read the gas price")?;
        let reply = modules().eth_rpc_module.gas_price_with_timeout(chain_id, t).map_err(|e| format!("{e:?}"))?;
        Ok(hex_u128(&inner(&reply)?))
    }

    /// The node's own tip suggestion.
    fn node_tip(&self, chain_id: i64, b: &Budget) -> Result<u128, String> {
        let t = Self::grant(b, "read the node's tip")?;
        let reply = modules()
            .eth_rpc_module
            .raw_rpc_with_timeout(chain_id, "eth_maxPriorityFeePerGas", "[]", t)
            .map_err(|e| format!("{e:?}"))?;
        Ok(hex_u128(&inner(&reply)?))
    }

    /// Suggestions per tier, priced as [`estimator::pricing`] decides. A history or a tip
    /// that cannot be read is the answer's error, never a zero tip.
    fn tiers(&self, chain_id: i64, b: &Budget) -> Result<(Vec<FeeSuggestion>, u128, &'static str), String> {
        let read = self.history(chain_id, b);
        match estimator::pricing(&read)? {
            Pricing::Rewards(h) => {
                let mut v: Vec<FeeSuggestion> = TIERS.iter().enumerate().map(|(i, t)| estimator::suggest(h, t, i)).collect();
                estimator::monotone(&mut v);
                Ok((v, estimator::next_base_fee(h), "feeHistory"))
            }
            Pricing::NodeTip(base) => {
                let tip = self.node_tip(chain_id, b)?;
                Ok((TIERS.iter().map(|t| estimator::suggest_with_tip(base, tip, t)).collect(), base, "maxPriorityFee"))
            }
            Pricing::GasPrice => {
                let gp = self.gas_price(chain_id, b)?;
                Ok((TIERS.iter().map(|t| estimator::suggest_legacy(gp, t)).collect(), 0u128, "gasPrice"))
            }
        }
    }

    /// A caller that supplies BOTH fee fields is obeyed verbatim; we advise, we do not
    /// overrule. Anything missing falls back to the named tier.
    fn fee_for(&self, chain_id: i64, req: &Value, b: &Budget) -> Result<Priced, String> {
        let over_max = req.get("maxFeePerGas").and_then(any_u128);
        let over_tip = req.get("maxPriorityFeePerGas").and_then(any_u128);
        if let (Some(m), Some(p)) = (over_max, over_tip) {
            if p > m {
                return Err("maxPriorityFeePerGas cannot exceed maxFeePerGas".into());
            }
            return Ok(Priced { fee: FeeSuggestion { max_fee_per_gas: m, max_priority_fee_per_gas: p }, source: "custom", base: 0 });
        }
        let wanted = req.get("tier").and_then(Value::as_str).unwrap_or("normal");
        let idx = TIERS.iter().position(|t| t.name == wanted).unwrap_or(1);
        let (sugg, base, source) = self.tiers(chain_id, b)?;
        Ok(Priced { fee: sugg[idx].clone(), source, base })
    }

    /// The slot a token keeps `allowance[owner][spender]` in, asked once per pair.
    fn allowance_slot(&self, chain_id: i64, a: &Approval, owner: &str, cache: &mut SlotCache, b: &Budget) -> Option<String> {
        let key = (a.token.clone(), a.spender.clone());
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
        let found = (|| {
            let p = slots::probe(&a.token, owner, &a.spender)?;
            let t = b.take()?;
            let params = json!([p.call, "latest", p.overrides]).to_string();
            let reply = modules().eth_rpc_module.raw_rpc_with_timeout(chain_id, "eth_call", &params, t).ok()?;
            let answer = inner(&reply).ok()?;
            slots::identify(&p, answer.as_str()?)
        })();
        cache.insert(key, found.clone());
        found
    }

    /// `eth_estimateGas` for call `i`, under the allowances the calls before it set.
    fn estimate_call(
        &self,
        chain_id: i64,
        from: &str,
        calls: &[Call],
        i: usize,
        cache: &mut SlotCache,
        assumptions: &mut Vec<Value>,
        b: &Budget,
    ) -> Result<Gas, String> {
        let call = &calls[i];
        if let Some(g) = call.gas_limit {
            return Ok(Gas { limit: g, source: "given" });
        }
        let name = bundle::call_name(i, call);
        let mut diffs: Map<String, Value> = Map::new();
        let mut unlocated: Vec<String> = Vec::new();
        for (j, a) in bundle::approvals_before(calls, i) {
            match self.allowance_slot(chain_id, &a, from, cache, b) {
                Some(slot) => {
                    let entry = diffs.entry(a.token.clone()).or_insert_with(|| json!({ "stateDiff": {} }));
                    entry["stateDiff"][slot] = json!(a.amount_word);
                    assumptions.push(json!({
                        "call": i + 1, "after": j + 1,
                        "token": a.token, "spender": a.spender, "allowance": a.amount(),
                    }));
                }
                None => unlocated.push(format!("the allowance {} keeps for {} could not be located, so call {} was not modelled", a.token, a.spender, j + 1)),
            }
        }
        let tx = for_estimate(&call.tx(from));
        let t = Self::grant(b, &format!("estimate {name}"))?;
        let reply = if diffs.is_empty() {
            modules().eth_rpc_module.estimate_gas_with_timeout(chain_id, &tx.to_string(), t)
        } else {
            let params = json!([tx, "latest", Value::Object(diffs.clone())]).to_string();
            modules().eth_rpc_module.raw_rpc_with_timeout(chain_id, "eth_estimateGas", &params, t)
        }
        .map_err(|e| format!("{name} could not be estimated: {e:?}"))?;
        let gas = inner(&reply).map_err(|why| {
            let mut msg = format!("{name} could not be estimated: {why}");
            for u in &unlocated {
                msg.push_str("; ");
                msg.push_str(u);
            }
            msg.push_str("; give it a gasLimit if it depends on an earlier call's effect that is not an ERC-20 approve");
            msg
        })?;
        Ok(Gas {
            limit: u64::try_from(hex_u128(&gas)).unwrap_or(u64::MAX),
            source: if diffs.is_empty() { "estimated" } else { "simulated" },
        })
    }

    fn bundle_reply(chain_id: i64, priced: &Priced, gas: &[Gas], assumptions: Vec<Value>) -> Value {
        let mut calls = Vec::with_capacity(gas.len());
        let mut total_gas: u64 = 0;
        let mut total_ceiling: u128 = 0;
        for g in gas {
            let ceiling = priced.fee.max_fee_per_gas.saturating_mul(u128::from(g.limit));
            let mut c = json!({ "gasLimit": g.limit, "gasSource": g.source });
            units::decorate(&mut c, "feeCeilingWei", ceiling);
            calls.push(c);
            total_gas = total_gas.saturating_add(g.limit);
            total_ceiling = total_ceiling.saturating_add(ceiling);
        }
        let mut v = json!({
            "ok": true,
            "chainId": chain_id,
            "source": priced.source,
            "baseFeePerGas": priced.base.to_string(),
            "maxFeePerGas": priced.fee.max_fee_per_gas.to_string(),
            "maxPriorityFeePerGas": priced.fee.max_priority_fee_per_gas.to_string(),
            // A NUMBER, not a string: a gas limit is a bounded count, and every consumer
            // reads it as one. Wei values are strings because 256 bits do not fit a number.
            "gasLimit": total_gas,
            "calls": calls,
            "assumptions": assumptions,
            "nativeDecimals": units::NATIVE_DECIMALS,
        });
        units::decorate(&mut v, "feeCeilingWei", total_ceiling);
        v
    }
}

impl FeeModule for FeeModuleImpl {
    fn suggest_fees(&self, chain_id: i64) -> String {
        let b = Budget::bounded_by(None);
        let (sugg, base, source) = match self.tiers(chain_id, &b) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        let mut tiers = serde_json::Map::new();
        for (t, s) in TIERS.iter().zip(sugg.iter()) {
            tiers.insert(
                t.name.to_string(),
                json!({
                    "maxFeePerGas": s.max_fee_per_gas.to_string(),
                    "maxPriorityFeePerGas": s.max_priority_fee_per_gas.to_string(),
                }),
            );
        }
        json!({ "ok": true, "chainId": chain_id, "baseFeePerGas": base.to_string(),
                "source": source, "tiers": tiers })
        .to_string()
    }

    fn estimate(&self, chain_id: i64, request_json: String) -> String {
        let req: Value = serde_json::from_str(&request_json).unwrap_or_else(|_| json!({}));
        let b = Budget::bounded_by(req.get("deadlineMs").and_then(Value::as_i64));
        let priced = match self.fee_for(chain_id, &req, &b) {
            Ok(p) => p,
            Err(e) => return err(e),
        };

        // Gas limit: an explicit override, else estimate_gas on the supplied tx. The tx is
        // the caller's own; only its fee fields are rewritten for the estimate.
        let gas = if let Some(g) = req.get("gasLimit").and_then(any_u128) {
            Gas { limit: u64::try_from(g).unwrap_or(u64::MAX), source: "given" }
        } else if let Some(tx) = req.get("tx") {
            let t = match Self::grant(&b, "estimate the call") {
                Ok(t) => t,
                Err(e) => return err(e),
            };
            match modules().eth_rpc_module.estimate_gas_with_timeout(chain_id, &for_estimate(tx).to_string(), t) {
                Ok(reply) => match inner(&reply) {
                    Ok(v) => Gas { limit: u64::try_from(hex_u128(&v)).unwrap_or(u64::MAX), source: "estimated" },
                    Err(e) => return err(e),
                },
                Err(e) => return err(format!("{e:?}")),
            }
        } else {
            Gas { limit: 0, source: "none" }
        };

        let ceiling = priced.fee.max_fee_per_gas.saturating_mul(u128::from(gas.limit));
        let mut v = json!({
            "ok": true,
            "chainId": chain_id,
            "maxFeePerGas": priced.fee.max_fee_per_gas.to_string(),
            "maxPriorityFeePerGas": priced.fee.max_priority_fee_per_gas.to_string(),
            "gasLimit": gas.limit,
            "gasSource": gas.source,
            "totalWei": ceiling.to_string(),
            "baseFeePerGas": priced.base.to_string(),
            "source": priced.source,
        });
        units::decorate(&mut v, "feeCeilingWei", ceiling);
        v.to_string()
    }

    fn estimate_bundle(&self, chain_id: i64, request_json: String) -> String {
        let req: Value = match serde_json::from_str(&request_json) {
            Ok(v) => v,
            Err(e) => return err(format!("request is not JSON: {e}")),
        };
        let b = Budget::bounded_by(req.get("deadlineMs").and_then(Value::as_i64));
        let from = req.get("from").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if !is_address(from) {
            return err("`from` is not an address");
        }
        let calls = match bundle::parse_calls(&req) {
            Ok(c) => c,
            Err(e) => return err(e),
        };
        let priced = match self.fee_for(chain_id, &req, &b) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let mut cache = SlotCache::new();
        let mut assumptions = Vec::new();
        let mut gas = Vec::with_capacity(calls.len());
        for i in 0..calls.len() {
            match self.estimate_call(chain_id, from, &calls, i, &mut cache, &mut assumptions, &b) {
                Ok(g) => gas.push(g),
                Err(e) => return json!({ "ok": false, "error": e, "call": i + 1 }).to_string(),
            }
        }
        Self::bundle_reply(chain_id, &priced, &gas, assumptions).to_string()
    }
}

// The registration hook. The generated provider glue DECLARES this symbol and
// the loader resolves it at dlopen; the author owes the definition. Omitting it
// links cleanly and produces a plugin that fails only on Linux, at load time,
// with `undefined symbol: logos_module_install` -- macOS resolves lazily and
// gives no hint. That is exactly how this module shipped its first build.
#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<FeeModuleImpl>();
}
