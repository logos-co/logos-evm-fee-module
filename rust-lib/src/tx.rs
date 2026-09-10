//! The transaction handed to `eth_estimateGas` — which is not the transaction
//! that gets signed. Pure, so it is unit-tested without the Logos runtime.

use serde_json::{json, Value};

/// An estimate-only copy of `tx` with the fee capped at zero.
///
/// Given no `gas`, the nimbus verified proxy prices the whole block gas limit
/// against the sender's balance before estimating, refusing anything under
/// ~0.27 ETH; a zero cap makes that check vacuous, on plain nodes too. Both
/// 1559 fields go to zero because a lone zero cap is rejected as
/// `maxFeePerGas (0x0) < maxPriorityFeePerGas`, and `gasPrice` is dropped
/// because it cannot be combined with either.
pub fn for_estimate(tx: &Value) -> Value {
    let Some(fields) = tx.as_object() else { return tx.clone() };
    let mut out = fields.clone();
    out.remove("gasPrice");
    out.insert("maxFeePerGas".to_string(), json!("0x0"));
    out.insert("maxPriorityFeePerGas".to_string(), json!("0x0"));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROM: &str = "0x1111111111111111111111111111111111111111";
    const TO: &str = "0x2222222222222222222222222222222222222222";

    #[test]
    fn a_plain_transfer_gains_a_zero_fee_cap() {
        // The wallet's native-send shape: from/to/value, no gas, no fee fields.
        let out = for_estimate(&json!({ "from": FROM, "to": TO, "value": "0x2386f26fc10000" }));
        assert_eq!(
            out,
            json!({ "from": FROM, "to": TO, "value": "0x2386f26fc10000",
                    "maxFeePerGas": "0x0", "maxPriorityFeePerGas": "0x0" }),
            "the estimate tx must be the caller's tx plus a zero cap, and nothing else"
        );
    }

    #[test]
    fn calldata_and_an_explicit_gas_limit_survive_untouched() {
        // The ERC-20 / WETH deposit() shape. Only fee fields may be rewritten.
        let out = for_estimate(&json!({ "from": FROM, "to": TO, "data": "0xd0e30db0", "gas": "0x5208" }));
        assert_eq!(out["data"], json!("0xd0e30db0"));
        assert_eq!(out["gas"], json!("0x5208"), "an explicit gas limit must reach the node");
        assert_eq!(out["maxFeePerGas"], json!("0x0"));
    }

    #[test]
    fn a_caller_supplied_tip_is_zeroed_beside_the_zero_cap() {
        // THE HAZARD. A zero cap next to a non-zero tip is not merely wasteful,
        // it is rejected: `maxFeePerGas (0x0) < maxPriorityFeePerGas`.
        let out = for_estimate(&json!({ "from": FROM, "to": TO, "value": "0x1",
                                        "maxFeePerGas": "0x77359400",
                                        "maxPriorityFeePerGas": "0x3b9aca00" }));
        assert_eq!(out["maxFeePerGas"], json!("0x0"));
        assert_eq!(out["maxPriorityFeePerGas"], json!("0x0"),
                   "a tip must never outlive the cap it is compared against");
    }

    #[test]
    fn a_legacy_gas_price_is_dropped_rather_than_mixed_with_the_cap() {
        let out = for_estimate(&json!({ "from": FROM, "to": TO, "gasPrice": "0x77359400" }));
        assert_eq!(out.get("gasPrice"), None, "gasPrice must not survive beside a fee cap");
        assert_eq!(out["maxFeePerGas"], json!("0x0"));
    }

    #[test]
    fn a_tx_that_is_not_an_object_is_forwarded_unchanged() {
        // `request["tx"]` is whatever the caller sent. Inventing an object from
        // a string or a null would send a request nobody wrote -- the node's own
        // rejection is the honest answer.
        for v in [json!("0xdeadbeef"), json!(null), json!([1, 2]), json!(7)] {
            assert_eq!(for_estimate(&v), v, "a non-object tx must pass through untouched");
        }
    }
}
