//! The TOML config, validated up front (unknown keys are errors, so a typo
//! never silently falls back to a default).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::grid::Spacing;
use crate::phantasma::rpc::TlsRoots;
use crate::saturn::{Network, parse_pool_id, valid_symbol};
use crate::units::{PRICE_DECIMALS, parse_units};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    network: String,
    pool: String,
    base: String,
    quote: String,
    grid: RawGrid,
    #[serde(default)]
    risk: RawRisk,
    #[serde(default)]
    engine: RawEngine,
    #[serde(default)]
    files: RawFiles,
    #[serde(default)]
    rpc: RawRpc,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGrid {
    lower: String,
    upper: String,
    levels: usize,
    #[serde(default = "default_spacing")]
    spacing: Spacing,
    quote_per_order: String,
    #[serde(default)]
    compound: bool,
}

fn default_spacing() -> Spacing {
    Spacing::Geometric
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawRisk {
    slippage_bps: u32,
    max_price_impact_bps: u32,
    min_profit_bps: i64,
    stop_outside_range_bps: u32,
    max_consecutive_failures: u32,
    max_trades_per_hour: u32,
    allow_unfunded_start: bool,
}

impl Default for RawRisk {
    fn default() -> Self {
        RawRisk {
            slippage_bps: 50,
            max_price_impact_bps: 100,
            min_profit_bps: 10,
            stop_outside_range_bps: 1_000,
            max_consecutive_failures: 3,
            max_trades_per_hour: 60,
            allow_unfunded_start: false,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawEngine {
    poll_ms: u64,
    confirm_poll_ms: u64,
    tx_ttl_secs: u32,
    max_clock_skew_secs: i64,
}

impl Default for RawEngine {
    fn default() -> Self {
        RawEngine { poll_ms: 1_000, confirm_poll_ms: 500, tx_ttl_secs: 90, max_clock_skew_secs: 30 }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct RawFiles {
    key: Option<String>,
    state: Option<String>,
    paper_state: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct RawRpc {
    url: Option<String>,
    allowed_hosts: Vec<String>,
    tls_roots: Option<String>,
}

/// The validated config.
#[derive(Clone, Debug)]
pub struct Config {
    pub network: Network,
    pub pool_id: u64,
    pub base: String,
    pub quote: String,
    pub lower: u128,
    pub upper: u128,
    pub levels: usize,
    pub spacing: Spacing,
    /// Quote per order, as written (converted to raw once decimals are read on chain).
    pub quote_per_order: String,
    pub compound: bool,
    pub slippage_bps: u32,
    pub max_price_impact_bps: u32,
    pub min_profit_bps: i64,
    pub stop_outside_range_bps: u32,
    pub max_consecutive_failures: u32,
    pub max_trades_per_hour: u32,
    pub allow_unfunded_start: bool,
    pub poll_ms: u64,
    pub confirm_poll_ms: u64,
    pub tx_ttl_secs: u32,
    pub max_clock_skew_secs: i64,
    pub key_file: PathBuf,
    pub state_file: PathBuf,
    pub paper_state_file: PathBuf,
    pub rpc_url: String,
    pub rpc_allowed_hosts: Vec<String>,
    pub tls_roots: TlsRoots,
}

fn range<T: PartialOrd + std::fmt::Display>(name: &str, v: T, lo: T, hi: T) -> Result<T, String> {
    if v < lo || v > hi { Err(format!("{name} must be between {lo} and {hi} (got {v})")) } else { Ok(v) }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
        Self::parse(&text, path.parent().unwrap_or(Path::new(".")))
    }

    /// Parse and validate; relative paths are taken from `base_dir`.
    pub fn parse(text: &str, base_dir: &Path) -> Result<Self, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| format!("config: {e}"))?;
        let network = Network::parse(&raw.network)?;
        let pool_id = parse_pool_id(&raw.pool)?;
        for (k, v) in [("base", &raw.base), ("quote", &raw.quote)] {
            if !valid_symbol(v) {
                return Err(format!("{k} must be a token symbol in capitals (like TAZ), not {v:?}"));
            }
        }
        if raw.base == raw.quote {
            return Err("base and quote must be different tokens".into());
        }
        let lower = parse_units(&raw.grid.lower, PRICE_DECIMALS).map_err(|e| format!("grid.lower: {e}"))?;
        let upper = parse_units(&raw.grid.upper, PRICE_DECIMALS).map_err(|e| format!("grid.upper: {e}"))?;
        if lower == 0 || upper <= lower {
            return Err("grid.lower must be above 0 and below grid.upper".into());
        }
        let levels = range("grid.levels", raw.grid.levels, 1, 200)?;
        if parse_units(&raw.grid.quote_per_order, 30).map_err(|e| format!("grid.quote_per_order: {e}"))? == 0 {
            return Err("grid.quote_per_order must be above 0".into());
        }
        let r = &raw.risk;
        let e = &raw.engine;
        let rel = |p: &str| -> PathBuf {
            let p = PathBuf::from(p);
            if p.is_absolute() { p } else { base_dir.join(p) }
        };
        let tag = format!("{}-v4-{pool_id}-{}-{}", network.name(), raw.base, raw.quote).to_lowercase();
        let state_file = rel(raw.files.state.as_deref().unwrap_or(&format!("state/{tag}.json")));
        let paper_state_file = rel(raw.files.paper_state.as_deref().unwrap_or(&format!("state/{tag}.paper.json")));
        if state_file == paper_state_file {
            return Err("the paper state file must differ from the live one".into());
        }
        let mut rpc_allowed_hosts = vec![network.default_rpc_host().to_string()];
        rpc_allowed_hosts.extend(raw.rpc.allowed_hosts.iter().map(|h| h.trim().to_ascii_lowercase()));
        let tls_roots = match raw.rpc.tls_roots.as_deref().unwrap_or("webpki") {
            "webpki" => TlsRoots::WebPki,
            "platform" => TlsRoots::Platform,
            other => return Err(format!("rpc.tls_roots must be \"webpki\" or \"platform\", not {other:?}")),
        };
        Ok(Config {
            network,
            pool_id,
            base: raw.base,
            quote: raw.quote,
            lower,
            upper,
            levels,
            spacing: raw.grid.spacing,
            quote_per_order: raw.grid.quote_per_order,
            compound: raw.grid.compound,
            // Saturn caps a link's slippage at 3%; so does this bot.
            slippage_bps: range("risk.slippage_bps", r.slippage_bps, 1, 300)?,
            max_price_impact_bps: range("risk.max_price_impact_bps", r.max_price_impact_bps, 1, 1_000)?,
            min_profit_bps: range("risk.min_profit_bps", r.min_profit_bps, -10_000, 10_000)?,
            stop_outside_range_bps: range("risk.stop_outside_range_bps", r.stop_outside_range_bps, 0, 100_000)?,
            max_consecutive_failures: range("risk.max_consecutive_failures", r.max_consecutive_failures, 1, 100)?,
            max_trades_per_hour: range("risk.max_trades_per_hour", r.max_trades_per_hour, 1, 3_600)?,
            allow_unfunded_start: r.allow_unfunded_start,
            // The public RPC is shared: no faster than 4 reads a second.
            poll_ms: range("engine.poll_ms", e.poll_ms, 250, 600_000)?,
            confirm_poll_ms: range("engine.confirm_poll_ms", e.confirm_poll_ms, 250, 10_000)?,
            tx_ttl_secs: range("engine.tx_ttl_secs", e.tx_ttl_secs, 30, 600)?,
            max_clock_skew_secs: range("engine.max_clock_skew_secs", e.max_clock_skew_secs, 1, 600)?,
            key_file: rel(raw.files.key.as_deref().unwrap_or(&format!("keys/{}.json", network.name()))),
            state_file,
            paper_state_file,
            rpc_url: raw.rpc.url.unwrap_or_else(|| network.default_rpc().to_string()),
            rpc_allowed_hosts,
            tls_roots,
        })
    }

    /// A digest of everything that shapes the grid: resuming a state made
    /// under other settings is refused.
    pub fn fingerprint(&self) -> String {
        let s = format!(
            "{}|{}|{}|{}|{}|{}|{}|{:?}|{}|{}",
            self.network.name(),
            self.pool_id,
            self.base,
            self.quote,
            self.lower,
            self.upper,
            self.levels,
            self.spacing,
            self.quote_per_order,
            self.compound
        );
        let d = Sha256::digest(s.as_bytes());
        d[..8].iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = r#"
        network = "devnet"
        pool = "v4:150"
        base = "SOUL"
        quote = "KCAL"
        [grid]
        lower = "8"
        upper = "12"
        levels = 10
        quote_per_order = "25"
    "#;

    #[test]
    fn minimal_config_gets_safe_defaults() {
        let c = Config::parse(MIN, Path::new("/srv/bot")).unwrap();
        assert_eq!(c.network, Network::Devnet);
        assert_eq!(c.pool_id, 150);
        assert_eq!(c.spacing, Spacing::Geometric);
        assert_eq!(c.slippage_bps, 50);
        assert_eq!(c.key_file, PathBuf::from("/srv/bot/keys/devnet.json"));
        assert_eq!(c.state_file, PathBuf::from("/srv/bot/state/devnet-v4-150-soul-kcal.json"));
        assert_eq!(c.rpc_url, "https://devnet.phantasma.info/rpc");
        assert_eq!(c.rpc_allowed_hosts, vec!["devnet.phantasma.info".to_string()]);
    }

    #[test]
    fn typos_and_bad_values_are_errors() {
        assert!(Config::parse(&MIN.replace("levels", "level"), Path::new(".")).is_err(), "unknown key");
        assert!(Config::parse(&format!("{MIN}\n[risk]\nslippage_bps = 900\n"), Path::new(".")).is_err(), "slippage cap");
        assert!(Config::parse(&MIN.replace("\"12\"", "\"7\""), Path::new(".")).is_err(), "upper below lower");
        assert!(Config::parse(&MIN.replace("v4:150", "v2:SOUL_KCAL"), Path::new(".")).is_err(), "v2 pool");
        assert!(Config::parse(&MIN.replace("\"KCAL\"", "\"SOUL\""), Path::new(".")).is_err(), "same token");
        assert!(Config::parse(&format!("{MIN}\n[engine]\npoll_ms = 10\n"), Path::new(".")).is_err(), "too fast");
    }

    #[test]
    fn fingerprint_tracks_the_grid() {
        let a = Config::parse(MIN, Path::new(".")).unwrap();
        let b = Config::parse(&MIN.replace("levels = 10", "levels = 11"), Path::new(".")).unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint(), Config::parse(MIN, Path::new(".")).unwrap().fingerprint());
    }
}
