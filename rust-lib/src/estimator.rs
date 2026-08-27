//! EIP-1559 fee suggestion — the pure algorithm, with no I/O and no module glue.
//!
//! This is the standard fee-history strategy (the one Nethereum calls
//! `MedianPriorityFeeHistorySuggestionStrategy`, and which ethers/alloy
//! implement in the same shape):
//!
//!   tip   = median of the reward percentile across the last N blocks
//!   base  = next block's base fee, projected from the latest one
//!   maxFee = base * headroom + tip
//!
//! Two properties matter and are easy to get wrong:
//!
//!   * `eth_gasPrice` is roughly `baseFee + tip`. Using it AS the tip tips
//!     approximately the whole base fee. Because EIP-1559 caps the effective
//!     tip at `maxFee - baseFee`, that does not cost 500x — it costs exactly
//!     2x, on every single send. That is the bug this module exists to remove.
//!   * headroom must multiply the BASE fee, not `gasPrice`. Doubling a number
//!     that already contains the tip gives the least headroom exactly when the
//!     base fee is climbing, which is when headroom is the point.


/// A percentile of the priority-fee distribution, and the label a UI shows.
#[derive(Clone, Copy, Debug)]
pub struct Tier {
    pub name: &'static str,
    pub reward_percentile: f64,
    /// Multiplier applied to the projected base fee to absorb base-fee growth
    /// while the transaction is pending. Applied to BASE, never to gasPrice.
    pub base_headroom: u64,
}

/// Slow / normal / fast. Percentiles are the conventional spread; headroom
/// rises with urgency because a fast transaction is the one that must survive
/// several blocks of base-fee growth.
pub const TIERS: [Tier; 3] = [
    Tier { name: "slow",   reward_percentile: 10.0, base_headroom: 2 },
    Tier { name: "normal", reward_percentile: 50.0, base_headroom: 2 },
    Tier { name: "fast",   reward_percentile: 90.0, base_headroom: 3 },
];

// No serde derive: u128 has no serde impl without alloy's `serde` feature, and
// the glue emits decimal wei STRINGS anyway — JSON numbers cannot hold 256 bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeeSuggestion {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// What `eth_feeHistory` gave us, already parsed.
#[derive(Clone, Debug, Default)]
pub struct FeeHistory {
    /// `baseFeePerGas` — one entry per block PLUS the next block's projection,
    /// which is why the last element is the one to use.
    pub base_fee_per_gas: Vec<u128>,
    /// `reward[i][j]` — block i, percentile j.
    pub reward: Vec<Vec<u128>>,
    pub gas_used_ratio: Vec<f64>,
}

/// Median of a set of tips, ignoring zeros (a zero reward means the block had
/// no transactions at that percentile, not that a zero tip would land).
pub fn median_tip(mut tips: Vec<u128>) -> u128 {
    tips.retain(|t| *t != 0);
    if tips.is_empty() {
        return 0u128;
    }
    tips.sort_unstable();
    let mid = tips.len() / 2;
    if tips.len() % 2 == 1 {
        tips[mid]
    } else {
        (tips[mid - 1] + tips[mid]) / 2u128
    }
}

/// The next block's base fee. `eth_feeHistory` already appends it as the final
/// element of `baseFeePerGas`; fall back to the last observed one.
pub fn next_base_fee(h: &FeeHistory) -> u128 {
    h.base_fee_per_gas.last().copied().unwrap_or(0u128)
}

/// Suggest a fee for one tier from fee history.
///
/// `column` selects which percentile column of `reward` this tier used, since
/// one `eth_feeHistory` call requests every percentile at once.
pub fn suggest(h: &FeeHistory, tier: &Tier, column: usize) -> FeeSuggestion {
    let tips: Vec<u128> = h
        .reward
        .iter()
        .filter_map(|row| row.get(column).copied())
        .collect();
    let tip = median_tip(tips);
    let base = next_base_fee(h);
    FeeSuggestion {
        max_fee_per_gas: base.saturating_mul(u128::from(tier.base_headroom)).saturating_add(tip),
        max_priority_fee_per_gas: tip,
    }
}

/// Fallback for a chain with no fee history (pre-1559, or a node that refuses
/// `eth_feeHistory`). Legacy pricing: the whole thing is the "max fee" and
/// there is no separate tip. Deliberately NOT the old
/// `tip = gas_price` shape — on a legacy chain the tip field is meaningless,
/// so it is zero rather than wrong.
pub fn suggest_legacy(gas_price: u128, tier: &Tier) -> FeeSuggestion {
    let bumped = match tier.name {
        "slow" => gas_price,
        "fast" => gas_price.saturating_mul(125) / 100,
        _ => gas_price.saturating_mul(110) / 100,
    };
    FeeSuggestion { max_fee_per_gas: bumped, max_priority_fee_per_gas: 0u128 }
}

