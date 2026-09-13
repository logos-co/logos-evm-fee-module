//! Where an ERC-20 keeps `allowance[owner][spender]`, found by asking rather than assuming.
//!
//! A call that follows an `approve` can only be estimated once that allowance exists, and
//! `eth_estimateGas` takes a state override for exactly that. But the slot is per token:
//! Solidity hashes `spender ‖ keccak(owner ‖ base)`, Vyper hashes `keccak(base ‖ owner) ‖
//! spender`, and `base` is wherever the mapping landed in that contract's layout. One
//! `eth_call` with every candidate slot set to its own sentinel tells us which one the token
//! reads — or that it reads none of them, in which case nothing is overridden.

use serde_json::{json, Map, Value};
use sha3::{Digest, Keccak256};

pub const APPROVE_SELECTOR: &str = "095ea7b3";
pub const ALLOWANCE_SELECTOR: &str = "dd62ed3e";
/// Candidate mapping slots per layout. No ERC-20 in circulation declares its allowance
/// mapping past slot 31, and 32 keeps the probe to one call carrying 64 sentinels.
pub const MAX_BASE_SLOT: u64 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Solidity,
    Vyper,
}

/// An `approve(spender, amount)` an earlier call makes, as a later call will find it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    /// Lowercased `0x…` token address.
    pub token: String,
    /// Lowercased `0x…` spender address.
    pub spender: String,
    /// The amount as a full 32-byte word, `0x` + 64 hex digits.
    pub amount_word: String,
}

impl Approval {
    /// The amount as a hex quantity, for reporting.
    pub fn amount(&self) -> String {
        let trimmed = self.amount_word[2..].trim_start_matches('0');
        if trimmed.is_empty() { "0x0".to_string() } else { format!("0x{trimmed}") }
    }
}

pub fn keccak256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut a = [0u8; 32];
    a.copy_from_slice(&out);
    a
}

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn hex_of(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn address_word(addr: &str) -> Option<[u8; 32]> {
    let b = hex_bytes(addr)?;
    if b.len() != 20 {
        return None;
    }
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(&b);
    Some(w)
}

fn u64_word(n: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&n.to_be_bytes());
    w
}

/// The storage slot of `allowance[owner][spender]` if the mapping sits at `base`.
pub fn slot_for(layout: Layout, owner: &str, spender: &str, base: u64) -> Option<String> {
    let (o, s, b) = (address_word(owner)?, address_word(spender)?, u64_word(base));
    let slot = match layout {
        Layout::Solidity => keccak256(&[s.as_slice(), keccak256(&[o, b].concat()).as_slice()].concat()),
        Layout::Vyper => keccak256(&[keccak256(&[b, o].concat()).as_slice(), s.as_slice()].concat()),
    };
    Some(hex_of(&slot))
}

/// A value no real allowance is, distinct per candidate, so the answer names the slot.
pub fn sentinel(layout: Layout, base: u64) -> String {
    let kind = match layout {
        Layout::Solidity => 1u64,
        Layout::Vyper => 2u64,
    };
    format!("0x{:064x}", (0xa11ceu64 << 16) | (kind << 8) | base)
}

/// One `eth_call` that reads `allowance(owner, spender)` over every candidate slot.
pub struct Probe {
    pub call: Value,
    pub overrides: Value,
    sentinels: Vec<(String, String)>,
}

pub fn probe(token: &str, owner: &str, spender: &str) -> Option<Probe> {
    let data = format!(
        "0x{ALLOWANCE_SELECTOR}{}{}",
        &hex_of(&address_word(owner)?)[2..],
        &hex_of(&address_word(spender)?)[2..]
    );
    let mut diff = Map::new();
    let mut sentinels = Vec::with_capacity(2 * MAX_BASE_SLOT as usize);
    for base in 0..MAX_BASE_SLOT {
        for layout in [Layout::Solidity, Layout::Vyper] {
            let slot = slot_for(layout, owner, spender, base)?;
            let s = sentinel(layout, base);
            diff.insert(slot.clone(), json!(s));
            sentinels.push((s, slot));
        }
    }
    Some(Probe {
        call: json!({ "to": token, "data": data }),
        overrides: json!({ token: { "stateDiff": diff } }),
        sentinels,
    })
}

/// The slot the token read, given what the probe call returned.
pub fn identify(p: &Probe, answer: &str) -> Option<String> {
    let a = answer.trim();
    let a = a.strip_prefix("0x").or_else(|| a.strip_prefix("0X")).unwrap_or(a).to_ascii_lowercase();
    if a.len() > 64 || a.is_empty() || !a.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let word = format!("{}{a}", "0".repeat(64 - a.len()));
    p.sentinels.iter().find(|(s, _)| s[2..] == word).map(|(_, slot)| slot.clone())
}

