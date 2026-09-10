//! fee_module — EIP-1559 fee suggestion for EVM chains.
//!
//! One job: turn `eth_feeHistory` into slow/normal/fast fee tiers a wallet can
//! show and a sender can use, and let a caller override any part of it.
//!
//! It exists as its own module rather than as wallet code because two wallets
//! need it and neither should own a copy — the previous arrangement, where the
//! backend derived fees inline, is exactly how the derivation drifted into
//! overpaying 2x on every send without anyone noticing.
//!
//! The algorithm ([`estimator`]) is pure and unit-tested with
//! `cargo test --no-default-features`; the Logos glue is behind the default
//! `logos_module` feature. This module makes no network calls of its own — it
//! asks `eth_rpc_module`, which owns the single fail-closed HTTP chokepoint.
mod estimator;
pub use estimator::{effective_price, is_usable, median_tip, next_base_fee, suggest, suggest_legacy,
                    FeeHistory, FeeSuggestion, Tier, TIERS};

mod tx;
pub use tx::for_estimate;

#[cfg(feature = "logos_module")]
mod glue;
