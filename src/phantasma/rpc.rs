//! A small JSON-RPC client for a Phantasma node, over one persistent HTTPS
//! connection (keep-alive), with a host allowlist and no redirects.

use std::cell::Cell;
use std::time::Duration;

use serde_json::{Value, json};
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig};

use super::keys::Address;
use super::tx::to_hex;
use super::vm::{self, VmValue};

#[derive(Debug)]
pub enum RpcError {
    /// No complete answer (timeout, connection lost): the outcome is unknown.
    Transport(String),
    /// The node answered with an error.
    Node(String),
    /// The answer could not be understood.
    Bad(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Transport(m) => write!(f, "no answer from the node: {m}"),
            RpcError::Node(m) => write!(f, "the node refused: {m}"),
            RpcError::Bad(m) => write!(f, "unreadable answer: {m}"),
        }
    }
}

impl std::error::Error for RpcError {}

pub type RpcResult<T> = Result<T, RpcError>;

/// Which root certificates verify the node's TLS certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsRoots {
    /// Mozilla's roots, built in (default).
    WebPki,
    /// The operating system's trust store (needed behind TLS-inspecting proxies).
    Platform,
}

/// One liquid balance (staked SOUL is not included).
#[derive(Clone, Debug)]
pub struct Balance {
    pub symbol: String,
    pub amount: u128,
    pub decimals: u8,
}

#[derive(Clone, Debug)]
pub struct TxEvent {
    pub kind: String,
    pub address: String,
    pub contract: String,
    pub data: String,
}

/// A transaction as the node reports it once it is in a block.
#[derive(Clone, Debug)]
pub struct TxReceipt {
    /// "Halt" when it ran; "Fault" or "Break" when it did not.
    pub state: String,
    pub debug_comment: String,
    pub result: String,
    pub events: Vec<TxEvent>,
    pub fee: String,
}

impl TxReceipt {
    pub fn succeeded(&self) -> bool {
        self.state == "Halt"
    }
}

pub struct Rpc {
    agent: Agent,
    url: String,
    chain: String,
    next_id: Cell<u64>,
    /// Unix seconds from the HTTP `Date` header of the last answer.
    last_server_time: Cell<Option<i64>>,
}

