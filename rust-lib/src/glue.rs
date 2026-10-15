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
use std::time::{Duration, Instant};

use logos_rust_sdk::LogosError;
use serde_json::{json, Map, Value};

use crate::budget::{self, Budget, CONFIG_READ};
use crate::bundle::{self, Call};
use crate::estimator::{self, FeeHistory, FeeSuggestion, Pricing, TIERS};
use crate::failure::{self, Failure};
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
    /// An explicit fee field is used verbatim — this module advises, it does not
    /// overrule the user. With one set, the other comes from the tier.
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

/// A refusal with its machine code beside the sentence, when it has one.
fn refused(f: Failure) -> Value {
    let mut v = json!({ "ok": false, "error": f.why });
    if !f.code.is_empty() {
        v["code"] = json!(f.code);
    }
    v
}

/// One request's chain as eth_rpc has it configured, and the allowance every round trip on it
/// shares. A refusal names the endpoint and the timeout the user set for it.
struct Chain {
    id: i64,
    endpoint: String,
    budget: Budget,
}

impl Chain {
    /// The round trip and the name come from eth_rpc's config, a local read charged to the same
    /// allowance. An unreadable one leaves eth_rpc's defaults.
    fn open(id: i64, rounds: u32, deadline_ms: Option<i64>) -> Self {
        let started = Instant::now();
        let config = modules()
            .eth_rpc_module
            .get_chain_config_with_timeout(id, CONFIG_READ)
            .ok()
            .and_then(|r| serde_json::from_str::<Value>(&r).ok())
            .and_then(|v| v.get("config").cloned())
            .unwrap_or(Value::Null);
        let name = config.get("name").and_then(Value::as_str).map(str::to_string)
            .unwrap_or_else(|| format!("chain {id}"));
        let verified = config.get("verifiedProxyMode").and_then(Value::as_str) == Some("required");
        let budget = Budget::for_chain(started, budget::round_trip(&config), rounds, deadline_ms);
        Chain { id, endpoint: failure::endpoint(&name, verified), budget }
    }

    /// One round trip, started only on a whole one: eth_rpc's reply, or why there was none.
    fn ask(&self, what: &str, call: impl FnOnce(Duration) -> Result<String, LogosError>) -> Result<String, Failure> {
        let rt = self.budget.round_trip();
        let t = self.budget.take().map_err(|left| self.failed(what, failure::no_time(left, &self.endpoint, rt)))?;
        call(t).map_err(|e| self.failed(what, match &e {
            LogosError::PluginCallFailed { message, .. } => failure::transport(message, &self.endpoint, rt),
            other => Failure { code: "transport".into(), why: format!("eth_rpc_module could not be asked ({other})") },
        }))
    }

    /// The reply's result, or eth_rpc's refusal in words.
    fn answer(&self, what: &str, reply: &str) -> Result<Value, Failure> {
        match failure::refusal(reply, &self.endpoint, self.budget.round_trip()) {
            Some(f) => Err(self.failed(what, f)),
            None => inner(reply).map_err(Failure::from),
        }
    }

    fn failed(&self, what: &str, f: Failure) -> Failure {
        Failure { code: f.code, why: format!("Couldn't {what}: {}.", f.why) }
    }
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

#[derive(Clone)]
enum Slot {
    At(String),
    Unlocated,
    /// The route refused the probe's override, so no override will be taken.
    NoOverrides,
}

type SlotCache = HashMap<(String, String), Slot>;

impl FeeModuleImpl {
    /// Pull fee history for every tier percentile in ONE call.
    fn history(&self, c: &Chain) -> Result<FeeHistory, Failure> {
        let what = "read the fee history";
        let percentiles = json!(TIERS.iter().map(|t| t.reward_percentile).collect::<Vec<f64>>()).to_string();
        let reply = c.ask(what, |t| {
            modules().eth_rpc_module.fee_history_with_timeout(c.id, HISTORY_BLOCKS, &percentiles, t)
        })?;
        let r = c.answer(what, &reply)?;
        Ok(FeeHistory {
            base_fee_per_gas: r["baseFeePerGas"].as_array().map(|a| a.iter().map(hex_u128).collect()).unwrap_or_default(),
            reward: r["reward"]
                .as_array()
                .map(|rows| rows.iter().map(|row| row.as_array().map(|c| c.iter().map(hex_u128).collect()).unwrap_or_default()).collect())
                .unwrap_or_default(),
            gas_used_ratio: r["gasUsedRatio"].as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default(),
        })
    }

