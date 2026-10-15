//! One allowance shared by every round trip an estimate makes. A bundle of N calls makes up
//! to N estimates plus a probe per token; the caller waits for their SUM, so that is what is
//! bounded. The arithmetic is clock-free so `cargo test --no-default-features` covers it.

use std::time::{Duration, Instant};

use serde_json::Value;

/// What an estimate may spend in total when the caller names no deadline, at least.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(20);
/// One round trip through `eth_rpc` when the chain's own timeout cannot be read: eth_rpc's
/// default `timeoutSecs`.
pub const RPC_CAP: Duration = Duration::from_secs(8);
/// Below this a grant buys nothing: the last sliver goes on answering.
pub const MIN_SLICE: Duration = Duration::from_millis(50);
/// Room for eth_rpc's own refusal to come home after its timeout fires.
pub const MARGIN: Duration = Duration::from_millis(300);
/// Reading the chain's config from eth_rpc: local work, no network.
pub const CONFIG_READ: Duration = Duration::from_millis(1500);

/// One round trip on a chain, from eth_rpc's `get_chain_config`: the `timeoutSecs` the user
/// set, or `verifiedTimeoutSecs` when its reads go through the verified proxy.
pub fn round_trip(config: &Value) -> Duration {
    let verified = config.get("verifiedProxyMode").and_then(Value::as_str) == Some("required");
    let (key, default) = if verified { ("verifiedTimeoutSecs", 15) } else { ("timeoutSecs", 8) };
    let secs = config.get(key).and_then(Value::as_u64).filter(|s| *s > 0).unwrap_or(default);
    Duration::from_secs(secs.clamp(1, 60))
}

/// The most `rounds` round trips of `rt` can take, never less than the old default.
pub fn default_total(rt: Duration, rounds: u32) -> Duration {
    DEFAULT_BUDGET.max((rt + MARGIN) * rounds)
}

pub struct Budget {
    total: Duration,
    started: Instant,
    round_trip: Duration,
}

impl Budget {
    /// Sized for `rounds` round trips of `rt`, from `started`. `deadlineMs` may only shorten
    /// that, never lengthen it.
    pub fn for_chain(started: Instant, rt: Duration, rounds: u32, deadline_ms: Option<i64>) -> Self {
        Budget { total: bound(deadline_ms, default_total(rt, rounds)), started, round_trip: rt }
    }

    /// What the next round trip may take, or what is left when that is less than one round trip.
    pub fn take(&self) -> Result<Duration, Duration> {
        slice(self.total, self.started.elapsed(), self.round_trip)
    }

    pub fn round_trip(&self) -> Duration {
        self.round_trip
    }
}

pub fn bound(deadline_ms: Option<i64>, default: Duration) -> Duration {
    match deadline_ms {
        Some(ms) if ms > 0 => Duration::from_millis(ms as u64).min(default),
        _ => default,
    }
}

/// A whole round trip and the margin its refusal needs, or `Err(left)`: a call started on
/// less than the chain's own timeout would be cut short by us rather than by the endpoint.
pub fn slice(total: Duration, elapsed: Duration, rt: Duration) -> Result<Duration, Duration> {
    let left = total.saturating_sub(elapsed);
    if left < rt.max(MIN_SLICE) {
        return Err(left);
    }
    Ok(left.min(rt + MARGIN))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_deadline_only_shortens_and_the_grants_never_outrun_it() {
        assert_eq!(bound(Some(2_000), DEFAULT_BUDGET), Duration::from_secs(2));
        assert_eq!(bound(Some(60_000), DEFAULT_BUDGET), DEFAULT_BUDGET);
        assert_eq!(bound(Some(0), DEFAULT_BUDGET), DEFAULT_BUDGET);
        assert_eq!(bound(None, DEFAULT_BUDGET), DEFAULT_BUDGET);
        let total = Duration::from_secs(10);
        assert_eq!(slice(total, Duration::ZERO, RPC_CAP), Ok(RPC_CAP + MARGIN));
        assert_eq!(slice(total, Duration::from_secs(1), RPC_CAP), Ok(Duration::from_secs(9).min(RPC_CAP + MARGIN)));
        assert_eq!(slice(total, Duration::from_secs(11), RPC_CAP), Err(Duration::ZERO));
    }

    /// The demo's mainnet chain at `timeoutSecs: 8`: 5.7 s were handed down, the fee history
    /// took 4.69 s, and the estimate was STARTED on the 1.01 s left. It is now refused instead.
    #[test]
    fn a_call_is_never_started_on_less_than_the_chains_own_timeout() {
        let total = Duration::from_millis(5_700);
        assert_eq!(slice(total, Duration::from_millis(4_690), RPC_CAP), Err(Duration::from_millis(1_010)));
        let rt = Duration::from_secs(2);
        assert_eq!(slice(total, Duration::from_millis(3_000), rt), Ok(rt + MARGIN));
        assert_eq!(slice(total, Duration::from_millis(3_500), rt), Ok(Duration::from_millis(2_200)));
    }

    #[test]
    fn the_round_trip_is_the_chains_own_timeout_on_the_route_it_takes() {
        assert_eq!(round_trip(&json!({ "timeoutSecs": 8, "verifiedTimeoutSecs": 15 })), Duration::from_secs(8));
        assert_eq!(round_trip(&json!({ "timeoutSecs": 8, "verifiedTimeoutSecs": 15,
                                       "verifiedProxyMode": "required" })), Duration::from_secs(15));
        assert_eq!(round_trip(&json!({})), RPC_CAP, "eth_rpc's own default");
        assert_eq!(round_trip(&json!({ "timeoutSecs": 0 })), RPC_CAP, "0 is reqwest's default, not zero");
        assert_eq!(round_trip(&json!({ "timeoutSecs": 600 })), Duration::from_secs(60));
    }

    #[test]
    fn the_default_grows_with_the_round_trip_and_never_shrinks_below_the_old_one() {
        assert_eq!(default_total(Duration::from_secs(3), 4), DEFAULT_BUDGET);
        assert_eq!(default_total(Duration::from_secs(15), 4), Duration::from_millis(61_200));
        let b = Budget::for_chain(Instant::now(), Duration::from_secs(15), 4, Some(30_000));
        assert_eq!(b.total, Duration::from_secs(30), "a deadline still only shortens");
    }
}
