//! One allowance shared by every round trip an estimate makes. A bundle of N calls makes up
//! to N estimates plus a probe per token; the caller waits for their SUM, so that is what is
//! bounded. The arithmetic is clock-free so `cargo test --no-default-features` covers it.

use std::time::{Duration, Instant};

/// What an estimate may spend in total when the caller names no deadline.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(20);
/// One round trip through `eth_rpc`. A cold node can take seconds on a first estimate.
pub const RPC_CAP: Duration = Duration::from_secs(8);
/// Below this a grant buys nothing: the last sliver goes on answering.
pub const MIN_SLICE: Duration = Duration::from_millis(50);

pub struct Budget {
    total: Duration,
    started: Instant,
}

impl Budget {
    /// `deadlineMs` may only shorten the default, never lengthen it.
    pub fn bounded_by(deadline_ms: Option<i64>) -> Self {
        Budget { total: bound(deadline_ms), started: Instant::now() }
    }

    /// What the next round trip may take, or `None` once too little is left.
    pub fn take(&self) -> Option<Duration> {
        slice(self.total, self.started.elapsed())
    }
}

pub fn bound(deadline_ms: Option<i64>) -> Duration {
    match deadline_ms {
        Some(ms) if ms > 0 => Duration::from_millis(ms as u64).min(DEFAULT_BUDGET),
        _ => DEFAULT_BUDGET,
    }
}

pub fn slice(total: Duration, elapsed: Duration) -> Option<Duration> {
    let grant = total.checked_sub(elapsed)?.min(RPC_CAP);
    (grant >= MIN_SLICE).then_some(grant)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deadline_only_shortens_and_the_grants_never_outrun_it() {
        assert_eq!(bound(Some(2_000)), Duration::from_secs(2));
        assert_eq!(bound(Some(60_000)), DEFAULT_BUDGET);
        assert_eq!(bound(Some(0)), DEFAULT_BUDGET);
        assert_eq!(bound(None), DEFAULT_BUDGET);
        let total = Duration::from_secs(10);
        assert_eq!(slice(total, Duration::ZERO), Some(RPC_CAP));
        assert_eq!(slice(total, Duration::from_secs(9)), Some(Duration::from_secs(1)));
        assert_eq!(slice(total, Duration::from_millis(9_970)), None, "30ms buys nothing");
        assert_eq!(slice(total, Duration::from_secs(11)), None);
    }
}
