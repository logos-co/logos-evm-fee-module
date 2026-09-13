//! fee_module — EIP-1559 fee suggestion and gas estimation for EVM chains.
//!
//! One job: turn `eth_feeHistory` into slow/normal/fast fee tiers a wallet can
//! show and a sender can use, put a gas limit under a call or a whole bundle of
//! them, and let a caller override any part of it.
//!
//! It exists as its own module rather than as wallet code because two wallets
//! need it and neither should own a copy — the previous arrangement, where the
//! backend derived fees inline, is exactly how the derivation drifted into
//! overpaying 2x on every send without anyone noticing.
//!
//! The algorithm ([`estimator`]), the slot arithmetic ([`slots`]) and the bundle
//! rules ([`bundle`]) are pure and unit-tested with
//! `cargo test --no-default-features`; the Logos glue is behind the default
//! `logos_module` feature. This module makes no network calls of its own — it
//! asks `eth_rpc_module`, which owns the single fail-closed HTTP chokepoint.
mod estimator;
pub use estimator::{effective_price, is_usable, median_tip, monotone, next_base_fee, suggest,
                    suggest_legacy, suggest_with_tip, FeeHistory, FeeSuggestion, Tier, TIERS};

mod tx;
pub use tx::for_estimate;

mod units;
pub use units::{decorate, format_display, format_exact, NATIVE_DECIMALS};

mod slots;
pub use slots::{identify, keccak256, parse_approve, probe, sentinel, slot_for, Approval, Layout, Probe,
                MAX_BASE_SLOT};

mod bundle;
pub use bundle::{approvals_before, call_name, name_of, parse_call, parse_calls, parse_quantity, Call, MAX_CALLS};

mod budget;
pub use budget::{bound, slice, Budget, DEFAULT_BUDGET, MIN_SLICE, RPC_CAP};

#[cfg(feature = "logos_module")]
mod glue;
