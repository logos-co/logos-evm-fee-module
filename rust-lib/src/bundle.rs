//! A bundle's calls and what each one inherits from the ones before it — pure, so the
//! ordering rules are unit-tested without a node.

use serde_json::{json, Value};

use crate::slots::{parse_approve, Approval};

pub const MAX_CALLS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub to: String,
    pub value: u128,
    /// `0x`-hex calldata, `"0x"` for a plain transfer.
    pub data: String,
    /// A limit the caller vouches for; the estimator does not second-guess one.
    pub gas_limit: Option<u64>,
    pub label: String,
}

impl Call {
    /// The transaction handed to `eth_estimateGas`, before the zero fee cap.
    pub fn tx(&self, from: &str) -> Value {
        let mut tx = json!({ "from": from, "to": self.to, "value": format!("0x{:x}", self.value) });
        if self.data != "0x" {
            tx["data"] = json!(self.data);
        }
        tx
    }
}

/// Decimal or `0x`-hex.
pub fn parse_quantity(s: &str) -> Option<u128> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) if !h.is_empty() => u128::from_str_radix(h, 16).ok(),
        Some(_) => None,
        None => s.parse::<u128>().ok(),
    }
}

fn quantity_field(v: &Value, key: &str, name: &str) -> Result<Option<u128>, String> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => parse_quantity(s).map(Some).ok_or_else(|| format!("{name}: `{key}` is not a quantity")),
        Some(Value::Number(n)) => n.as_u64().map(|x| Some(u128::from(x))).ok_or_else(|| format!("{name}: `{key}` is not a quantity")),
        Some(_) => Err(format!("{name}: `{key}` is not a quantity")),
    }
}

fn normalize_data(v: Option<&Value>, name: &str) -> Result<String, String> {
    let s = match v {
        None | Some(Value::Null) => return Ok("0x".to_string()),
        Some(Value::String(s)) => s.trim(),
        Some(_) => return Err(format!("{name}: `data` must be a hex string")),
    };
    let body = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if body.len() % 2 != 0 || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{name}: `data` is not even-length hex"));
    }
    Ok(format!("0x{}", body.to_ascii_lowercase()))
}

/// `{ to, value?, data?, gasLimit?, label? }`. `name` is how a refusal refers to it.
pub fn parse_call(v: &Value, name: &str) -> Result<Call, String> {
    let to = v.get("to").and_then(Value::as_str).map(str::trim).unwrap_or("");
    let to_hex = to.strip_prefix("0x").or_else(|| to.strip_prefix("0X")).unwrap_or("");
    if to_hex.len() != 40 || !to_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{name}: `to` is not an address"));
    }
    let gas_limit = quantity_field(v, "gasLimit", name)?
        .map(|g| u64::try_from(g).ok().filter(|g| *g > 0).ok_or_else(|| format!("{name}: `gasLimit` is not a positive gas count")))
        .transpose()?;
    Ok(Call {
        to: to.to_string(),
        value: quantity_field(v, "value", name)?.unwrap_or(0),
        data: normalize_data(v.get("data"), name)?,
        gas_limit,
        label: v.get("label").and_then(Value::as_str).unwrap_or("").trim().to_string(),
    })
}

/// How a refusal names call `i` (zero-based) to a human.
pub fn name_of(i: usize, v: &Value) -> String {
    match v.get("label").and_then(Value::as_str).map(str::trim).filter(|l| !l.is_empty()) {
        Some(l) => format!("call {} ({l})", i + 1),
        None => format!("call {}", i + 1),
    }
}

/// The same naming for a parsed call.
pub fn call_name(i: usize, c: &Call) -> String {
    if c.label.is_empty() { format!("call {}", i + 1) } else { format!("call {} ({})", i + 1, c.label) }
}

/// The `calls` array of a bundle request, in order.
pub fn parse_calls(req: &Value) -> Result<Vec<Call>, String> {
    let arr = req.get("calls").and_then(Value::as_array).ok_or("`calls` must be an array")?;
    if arr.is_empty() || arr.len() > MAX_CALLS {
        return Err(format!("a bundle carries between 1 and {MAX_CALLS} calls, not {}", arr.len()));
    }
    arr.iter().enumerate().map(|(i, v)| parse_call(v, &name_of(i, v))).collect()
}

