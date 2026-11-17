//! Saturn's public market-data API, used only to help find a pool id
//! (`saturn-grid pools`). Trading never depends on it: the bot reads the chain.

use std::time::Duration;

use serde_json::Value;
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig};

use crate::phantasma::rpc::{TlsRoots, clip};
use crate::saturn::{Network, valid_symbol};

const API: &str = "https://apiops.saturnx.cc";

pub struct PoolRow {
    pub id: String,
    pub token_a: String,
    pub token_b: String,
    pub fee_bps: u64,
    pub tvl_usd: Option<f64>,
    pub burned: bool,
}

/// Active v4 pools between `a` and `b`, by USD liquidity (deepest first).
pub fn pools_for_pair(network: Network, a: &str, b: &str, roots: TlsRoots) -> Result<Vec<PoolRow>, String> {
    if !valid_symbol(a) || !valid_symbol(b) || a == b {
        return Err("give two different token symbols, like --pair RA,TAZ".into());
    }
    let tls = TlsConfig::builder()
        .root_certs(match roots {
            TlsRoots::WebPki => RootCerts::WebPki,
            TlsRoots::Platform => RootCerts::PlatformVerifier,
        })
        .build();
    let agent: Agent = Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent(concat!("saturn-grid/", env!("CARGO_PKG_VERSION")))
        .tls_config(tls)
        .build()
        .into();
    let url = format!(
        "{API}/v1/pools?network={}&pair={a},{b}&active=true&version=v4&fields=id,tokenA,tokenB,feeBps,tvlUsd,burned",
        network.name()
    );
    let text = agent
        .get(&url)
        .call()
        .map_err(|e| format!("Saturn API: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("Saturn API: {e}"))?;
    let v: Value = serde_json::from_str(&text).map_err(|_| "Saturn API: not JSON".to_string())?;
    if let Some(e) = v.get("error") {
        return Err(format!("Saturn API: {}", clip(&e.to_string(), 300)));
    }
    let rows = v.get("data").and_then(Value::as_array).ok_or("Saturn API: no data")?;
    Ok(rows
        .iter()
        .map(|p| {
            let s = |k: &str| p.get(k).and_then(Value::as_str).map(|x| clip(x, 40)).unwrap_or_default();
            PoolRow {
                id: s("id"),
                token_a: s("tokenA"),
                token_b: s("tokenB"),
                fee_bps: p.get("feeBps").and_then(Value::as_u64).unwrap_or(0),
                tvl_usd: p.get("tvlUsd").and_then(Value::as_f64),
                burned: p.get("burned").and_then(Value::as_bool).unwrap_or(false),
            }
        })
        .collect())
}
