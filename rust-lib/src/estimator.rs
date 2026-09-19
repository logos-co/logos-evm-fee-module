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

/// Slow / normal / fast. A block's reward percentiles are its included tips ordered by gas,
/// so the marginal tip that got in sits low in the distribution; the top of it is MEV.
/// Measured on mainnet 2026-09-13 at a 0.07 gwei base fee: p50 ≈ 0.009 gwei, p90 ≈ 1 gwei —
/// the old fast tier tipped fourteen base fees for the same next-block inclusion.
pub const TIERS: [Tier; 3] = [
    Tier { name: "slow",   reward_percentile: 10.0, base_headroom: 2 },
    Tier { name: "normal", reward_percentile: 30.0, base_headroom: 2 },
    Tier { name: "fast",   reward_percentile: 60.0, base_headroom: 3 },
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

/// Is this fee history actually usable, or is it a successful-but-empty body?
///
/// A node can answer `eth_feeHistory` with `success: true` and every array
/// empty — observed through the nimbus verified proxy before 71f2a085, for a
/// hex `blockCount`. There is no error to catch, so a caller that only checks
/// for failure derives tiers from nothing and still reports them as
/// fee-history-derived.
///
/// Two ways to be empty, and BOTH matter:
///   * no base fee at all, or `oldestBlock == 0` — the whole body is blank;
///   * reward rows present but every row empty — this one is worse, because it
///     survives a naive `!reward.is_empty()` check and yields a ZERO tip
///     against a real base fee, i.e. a plausible suggestion that will not get
///     the transaction mined.
pub fn is_usable(h: &FeeHistory) -> bool {
    next_base_fee(h) != 0 && h.reward.iter().any(|row| row.iter().any(|t| *t != 0))
}

/// Where a chain's tiers are priced from.
#[derive(Debug)]
pub enum Pricing<'a> {
    /// The history's reward rows.
    Rewards(&'a FeeHistory),
    /// The node's own tip against this base fee: the reward rows were blank.
    NodeTip(u128),
    /// The legacy gas price: the chain has no base fee.
    GasPrice,
}

/// A history that could not be read prices nothing. Swallowed, it read as a chain with no
/// base fee, and every tier went out type-2 with a zero tip that may never be mined.
pub fn pricing(read: &Result<FeeHistory, String>) -> Result<Pricing<'_>, String> {
    let h = read.as_ref().map_err(Clone::clone)?;
    if is_usable(h) {
        return Ok(Pricing::Rewards(h));
    }
    match next_base_fee(h) {
        0 => Ok(Pricing::GasPrice),
        base => Ok(Pricing::NodeTip(base)),
    }
}

/// A tier priced from a base fee and a tip that came from anywhere: the node's own
/// `eth_maxPriorityFeePerGas` when the reward rows are blank, or the history below.
pub fn suggest_with_tip(base: u128, tip: u128, tier: &Tier) -> FeeSuggestion {
    FeeSuggestion {
        max_fee_per_gas: base.saturating_mul(u128::from(tier.base_headroom)).saturating_add(tip),
        max_priority_fee_per_gas: tip,
    }
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
    suggest_with_tip(next_base_fee(h), median_tip(tips), tier)
}