/// The allowances the calls before `i` set, as call `i` will find them: one entry per
/// `(token, spender)` with the LAST approve winning, each tagged with the call that set it.
/// A reset-then-set pair therefore reads as the set, and the set itself is estimated with
/// the reset already applied.
pub fn approvals_before(calls: &[Call], i: usize) -> Vec<(usize, Approval)> {
    let mut out: Vec<(usize, Approval)> = Vec::new();
    for (j, c) in calls.iter().enumerate().take(i) {
        let Some(a) = parse_approve(&c.to, &c.data) else { continue };
        match out.iter_mut().find(|(_, e)| e.token == a.token && e.spender == a.spender) {
            Some(slot) => *slot = (j, a),
            None => out.push((j, a)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const ROUTER: &str = "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45";

    fn approve(amount: u64) -> Call {
        let data = format!("0x095ea7b3000000000000000000000000{}{:064x}", &ROUTER[2..].to_ascii_lowercase(), amount);
        Call { to: TOKEN.into(), value: 0, data, gas_limit: None, label: "Approve".into() }
    }

    fn swap() -> Call {
        Call { to: ROUTER.into(), value: 0, data: "0x5ae401dc".into(), gas_limit: None, label: "Swap".into() }
    }

    #[test]
    fn an_approve_only_reaches_the_calls_after_it() {
        let calls = [approve(5), swap(), approve(7)];
        assert!(approvals_before(&calls, 0).is_empty(), "the first call inherits nothing");
        let before_swap = approvals_before(&calls, 1);
        assert_eq!(before_swap.len(), 1);
        assert_eq!(before_swap[0].0, 0);
        assert_eq!(before_swap[0].1.amount(), "0x5");
        assert_eq!(approvals_before(&calls, 2).len(), 1, "a call does not see itself");
    }

    #[test]
    fn a_reset_then_set_reads_as_the_set_and_the_set_sees_the_reset() {
        let calls = [approve(0), approve(9), swap()];
        let before_set = approvals_before(&calls, 1);
        assert_eq!((before_set[0].0, before_set[0].1.amount()), (0, "0x0".to_string()));
        let before_swap = approvals_before(&calls, 2);
        assert_eq!(before_swap.len(), 1, "one entry per (token, spender)");
        assert_eq!((before_swap[0].0, before_swap[0].1.amount()), (1, "0x9".to_string()));
    }

    #[test]
    fn calls_are_parsed_strictly_and_named_for_a_human() {
        let req = json!({ "calls": [
            { "to": TOKEN, "data": "0x095EA7B3", "label": " Approve " },
            { "to": ROUTER, "value": "0x10", "gasLimit": "210000" },
            { "to": ROUTER, "value": "16" },
        ]});
        let calls = parse_calls(&req).unwrap();
        assert_eq!(calls[0].data, "0x095ea7b3");
        assert_eq!(calls[0].label, "Approve");
        assert_eq!((calls[1].value, calls[1].gas_limit), (16, Some(210_000)));
        assert_eq!((calls[2].value, calls[2].gas_limit, calls[2].data.as_str()), (16, None, "0x"));
        assert_eq!(name_of(0, &req["calls"][0]), "call 1 (Approve)");
        assert_eq!(name_of(1, &req["calls"][1]), "call 2");
        assert_eq!(call_name(0, &calls[0]), "call 1 (Approve)");
        assert_eq!(call_name(1, &calls[1]), "call 2");
        let tx = calls[2].tx("0x1111111111111111111111111111111111111111");
        assert_eq!(tx, json!({ "from": "0x1111111111111111111111111111111111111111", "to": ROUTER, "value": "0x10" }));

        let bad = |v: Value| parse_calls(&json!({ "calls": [v] })).unwrap_err();
        assert_eq!(bad(json!({ "to": "0x12" })), "call 1: `to` is not an address");
        assert_eq!(bad(json!({ "to": TOKEN, "data": "0xabc" })), "call 1: `data` is not even-length hex");
        assert_eq!(bad(json!({ "to": TOKEN, "gasLimit": "0", "label": "Swap" })), "call 1 (Swap): `gasLimit` is not a positive gas count");
        assert_eq!(bad(json!({ "to": TOKEN, "value": "lots" })), "call 1: `value` is not a quantity");
        assert!(parse_calls(&json!({ "calls": [] })).is_err());
        assert!(parse_calls(&json!({})).is_err());
    }

    #[test]
    fn quantities_read_both_spellings() {
        assert_eq!(parse_quantity("0x3b9aca00"), Some(1_000_000_000));
        assert_eq!(parse_quantity("1000000000"), Some(1_000_000_000));
        assert_eq!(parse_quantity("0x"), None);
        assert_eq!(parse_quantity("-1"), None);
        assert_eq!(parse_quantity("1e9"), None);
    }
}