    fn gas_price(&self, c: &Chain) -> Result<u128, Failure> {
        let what = "read the gas price";
        let reply = c.ask(what, |t| modules().eth_rpc_module.gas_price_with_timeout(c.id, t))?;
        Ok(hex_u128(&c.answer(what, &reply)?))
    }

    /// The node's own tip suggestion.
    fn node_tip(&self, c: &Chain) -> Result<u128, Failure> {
        let what = "read the node's tip";
        let reply = c.ask(what, |t| {
            modules().eth_rpc_module.raw_rpc_with_timeout(c.id, "eth_maxPriorityFeePerGas", "[]", t)
        })?;
        Ok(hex_u128(&c.answer(what, &reply)?))
    }

    /// Suggestions per tier, priced as [`estimator::pricing`] decides. A history or a tip
    /// that cannot be read is the answer's error, never a zero tip.
    fn tiers(&self, c: &Chain) -> Result<(Vec<FeeSuggestion>, u128, &'static str), Failure> {
        let read: Result<FeeHistory, String> = Ok(self.history(c)?);
        match estimator::pricing(&read)? {
            Pricing::Rewards(h) => {
                let mut v: Vec<FeeSuggestion> = TIERS.iter().enumerate().map(|(i, t)| estimator::suggest(h, t, i)).collect();
                estimator::monotone(&mut v);
                Ok((v, estimator::next_base_fee(h), "feeHistory"))
            }
            Pricing::NodeTip(base) => {
                let tip = self.node_tip(c)?;
                Ok((TIERS.iter().map(|t| estimator::suggest_with_tip(base, tip, t)).collect(), base, "maxPriorityFee"))
            }
            Pricing::GasPrice => {
                let gp = self.gas_price(c)?;
                Ok((TIERS.iter().map(|t| estimator::suggest_legacy(gp, t)).collect(), 0u128, "gasPrice"))
            }
        }
    }

    /// A fee field the caller set is used as given; we advise, we do not overrule. Both set,
    /// the chain is not asked; one set, the other comes from the named tier
    /// ([`estimator::with_overrides`]). Either set, the source is `custom`.
    fn fee_for(&self, c: &Chain, req: &Value) -> Result<Priced, Failure> {
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
        let (sugg, base, source) = self.tiers(c)?;
        let fee = estimator::with_overrides(&sugg[idx], over_max, over_tip)?;
        let source = if over_max.is_some() || over_tip.is_some() { "custom" } else { source };
        Ok(Priced { fee, source, base })
    }

    /// The slot a token keeps `allowance[owner][spender]` in, asked once per pair.
    fn allowance_slot(&self, c: &Chain, a: &Approval, owner: &str, cache: &mut SlotCache) -> Slot {
        let key = (a.token.clone(), a.spender.clone());
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
        let found = (|| {
            let p = slots::probe(&a.token, owner, &a.spender).ok_or(Slot::Unlocated)?;
            let t = c.budget.take().map_err(|_| Slot::Unlocated)?;
            let params = json!([p.call, "latest", p.overrides]).to_string();
            let reply = modules().eth_rpc_module.raw_rpc_with_timeout(c.id, "eth_call", &params, t).map_err(|_| Slot::Unlocated)?;
            if bundle::refused_with(&reply, bundle::OVERRIDES_UNSUPPORTED) {
                return Err(Slot::NoOverrides);
            }
            let answer = inner(&reply).map_err(|_| Slot::Unlocated)?;
            answer.as_str().and_then(|a| slots::identify(&p, a)).map(Slot::At).ok_or(Slot::Unlocated)
        })()
        .unwrap_or_else(|s| s);
        cache.insert(key, found.clone());
        found
    }

