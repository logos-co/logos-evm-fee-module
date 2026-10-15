//! A failed `eth_rpc` round trip as a clause a person can read, beside the code a caller acts
//! on. Pure, so `cargo test --no-default-features` covers the wording.

use std::time::Duration;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub code: String,
    pub why: String,
}

/// A refusal of our own, worded where it happened: no code to add.
impl From<String> for Failure {
    fn from(why: String) -> Self {
        Failure { code: String::new(), why }
    }
}

impl From<&str> for Failure {
    fn from(why: &str) -> Self {
        why.to_string().into()
    }
}

/// Who answers a chain's reads: its RPC endpoint, or the verified proxy in front of it.
pub fn endpoint(name: &str, verified: bool) -> String {
    if verified { format!("{name}'s verified proxy") } else { format!("{name}'s RPC endpoint") }
}

/// "8 s", "2.5 s".
pub fn secs(d: Duration) -> String {
    let tenths = (d.as_millis() + 50) / 100;
    if tenths % 10 == 0 { format!("{} s", tenths / 10) } else { format!("{}.{} s", tenths / 10, tenths % 10) }
}

/// The SDK's own failure, whose message is the runtime's `{ code, message }` object.
pub fn transport(message: &str, endpoint: &str, rt: Duration) -> Failure {
    let v: Value = serde_json::from_str(message).unwrap_or(Value::Null);
    let code = v.get("code").and_then(Value::as_str).unwrap_or("transport").to_string();
    if code == "timeout" {
        return Failure { code, why: format!("{endpoint} didn't answer within {}", secs(rt)) };
    }
    let detail = v.get("message").and_then(Value::as_str).unwrap_or(message);
    Failure { code, why: format!("eth_rpc_module could not be asked ({detail})") }
}

/// eth_rpc's refusal `{ ok: false, code, error }`, or `None` for anything else.
pub fn refusal(reply: &str, endpoint: &str, rt: Duration) -> Option<Failure> {
    let v: Value = serde_json::from_str(reply).ok()?;
    if v.get("ok").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    let code = v.get("code").and_then(Value::as_str).unwrap_or("rpc").to_string();
    let why = if code == "timeout" {
        format!("{endpoint} didn't answer within {}", secs(rt))
    } else {
        v.get("error").and_then(Value::as_str).unwrap_or("rpc failed").to_string()
    };
    Some(Failure { code, why })
}

/// Refused before it started: what was left could not hold one round trip.
pub fn no_time(left: Duration, endpoint: &str, rt: Duration) -> Failure {
    Failure {
        code: "not_enough_time".into(),
        why: format!("only {} of this request's time was left, and {endpoint} is allowed {} per call",
                     secs(left), secs(rt)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENDPOINT: &str = "Ethereum's RPC endpoint";

    /// The demo's own failure, verbatim from the send form.
    #[test]
    fn a_transport_timeout_reads_as_the_endpoint_not_answering() {
        let m = r#"{"code":"timeout","message":"call to 'eth_rpc_module.estimate_gas' timed out after 1010ms","origin":"eth_rpc_module"}"#;
        let f = transport(m, ENDPOINT, Duration::from_secs(8));
        assert_eq!(f.code, "timeout");
        assert_eq!(f.why, "Ethereum's RPC endpoint didn't answer within 8 s");
        let other = transport(r#"{"code":"not_loaded","message":"no such module"}"#, ENDPOINT, Duration::from_secs(8));
        assert_eq!((other.code.as_str(), other.why.as_str()), ("not_loaded", "eth_rpc_module could not be asked (no such module)"));
        assert_eq!(transport("lp_invoke failed with code 3", ENDPOINT, Duration::from_secs(8)).code, "transport");
    }

    #[test]
    fn eth_rpcs_own_refusal_keeps_its_code_and_its_words() {
        let t = refusal(r#"{"ok":false,"code":"timeout","error":"no answer within 8000ms: x"}"#, ENDPOINT, Duration::from_secs(8));
        assert_eq!(t.unwrap().why, "Ethereum's RPC endpoint didn't answer within 8 s");
        let r = refusal(r#"{"ok":false,"code":"rpc","error":"rpc error 3: execution reverted"}"#, ENDPOINT, Duration::from_secs(8)).unwrap();
        assert_eq!((r.code.as_str(), r.why.as_str()), ("rpc", "rpc error 3: execution reverted"));
        assert_eq!(refusal(r#"{"ok":true,"result":"0x1"}"#, ENDPOINT, Duration::from_secs(8)), None);
    }

    #[test]
    fn a_refusal_before_the_call_says_what_was_left_and_what_one_call_needs() {
        let f = no_time(Duration::from_millis(1_010), ENDPOINT, Duration::from_secs(8));
        assert_eq!(f.code, "not_enough_time");
        assert_eq!(f.why, "only 1 s of this request's time was left, and Ethereum's RPC endpoint is allowed 8 s per call");
        assert_eq!(secs(Duration::from_millis(2_460)), "2.5 s");
        assert_eq!(endpoint("Ethereum", true), "Ethereum's verified proxy");
    }
}