/// Make the tiers non-decreasing in tip and in cap. Each column drops its zero rewards
/// before the median, so a thin low column can median ABOVE a fuller higher one; a "slow"
/// that costs more than "normal" is a suggestion no human can act on.
pub fn monotone(tiers: &mut [FeeSuggestion]) {
    for i in 1..tiers.len() {
        let prev = tiers[i - 1].clone();
        let cur = &mut tiers[i];
        cur.max_priority_fee_per_gas = cur.max_priority_fee_per_gas.max(prev.max_priority_fee_per_gas);
        cur.max_fee_per_gas = cur.max_fee_per_gas.max(prev.max_fee_per_gas).max(cur.max_priority_fee_per_gas);
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

/// A tier with the caller's own fields in it. A field the caller set is used as given; with
/// one set, the other comes from the tier: a lone tip keeps the tier's headroom over the base
/// fee beneath it, and a lone max fee caps the tier's tip.
pub fn with_overrides(tier: &FeeSuggestion, max: Option<u128>, tip: Option<u128>) -> Result<FeeSuggestion, String> {
    let s = match (max, tip) {
        (Some(m), Some(p)) => FeeSuggestion { max_fee_per_gas: m, max_priority_fee_per_gas: p },
        (Some(m), None) => FeeSuggestion { max_fee_per_gas: m, max_priority_fee_per_gas: tier.max_priority_fee_per_gas.min(m) },
        (None, Some(p)) => FeeSuggestion {
            max_fee_per_gas: tier.max_fee_per_gas.saturating_sub(tier.max_priority_fee_per_gas).saturating_add(p),
            max_priority_fee_per_gas: p,
        },
        (None, None) => tier.clone(),
    };
    if s.max_priority_fee_per_gas > s.max_fee_per_gas {
        return Err("maxPriorityFeePerGas cannot exceed maxFeePerGas".into());
    }
    Ok(s)
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
    fn tiers_never_invert_when_zero_rewards_thin_a_column() {
        // Measured shape, 2026-09-13: the p10 column is zero in eight blocks of ten and
        // 0.006 gwei in one, so its nonzero median is 0.006 while p30's is 0.001.
        let mut rows = [[0u128, gwei(0.001), gwei(0.01)]; 10];
        rows[4] = [gwei(0.006), gwei(0.006), gwei(0.02)];
        let h = history(gwei(0.07), &rows);
        let mut tiers: Vec<FeeSuggestion> =
            TIERS.iter().enumerate().map(|(i, t)| suggest(&h, t, i)).collect();
        assert!(tiers[0].max_priority_fee_per_gas > tiers[1].max_priority_fee_per_gas,
                "the raw medians DO invert here; that is the defect");
        monotone(&mut tiers);
        for w in tiers.windows(2) {
            assert!(w[0].max_priority_fee_per_gas <= w[1].max_priority_fee_per_gas);
            assert!(w[0].max_fee_per_gas <= w[1].max_fee_per_gas);
        }
        for t in &tiers {
            assert!(t.max_priority_fee_per_gas <= t.max_fee_per_gas, "a tip above its own cap");
        }
        assert_eq!(tiers[1].max_priority_fee_per_gas, gwei(0.006), "lifted to slow's, not invented");
    }

    #[test]
    fn a_known_tip_prices_every_tier_off_the_base_fee() {
        // The fallback for a 1559 chain whose reward rows are blank: the node's own tip.
        let (base, tip) = (gwei(1.0), gwei(1.0));
        let normal = suggest_with_tip(base, tip, &TIERS[1]);
        let fast = suggest_with_tip(base, tip, &TIERS[2]);
        assert_eq!(normal, FeeSuggestion { max_fee_per_gas: gwei(3.0), max_priority_fee_per_gas: tip });
        assert_eq!(fast.max_fee_per_gas, gwei(4.0));
        assert_eq!(suggest_with_tip(base, 0, &TIERS[0]).max_priority_fee_per_gas, 0, "a zero tip is passed on, not padded");
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
    fn a_successful_but_empty_body_is_not_usable() {
        // Verbatim shape returned by the verified proxy for a hex blockCount:
        // success: true, every array empty, oldestBlock "0x0".
        let h = FeeHistory { base_fee_per_gas: vec![], reward: vec![], gas_used_ratio: vec![] };
        assert!(!is_usable(&h), "an empty body must be a MISS, not a suggestion");
    }

    #[test]
    fn reward_rows_that_are_all_empty_are_not_usable() {
        // The nastier shape: rows exist, so `!reward.is_empty()` passes, but
        // there is no tip in any of them. Deriving from this gives a ZERO tip
        // against a real base fee -- a suggestion that looks fine and does not
        // get mined.
        let h = FeeHistory {
            base_fee_per_gas: vec![gwei(30.0), gwei(30.0)],
            reward: vec![vec![], vec![], vec![]],
            gas_used_ratio: vec![0.5; 3],
        };
        assert!(!is_usable(&h), "empty reward rows must be a MISS");
    }

    #[test]
    fn a_fee_history_that_cannot_be_read_is_an_error() {
        // Measured on mainnet: the verified proxy refused a numeric blockCount, and every
        // tier was priced off the gas price with a zero tip.
        let why = "verified proxy: eth_feeHistory: quantity parameter must be a 0x-prefixed hex string";
        assert_eq!(pricing(&Err(why.into())).unwrap_err(), why);
    }

    #[test]
    fn blank_rewards_price_off_the_node_tip_and_no_base_fee_off_the_gas_price() {
        let real = Ok(history(gwei(30.0), &[[gwei(1.0); 3]; 3]));
        assert!(matches!(pricing(&real), Ok(Pricing::Rewards(_))));
        let blank = Ok(history(gwei(30.0), &[[0; 3]; 3]));
        assert!(matches!(pricing(&blank), Ok(Pricing::NodeTip(b)) if b == gwei(30.0)));
        assert!(matches!(pricing(&Ok(FeeHistory::default())), Ok(Pricing::GasPrice)));
    }

    #[test]
    fn a_real_body_is_usable() {
        let h = history(gwei(30.0), &[[gwei(1.0); 3]; 3]);
        assert!(is_usable(&h));
    }

    #[test]
    fn empty_history_does_not_panic_or_suggest_garbage() {
        let h = FeeHistory::default();
        let s = suggest(&h, &TIERS[1], 1);
        assert_eq!(s.max_fee_per_gas, 0u128);
        assert_eq!(s.max_priority_fee_per_gas, 0u128);
    }

    // A field the caller set is theirs; a lone one used to price the whole fee at the tier.
    #[test]
    fn a_lone_tip_rides_on_the_tiers_headroom_over_the_base_fee() {
        let tier = FeeSuggestion { max_fee_per_gas: gwei(3.0), max_priority_fee_per_gas: gwei(1.0) };
        let s = with_overrides(&tier, None, Some(0)).unwrap();
        assert_eq!(s, FeeSuggestion { max_fee_per_gas: gwei(2.0), max_priority_fee_per_gas: 0 });
        let s = with_overrides(&tier, None, Some(gwei(4.0))).unwrap();
        assert_eq!(s, FeeSuggestion { max_fee_per_gas: gwei(6.0), max_priority_fee_per_gas: gwei(4.0) });
    }

    #[test]
    fn a_lone_max_fee_caps_the_tiers_tip() {
        let tier = FeeSuggestion { max_fee_per_gas: gwei(3.0), max_priority_fee_per_gas: gwei(1.0) };
        assert_eq!(with_overrides(&tier, Some(gwei(0.5)), None).unwrap(),
                   FeeSuggestion { max_fee_per_gas: gwei(0.5), max_priority_fee_per_gas: gwei(0.5) });
        assert_eq!(with_overrides(&tier, Some(gwei(5.0)), None).unwrap(),
                   FeeSuggestion { max_fee_per_gas: gwei(5.0), max_priority_fee_per_gas: gwei(1.0) });
    }

    #[test]
    fn both_fields_set_are_obeyed_and_none_set_is_the_tier() {
        let tier = FeeSuggestion { max_fee_per_gas: gwei(3.0), max_priority_fee_per_gas: gwei(1.0) };
        assert_eq!(with_overrides(&tier, Some(7), Some(5)).unwrap(),
                   FeeSuggestion { max_fee_per_gas: 7, max_priority_fee_per_gas: 5 });
        assert_eq!(with_overrides(&tier, None, None).unwrap(), tier);
        assert!(with_overrides(&tier, Some(5), Some(7)).unwrap_err().contains("cannot exceed"));
    }

    // A chain with no base fee prices its tiers off gasPrice with no tip: a tip goes on top.
    #[test]
    fn a_lone_tip_on_a_legacy_tier_goes_on_top_of_its_price() {
        let tier = suggest_legacy(gwei(2.0), &TIERS[1]);
        let s = with_overrides(&tier, None, Some(gwei(1.0))).unwrap();
        assert_eq!(s, FeeSuggestion { max_fee_per_gas: gwei(2.2) + gwei(1.0), max_priority_fee_per_gas: gwei(1.0) });
    }
}
