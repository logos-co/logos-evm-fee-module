//! Logos module glue for `fee_module` (rust-first authoring).
//!
//! The builder derives the `.lidl` from the `FeeModule` trait below
//! (`codegen.rust = { trait, source: "src/glue.rs" }`). Compiled only with the
//! default `logos_module` feature; `cargo test --no-default-features` exercises
//! the pure [`crate::estimator`] without the Logos runtime.
//!
//! `concurrency: "multi"` (metadata.json): every method here makes a blocking
//! call out to `eth_rpc_module`, so the module opts into concurrent dispatch —
//! one slow chain cannot stall a suggestion for another. The multi contract
//! makes the generated trait take `&self` + `Send + Sync`, which is why this
//! module holds NO mutable state at all: it is a pure function of what
//! `eth_rpc_module` reports. Written that way from the first commit, because
//! retrofitting `multi` onto a `&mut self` module is a refactor, not a flag.

use serde_json::{json, Value};

use crate::estimator::{self, FeeHistory, FeeSuggestion, TIERS};

/// Blocks of history to sample. Long enough that a single empty block does not
/// swing the median, short enough to still track a moving base fee.
const HISTORY_BLOCKS: i64 = 10;

pub trait FeeModule: Send + Sync + 'static {
    /// Slow/normal/fast suggestions for a chain, derived from `eth_feeHistory`.
    ///
    /// `{ ok, chainId, baseFeePerGas, source, tiers: { slow, normal, fast } }`
    /// where each tier is `{ maxFeePerGas, maxPriorityFeePerGas }` as decimal
    /// wei strings. `source` is `"feeHistory"` or `"gasPrice"` so a caller can
    /// tell a real EIP-1559 suggestion from the legacy fallback.
    fn suggest_fees(&self, chain_id: i64) -> String;

    /// Resolve a concrete fee for a send, honouring any override.
    ///
    /// `request_json` accepts:
    ///   `{ "tier": "slow"|"normal"|"fast" }`            — pick a suggested tier
    ///   `{ "maxFeePerGas": "...", "maxPriorityFeePerGas": "..." }` — override
    ///   `{ "gasLimit": "..." }`                          — override the limit
    ///   `{ "tx": { ... } }`                              — estimate the limit
    ///
    /// Returns `{ ok, maxFeePerGas, maxPriorityFeePerGas, gasLimit, totalWei,
    /// source }`. An explicit fee override is used verbatim — this module
    /// advises, it does not overrule the user.
    fn estimate(&self, chain_id: i64, request_json: String) -> String;

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
    let s = v.as_str()?;
    match s.strip_prefix("0x") {
        Some(h) => u128::from_str_radix(h, 16).ok(),
        None => s.parse::<u128>().ok(),
    }
}

impl FeeModuleImpl {
    /// Pull fee history for every tier percentile in ONE call.
    fn history(&self, chain_id: i64) -> Result<FeeHistory, String> {
        let percentiles: Vec<f64> = TIERS.iter().map(|t| t.reward_percentile).collect();
        let reply = modules()
            .eth_rpc_module
            .fee_history(chain_id, HISTORY_BLOCKS, &json!(percentiles).to_string())
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

    fn gas_price(&self, chain_id: i64) -> Result<u128, String> {
        let reply = modules().eth_rpc_module.gas_price(chain_id).map_err(|e| format!("{e:?}"))?;
        Ok(hex_u128(&inner(&reply)?))
    }

    /// Suggestions per tier, with the legacy fallback when a chain has no
    /// usable fee history.
    fn tiers(&self, chain_id: i64) -> Result<(Vec<FeeSuggestion>, u128, &'static str), String> {
        let h = self.history(chain_id).unwrap_or_default();
        // A successful-but-EMPTY body is a miss, not a suggestion: the verified
        // proxy answers eth_feeHistory with success:true and every array empty
        // when blockCount is a hex string. No error to catch -- so the check
        // lives in the estimator, where it is unit-tested, rather than here.
        let usable = estimator::is_usable(&h);
        if usable {
            let base = estimator::next_base_fee(&h);
            Ok((TIERS.iter().enumerate().map(|(i, t)| estimator::suggest(&h, t, i)).collect(), base, "feeHistory"))
        } else {
            let gp = self.gas_price(chain_id)?;
            Ok((TIERS.iter().map(|t| estimator::suggest_legacy(gp, t)).collect(), 0u128, "gasPrice"))
        }
    }
}

impl FeeModule for FeeModuleImpl {
    fn suggest_fees(&self, chain_id: i64) -> String {
        let (sugg, base, source) = match self.tiers(chain_id) {
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
        let req: Value = match serde_json::from_str(&request_json) {
            Ok(v) => v,
            Err(_) => json!({}),
        };

        // A caller that supplies BOTH fee fields is obeyed verbatim; we advise,
        // we do not overrule. Anything missing falls back to the named tier.
        let over_max = req.get("maxFeePerGas").and_then(any_u128);
        let over_tip = req.get("maxPriorityFeePerGas").and_then(any_u128);

        let (fee, source) = if let (Some(m), Some(p)) = (over_max, over_tip) {
            (FeeSuggestion { max_fee_per_gas: m, max_priority_fee_per_gas: p }, "custom")
        } else {
            let wanted = req.get("tier").and_then(Value::as_str).unwrap_or("normal");
            let idx = TIERS.iter().position(|t| t.name == wanted).unwrap_or(1);
            match self.tiers(chain_id) {
                Ok((sugg, _, src)) => (sugg[idx].clone(), src),
                Err(e) => return err(e),
            }
        };

        // Gas limit: an explicit override, else estimate_gas on the supplied tx.
        let gas_limit = if let Some(g) = req.get("gasLimit").and_then(any_u128) {
            g
        } else if let Some(tx) = req.get("tx") {
            match modules().eth_rpc_module.estimate_gas(chain_id, &tx.to_string()) {
                Ok(reply) => match inner(&reply) {
                    Ok(v) => hex_u128(&v),
                    Err(e) => return err(e),
                },
                Err(e) => return err(format!("{e:?}")),
            }
        } else {
            0u128
        };

        json!({
            "ok": true,
            "maxFeePerGas": fee.max_fee_per_gas.to_string(),
            "maxPriorityFeePerGas": fee.max_priority_fee_per_gas.to_string(),
            "gasLimit": gas_limit.to_string(),
            "totalWei": fee.max_fee_per_gas.saturating_mul(gas_limit).to_string(),
            "source": source,
        })
        .to_string()
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