/// What a sender will actually pay per gas, given a suggestion and the base
/// fee that ends up in the block: `base + min(tip, maxFee - base)`.
/// Exposed because it is the only honest way to compare two suggestions, and
/// it is what the unit tests assert on.
pub fn effective_price(s: &FeeSuggestion, base: u128) -> u128 {
    if s.max_fee_per_gas <= base {
        return s.max_fee_per_gas;
    }
    let room = s.max_fee_per_gas - base;
    base + std::cmp::min(s.max_priority_fee_per_gas, room)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gwei(x: f64) -> u128 { (x * 1e9) as u128 }

    fn history(base: u128, rows: &[[u128; 3]]) -> FeeHistory {
        FeeHistory {
            // eth_feeHistory returns one more base fee than blocks: the projection.
            base_fee_per_gas: vec![base, base],
            reward: rows.iter().map(|r| r.to_vec()).collect(),
            gas_used_ratio: vec![0.5; rows.len()],
        }
    }

    #[test]
    fn median_ignores_empty_blocks() {
        // A zero is "no transaction at this percentile", not "a zero tip works".
        assert_eq!(median_tip(vec![0u128, 10u128, 0u128]), 10u128);
        assert_eq!(median_tip(vec![]), 0u128);
        assert_eq!(median_tip(vec![0u128]), 0u128);
    }

    #[test]
    fn median_is_the_middle_not_the_mean() {
        // One whale tipping 1000 must not drag the suggestion up.
        let tips = vec![1u128, 2u128, 3u128, 1000u128];
        assert_eq!(median_tip(tips), (2u128 + 3u128) / 2);
    }

    #[test]
    fn tip_comes_from_the_reward_column_not_from_gas_price() {
        // THE REGRESSION THIS MODULE EXISTS FOR. Real mainnet shape:
        // base 0.2904 gwei, prevailing tip 0.0005 gwei.
        let base = gwei(0.2904);
        let tip  = gwei(0.0005);
        let h = history(base, &[[tip; 3]; 5]);
        let s = suggest(&h, &TIERS[1], 1);

        assert_eq!(s.max_priority_fee_per_gas, tip, "tip must be the observed tip");

        // The old derivation: maxFee = gasPrice*2, tip = gasPrice, gasPrice ~ base+tip.
        let gas_price = base + tip;
        let old = FeeSuggestion {
            max_fee_per_gas: gas_price.saturating_mul(2),
            max_priority_fee_per_gas: gas_price,
        };

        let new_paid = effective_price(&s, base);
        let old_paid = effective_price(&old, base);
        assert!(new_paid < old_paid, "the fix must cost less: {new_paid} vs {old_paid}");
        // It was almost exactly 2x. Assert we removed most of it rather than a
        // hardcoded ratio, so the test survives a headroom tweak.
        assert!(old_paid > new_paid * 19 / 10,
                "expected ~2x saving, got {old_paid} vs {new_paid}");
    }

    #[test]
    fn headroom_multiplies_base_not_gas_price() {
        let base = gwei(100.0);
        let tip = gwei(1.0);
        let h = history(base, &[[tip; 3]; 3]);
        let s = suggest(&h, &TIERS[1], 1);
        assert_eq!(s.max_fee_per_gas, base * 2 + tip);
        // The sender pays base + tip, NOT the max.
        assert_eq!(effective_price(&s, base), base + tip);
    }

    #[test]
    fn fast_tier_survives_more_base_fee_growth_than_slow() {
        let base = gwei(50.0);
        let tip = gwei(2.0);
        let h = history(base, &[[tip; 3]; 4]);
        let slow = suggest(&h, &TIERS[0], 0);
        let fast = suggest(&h, &TIERS[2], 2);
        assert!(fast.max_fee_per_gas > slow.max_fee_per_gas);
        // A fast tx should still be includable after the base fee doubles.
        assert!(fast.max_fee_per_gas > base * 2);
    }

    #[test]
    fn effective_price_caps_the_tip_at_the_remaining_room() {
        let base = gwei(100.0);
        // maxFee leaves only 1 gwei of room, but the tip asks for 5.
        let s = FeeSuggestion { max_fee_per_gas: base + gwei(1.0), max_priority_fee_per_gas: gwei(5.0) };
        assert_eq!(effective_price(&s, base), base + gwei(1.0));
    }

    #[test]
    fn legacy_fallback_leaves_the_tip_field_empty() {
        // On a pre-1559 chain a "priority fee" is not a thing; emitting the gas
        // price there is what produced the original bug.
        let s = suggest_legacy(gwei(30.0), &TIERS[1]);
        assert_eq!(s.max_priority_fee_per_gas, 0u128);
        assert!(s.max_fee_per_gas >= gwei(30.0));
    }

    #[test]
    fn empty_history_does_not_panic_or_suggest_garbage() {
        let h = FeeHistory::default();
        let s = suggest(&h, &TIERS[1], 1);
        assert_eq!(s.max_fee_per_gas, 0u128);
        assert_eq!(s.max_priority_fee_per_gas, 0u128);
    }
}