/// Read `approve(spender, amount)` off a call's calldata, or nothing if it is any other call.
pub fn parse_approve(to: &str, data: &str) -> Option<Approval> {
    let d = data.trim();
    let d = d.strip_prefix("0x").or_else(|| d.strip_prefix("0X"))?.to_ascii_lowercase();
    if d.len() != 136 || !d.starts_with(APPROVE_SELECTOR) || !d.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let spender_word = &d[8..72];
    if spender_word[..24].bytes().any(|b| b != b'0') {
        return None;
    }
    address_word(to)?;
    Some(Approval {
        token: to.trim().to_ascii_lowercase(),
        spender: format!("0x{}", &spender_word[24..]),
        amount_word: format!("0x{}", &d[72..136]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "0x28C6c06298d514Db089934071355E5743bf21d60";
    const SPENDER: &str = "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45";
    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";

    #[test]
    fn keccak_matches_the_known_vectors() {
        assert_eq!(hex_of(&keccak256(b"")), "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470");
        assert_eq!(hex_of(&keccak256(b"abc")), "0x4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45");
    }

    #[test]
    fn the_slot_arithmetic_matches_cast_for_both_layouts() {
        // USDC keeps `allowed` at slot 10; both figures computed with `cast keccak`.
        assert_eq!(
            slot_for(Layout::Solidity, OWNER, SPENDER, 10).unwrap(),
            "0x6e3a9bc4e1278d0fa5b5a94c63b17c3d1255fd1dd71c6eec15a992f6119ad7d2"
        );
        assert_eq!(
            slot_for(Layout::Vyper, OWNER, SPENDER, 10).unwrap(),
            "0xe018a09d9d29947229c90cece446ea93180954c1aad1eb376c5c7a33ec09d5e7"
        );
        assert_eq!(slot_for(Layout::Solidity, "0x12", SPENDER, 1), None, "a short address is not padded into one");
    }

    #[test]
    fn a_probe_names_the_slot_a_token_reads() {
        let p = probe(USDC, OWNER, SPENDER).unwrap();
        assert_eq!(p.call["to"], USDC);
        assert_eq!(
            p.call["data"],
            "0xdd62ed3e00000000000000000000000028c6c06298d514db089934071355e5743bf21d60\
             00000000000000000000000068b3465833fb72a70ecdf485e0e4c7bd8665fc45"
        );
        let diff = p.overrides[USDC]["stateDiff"].as_object().unwrap();
        assert_eq!(diff.len(), 64, "32 bases × 2 layouts, every slot distinct");
        // What the fork answered for USDC on 2026-09-13: the Solidity sentinel for base 10.
        let answer = "0x00000000000000000000000000000000000000000000000000000a11ce010a";
        assert_eq!(identify(&p, answer).as_deref(), slot_for(Layout::Solidity, OWNER, SPENDER, 10).as_deref());
        assert_eq!(identify(&p, "0x0"), None, "a real allowance of zero names nothing");
        assert_eq!(identify(&p, "0x3b9aca00"), None, "a real allowance names nothing");
        assert_eq!(identify(&p, "0xzz"), None);
    }

    #[test]
    fn sentinels_are_distinct_and_never_a_plausible_allowance() {
        let mut all: Vec<String> = Vec::new();
        for base in 0..MAX_BASE_SLOT {
            all.push(sentinel(Layout::Solidity, base));
            all.push(sentinel(Layout::Vyper, base));
        }
        let n = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), n);
        assert_eq!(sentinel(Layout::Vyper, 3), "0x0000000000000000000000000000000000000000000000000000000a11ce0203");
    }

    #[test]
    fn only_a_canonical_approve_is_read_as_one() {
        let amount = "000000000000000000000000000000000000000000000000000000003b9aca00";
        let data = format!("0x095ea7b3000000000000000000000000{}{amount}", &SPENDER[2..].to_ascii_lowercase());
        let a = parse_approve(USDC, &data).unwrap();
        assert_eq!(a.token, USDC.to_ascii_lowercase());
        assert_eq!(a.spender, SPENDER.to_ascii_lowercase());
        assert_eq!(a.amount(), "0x3b9aca00");
        assert_eq!(parse_approve(USDC, &data.to_ascii_uppercase().replace("0X", "0x")).map(|a| a.amount()).as_deref(), Some("0x3b9aca00"));
        // Not approves: a transfer, a truncated approve, dirty upper bytes in the spender word.
        assert_eq!(parse_approve(USDC, &data.replace("095ea7b3", "a9059cbb")), None);
        assert_eq!(parse_approve(USDC, &data[..100]), None);
        assert_eq!(parse_approve(USDC, &data.replacen("0x095ea7b3000000000000000000000000", "0x095ea7b3ff0000000000000000000000", 1)), None);
        assert_eq!(parse_approve("not-an-address", &data), None);
        let zero = parse_approve(USDC, &data.replace(amount, &"0".repeat(64))).unwrap();
        assert_eq!(zero.amount(), "0x0", "a reset is an approve too");
    }
}