    /// `eth_estimateGas` for call `i`, under the allowances the calls before it set.
    fn estimate_call(
        &self,
        c: &Chain,
        from: &str,
        calls: &[Call],
        i: usize,
        cache: &mut SlotCache,
        assumptions: &mut Vec<Value>,
    ) -> Result<Gas, Failure> {
        let call = &calls[i];
        if let Some(g) = call.gas_limit {
            return Ok(Gas { limit: g, source: "given" });
        }
        let name = bundle::call_name(i, call);
        let mut diffs: Map<String, Value> = Map::new();
        let mut unlocated: Vec<String> = Vec::new();
        let approvals = bundle::approvals_before(calls, i);
        let fallback = |assumptions: &mut Vec<Value>| -> Result<Gas, Failure> {
            let (j, a) = &approvals[0];
            let limit = bundle::fallback_for(call, &name, j + 1)?;
            assumptions.push(json!({
                "call": i + 1, "after": j + 1, "token": a.token, "spender": a.spender,
                "gasLimitFallback": limit, "reason": "this route cannot take a state override",
            }));
            Ok(Gas { limit, source: "fallback" })
        };
        for (j, a) in approvals.iter().cloned() {
            match self.allowance_slot(c, &a, from, cache) {
                Slot::NoOverrides => return fallback(assumptions),
                Slot::At(slot) => {
                    let entry = diffs.entry(a.token.clone()).or_insert_with(|| json!({ "stateDiff": {} }));
                    entry["stateDiff"][slot] = json!(a.amount_word);
                    assumptions.push(json!({
                        "call": i + 1, "after": j + 1,
                        "token": a.token, "spender": a.spender, "allowance": a.amount(),
                    }));
                }
                Slot::Unlocated => unlocated.push(format!("the allowance {} keeps for {} could not be located, so call {} was not modelled", a.token, a.spender, j + 1)),
            }
        }
        let tx = for_estimate(&call.tx(from));
        let what = format!("estimate {name}");
        let reply = c.ask(&what, |t| {
            if diffs.is_empty() {
                modules().eth_rpc_module.estimate_gas_with_timeout(c.id, &tx.to_string(), t)
            } else {
                let params = json!([tx, "latest", Value::Object(diffs.clone())]).to_string();
                modules().eth_rpc_module.raw_rpc_with_timeout(c.id, "eth_estimateGas", &params, t)
            }
        })?;
        if !diffs.is_empty() && bundle::refused_with(&reply, bundle::OVERRIDES_UNSUPPORTED) {
            return fallback(assumptions);
        }
        // A timeout is the endpoint's, not the call's: no gasLimit advice under it.
        let refusal = failure::refusal(&reply, &c.endpoint, c.budget.round_trip());
        if let Some(f) = refusal.clone().filter(|f| f.code == "timeout") {
            return Err(c.failed(&what, f));
        }
        let code = refusal.map(|f| f.code).unwrap_or_default();
        let gas = inner(&reply).map_err(|why| {
            let mut msg = format!("{name} could not be estimated: {why}");
            for u in &unlocated {
                msg.push_str("; ");
                msg.push_str(u);
            }
            msg.push_str("; give it a gasLimit if it depends on an earlier call's effect that is not an ERC-20 approve");
            Failure { code, why: msg }
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
        let c = Chain::open(chain_id, 2, None);
        let (sugg, base, source) = match self.tiers(&c) {
            Ok(v) => v,
            Err(e) => return refused(e).to_string(),
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
        let c = Chain::open(chain_id, 3, req.get("deadlineMs").and_then(Value::as_i64));
        let priced = match self.fee_for(&c, &req) {
            Ok(p) => p,
            Err(e) => return refused(e).to_string(),
        };

        // Gas limit: an explicit override, else estimate_gas on the supplied tx. The tx is
        // the caller's own; only its fee fields are rewritten for the estimate.
        let gas = if let Some(g) = req.get("gasLimit").and_then(any_u128) {
            Gas { limit: u64::try_from(g).unwrap_or(u64::MAX), source: "given" }
        } else if let Some(tx) = req.get("tx") {
            let what = "estimate the call";
            let tx = for_estimate(tx).to_string();
            match c.ask(what, |t| modules().eth_rpc_module.estimate_gas_with_timeout(chain_id, &tx, t))
                .and_then(|reply| c.answer(what, &reply))
            {
                Ok(v) => Gas { limit: u64::try_from(hex_u128(&v)).unwrap_or(u64::MAX), source: "estimated" },
                Err(e) => return refused(e).to_string(),
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
        let from = req.get("from").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if !is_address(from) {
            return err("`from` is not an address");
        }
        let calls = match bundle::parse_calls(&req) {
            Ok(c) => c,
            Err(e) => return err(e),
        };
        // The history, a tip, an estimate per call and an allowance probe per call after the first.
        let rounds = 1 + 2 * calls.len() as u32;
        let c = Chain::open(chain_id, rounds, req.get("deadlineMs").and_then(Value::as_i64));
        let priced = match self.fee_for(&c, &req) {
            Ok(p) => p,
            Err(e) => return refused(e).to_string(),
        };
        let mut cache = SlotCache::new();
        let mut assumptions = Vec::new();
        let mut gas = Vec::with_capacity(calls.len());
        for i in 0..calls.len() {
            match self.estimate_call(&c, from, &calls, i, &mut cache, &mut assumptions) {
                Ok(g) => gas.push(g),
                Err(e) => {
                    let mut v = refused(e);
                    v["call"] = json!(i + 1);
                    return v.to_string();
                }
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