/// The host of an `https://host/path` URL, refusing user info and ports.
pub fn https_host(url: &str) -> Result<&str, String> {
    let rest = url.strip_prefix("https://").ok_or_else(|| format!("RPC URL must use https: {url}"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') || authority.contains(':') {
        return Err(format!("RPC URL must be https://<host>/… with no port or user: {url}"));
    }
    Ok(authority)
}

impl Rpc {
    /// A client for `url`, whose host must be in `allowed_hosts`.
    pub fn new(url: &str, chain: &str, allowed_hosts: &[String], roots: TlsRoots) -> Result<Self, String> {
        let host = https_host(url)?;
        if !allowed_hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
            return Err(format!(
                "RPC host {host} is not on the allowlist ({}). Add it to `rpc_allowed_hosts` only if you trust it.",
                allowed_hosts.join(", ")
            ));
        }
        let tls = TlsConfig::builder()
            .root_certs(match roots {
                TlsRoots::WebPki => RootCerts::WebPki,
                TlsRoots::Platform => RootCerts::PlatformVerifier,
            })
            .build();
        let config = Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(20)))
            .timeout_connect(Some(Duration::from_secs(8)))
            .max_idle_connections_per_host(2)
            .max_idle_age(Duration::from_secs(90))
            .user_agent(concat!("saturn-grid/", env!("CARGO_PKG_VERSION")))
            .tls_config(tls)
            .build();
        Ok(Rpc {
            agent: config.into(),
            url: url.to_string(),
            chain: chain.to_string(),
            next_id: Cell::new(1),
            last_server_time: Cell::new(None),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The node's clock (from the last answer's `Date` header), unix seconds.
    pub fn last_server_time(&self) -> Option<i64> {
        self.last_server_time.get()
    }

    fn call(&self, method: &str, params: Value) -> RpcResult<Value> {
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params, "id": id.to_string() }).to_string();
        let mut resp = self
            .agent
            .post(&self.url)
            .header("content-type", "application/json")
            .send(body.as_str())
            .map_err(|e| RpcError::Transport(format!("{method}: {e}")))?;
        if let Some(t) = resp.headers().get("date").and_then(|v| v.to_str().ok()).and_then(crate::clock::parse_http_date) {
            self.last_server_time.set(Some(t));
        }
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .with_config()
            .limit(8 * 1024 * 1024)
            .read_to_string()
            .map_err(|e| RpcError::Transport(format!("{method}: {e}")))?;
        let parsed: Option<Value> = serde_json::from_str(&text).ok();
        let failed = |e: &Value| !(e.is_null() || e.as_str() == Some(""));
        // The node's own words, when the answer carries a JSON-RPC error (also on HTTP 4xx).
        let node_error = parsed
            .as_ref()
            .and_then(|v| v.get("error"))
            .filter(|e| failed(e))
            .map(|e| e.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| e.to_string()));
        if status == 429 || status >= 500 {
            return Err(RpcError::Transport(format!("{method}: HTTP {status}")));
        }
        if let Some(msg) = node_error {
            return Err(RpcError::Node(format!("{method}: {}", clip(&msg, 400))));
        }
        if status != 200 {
            return Err(RpcError::Node(format!("{method}: HTTP {status} {}", clip(&text, 200))));
        }
        let v = parsed.ok_or_else(|| RpcError::Bad(format!("{method}: not JSON")))?;
        let result = v.get("result").cloned().unwrap_or(Value::Null);
        if let Some(e) = result.get("error").filter(|e| failed(e)) {
            return Err(RpcError::Node(format!("{method}: {}", clip(&e.to_string(), 400))));
        }
        Ok(result)
    }

    /// Run a read-only script; one decoded value per call in the script, in order.
    pub fn invoke_raw_script(&self, script: &[u8]) -> RpcResult<Vec<VmValue>> {
        let r = self.call("invokeRawScript", json!([self.chain, to_hex(script)]))?;
        let results =
            r.get("results").and_then(Value::as_array).ok_or_else(|| RpcError::Bad("invokeRawScript: no results".into()))?;
        results
            .iter()
            .map(|h| {
                let h = h.as_str().ok_or_else(|| RpcError::Bad("invokeRawScript: result is not hex".into()))?;
                vm::decode_hex(h).map_err(|e| RpcError::Bad(format!("invokeRawScript: {e}")))
            })
            .collect()
    }

    /// Liquid balances of an address.
    pub fn balances(&self, address: &Address) -> RpcResult<Vec<Balance>> {
        let r = self.call("getAccount", json!([address.text()]))?;
        let list = r.get("balances").and_then(Value::as_array).ok_or_else(|| RpcError::Bad("getAccount: no balances".into()))?;
        let mut out = Vec::with_capacity(list.len());
        for b in list {
            let symbol = b.get("symbol").and_then(Value::as_str).unwrap_or_default().to_string();
            let amount = b
                .get("amount")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<u128>().ok())
                .ok_or_else(|| RpcError::Bad(format!("getAccount: bad amount for {symbol}")))?;
            let decimals = b
                .get("decimals")
                .and_then(Value::as_u64)
                .and_then(|d| u8::try_from(d).ok())
                .ok_or_else(|| RpcError::Bad(format!("getAccount: bad decimals for {symbol}")))?;
            out.push(Balance { symbol, amount, decimals });
        }
        Ok(out)
    }

    /// A token's on-chain decimals.
    pub fn token_decimals(&self, symbol: &str) -> RpcResult<u8> {
        let r = self.call("getToken", json!([symbol, false]))?;
        let got = r.get("symbol").and_then(Value::as_str).unwrap_or_default();
        if got != symbol {
            return Err(RpcError::Bad(format!("getToken({symbol}) answered for {got:?}")));
        }
        r.get("decimals")
            .and_then(Value::as_u64)
            .and_then(|d| u8::try_from(d).ok())
            .filter(|&d| d <= 30)
            .ok_or_else(|| RpcError::Bad(format!("getToken({symbol}): no decimals")))
    }

    /// Broadcast a signed transaction; the node answers its hash.
    pub fn send_raw_transaction(&self, signed_hex: &str) -> RpcResult<String> {
        let r = self.call("sendRawTransaction", json!([signed_hex]))?;
        match r.as_str() {
            Some(h) if h.len() == 64 && h.bytes().all(|c| c.is_ascii_hexdigit()) => Ok(h.to_ascii_uppercase()),
            Some(other) => Err(RpcError::Node(format!("sendRawTransaction: {}", clip(other, 300)))),
            None => Err(RpcError::Bad("sendRawTransaction: no hash".into())),
        }
    }

    /// A transaction's receipt, or `None` while the node does not know it.
    pub fn transaction(&self, hash: &str) -> RpcResult<Option<TxReceipt>> {
        let r = match self.call("getTransaction", json!([hash])) {
            Ok(r) => r,
            Err(RpcError::Node(m)) if m.to_ascii_lowercase().contains("not found") => return Ok(None),
            Err(e) => return Err(e),
        };
        let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let state = s("state");
        if state.is_empty() {
            return Ok(None);
        }
        let events = r
            .get("events")
            .and_then(Value::as_array)
            .map(|evs| {
                evs.iter()
                    .map(|e| {
                        let g = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                        TxEvent { kind: g("kind"), address: g("address"), contract: g("contract"), data: g("data") }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Some(TxReceipt { state, debug_comment: clip(&s("debugComment"), 300), result: s("result"), events, fee: s("fee") }))
    }
}

/// Shorten untrusted text for logs (and strip control characters).
pub fn clip(s: &str, max: usize) -> String {
    let cleaned: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    if cleaned.chars().count() <= max {
        cleaned
    } else {
        let mut t: String = cleaned.chars().take(max).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parsing_and_allowlist() {
        assert_eq!(https_host("https://pharpc1.phantasma.info/rpc").unwrap(), "pharpc1.phantasma.info");
        assert!(https_host("http://pharpc1.phantasma.info/rpc").is_err());
        assert!(https_host("https://user@pharpc1.phantasma.info/rpc").is_err());
        assert!(https_host("https://pharpc1.phantasma.info:8443/rpc").is_err());
        let allowed = vec!["pharpc1.phantasma.info".to_string()];
        assert!(Rpc::new("https://evil.example/rpc", "main", &allowed, TlsRoots::WebPki).is_err());
        assert!(Rpc::new("https://pharpc1.phantasma.info/rpc", "main", &allowed, TlsRoots::WebPki).is_ok());
    }

    #[test]
    fn clip_strips_control_chars() {
        assert_eq!(clip("a\nb\u{1b}[31m", 100), "a b [31m");
        assert_eq!(clip("abcdef", 3), "abc…");
    }
}
