//! Saturn DEX v4 on Phantasma: on-chain pool reads, the exact swap quote,
//! and the swap transaction, exactly as the Saturn website builds it.
//!
//! What was verified against mainnet (November 2026):
//! - swaps call `saturnswap.swap(from, poolId, amountIn, tokenIn, tokenOut,
//!   minAmountOut)` with raw amounts, under `AllowGas(from, Null, 100000,
//!   90000)` … `SpendGas(from)`, payload `SATURN_swap <IN>><OUT>`;
//! - `saturnpools` keeps reserves normalised to 8 decimals ("scaled");
//! - out = ⌊net·R_out / (R_in + net)⌋ with net = in − ⌊in·fee/10000⌋, all
//!   scaled, then scaled back down to the output token's raw units.
//!   This reproduces a real fill to the unit (see the tests).

use crate::phantasma::keys::Address;
use crate::phantasma::rpc::{Rpc, TxReceipt};
use crate::phantasma::script::{Arg, ScriptBuilder};
use crate::phantasma::tx::Transaction;
use crate::phantasma::vm::{self, VmValue};
use crate::units::{mul_div, pow10};

pub const SWAP_CONTRACT: &str = "saturnswap";
pub const POOLS_CONTRACT: &str = "saturnpools";
pub const ROUTER_CONTRACT: &str = "saturnrouter";

/// The website's v4 gas: price 100,000 × limit 90,000 per hop (at most 0.9 KCAL; unused gas is refunded).
pub const GAS_PRICE: u64 = 100_000;
pub const GAS_LIMIT_PER_HOP: u64 = 90_000;
/// KCAL a single-hop swap may need for gas, raw (10 decimals).
pub const GAS_KCAL_RAW: u128 = GAS_PRICE as u128 * GAS_LIMIT_PER_HOP as u128;
/// Phantasma charges a small data fee in SOUL on every transaction; the site asks for 1 SOUL free.
pub const SOUL_FEE_RESERVE_RAW: u128 = 100_000_000;
/// Extra SOUL the site keeps when SOUL itself is being sold (0.01 SOUL).
pub const SOUL_SELL_EXTRA_RAW: u128 = 1_000_000;
pub const TARGET_DECIMALS: u8 = 8;
/// saturnrouter.getMaxTruncationPercent: the most of an input that scaling may cut off.
pub const MAX_TRUNCATION_PCT: u128 = 10;
/// The major versions this bot was written and verified against.
pub const SWAP_VERSION_PREFIX: &str = "saturnswap-4.";
pub const POOLS_VERSION_PREFIX: &str = "saturnpools-4.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    Devnet,
}

impl Network {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mainnet" => Ok(Network::Mainnet),
            "devnet" | "testnet" => Ok(Network::Devnet),
            other => Err(format!("network must be \"mainnet\" or \"devnet\", not {other:?}")),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Network::Mainnet => "mainnet",
            Network::Devnet => "devnet",
        }
    }
    /// The nexus name signed into every transaction.
    pub fn nexus(self) -> &'static str {
        match self {
            Network::Mainnet => "mainnet",
            Network::Devnet => "testnet",
        }
    }
    pub fn chain(self) -> &'static str {
        "main"
    }
    pub fn default_rpc(self) -> &'static str {
        match self {
            Network::Mainnet => "https://pharpc1.phantasma.info/rpc",
            Network::Devnet => "https://devnet.phantasma.info/rpc",
        }
    }
    pub fn default_rpc_host(self) -> &'static str {
        match self {
            Network::Mainnet => "pharpc1.phantasma.info",
            Network::Devnet => "devnet.phantasma.info",
        }
    }
    pub fn explorer_tx(self, hash: &str) -> String {
        match self {
            Network::Mainnet => format!("https://explorer.phantasma.info/tx/{hash}"),
            Network::Devnet => format!("https://devnet-explorer.phantasma.info/tx/{hash}"),
        }
    }
}

/// A token symbol as the chain writes it: upper case, 1–10 characters.
pub fn valid_symbol(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b.len() <= 10 && b[0].is_ascii_uppercase() && b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// Parse a v4 pool id ("v4:33" or "33").
pub fn parse_pool_id(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let n = t.strip_prefix("v4:").or_else(|| t.strip_prefix("V4:")).unwrap_or(t);
    match n.parse::<u64>() {
        Ok(id) if id > 0 => Ok(id),
        _ => Err(format!("pool must be a v4 pool id like \"v4:33\", not {s:?} (v2/\"V3\" pools are not supported)")),
    }
}

/// How saturnpools turns raw units into 8-decimal "scaled" units: raw × factor / divisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    pub factor: u128,
    pub divisor: u128,
}

impl Scale {
    pub fn for_decimals(d: u8) -> Option<Scale> {
        Some(if d <= TARGET_DECIMALS {
            Scale { factor: pow10(TARGET_DECIMALS - d)?, divisor: 1 }
        } else {
            Scale { factor: 1, divisor: pow10(d - TARGET_DECIMALS)? }
        })
    }
    pub fn up(&self, raw: u128) -> Option<u128> {
        mul_div(raw, self.factor, self.divisor)
    }
    pub fn down(&self, scaled: u128) -> Option<u128> {
        mul_div(scaled, self.divisor, self.factor)
    }
}

/// A token as the bot uses it.
#[derive(Clone, Debug)]
pub struct Token {
    pub symbol: String,
    pub decimals: u8,
    pub scale: Scale,
    /// saturnrouter.getMinRawForSwap: the smallest input a swap accepts.
    pub min_swap_raw: u128,
}

#[derive(Clone, Debug)]
pub struct Pool {
    pub id: u64,
    pub token_a: String,
    pub token_b: String,
    pub reserves: Reserves,
}

/// The fast-changing part of a pool, read every tick in one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reserves {
    /// Scaled (8-decimal) reserve of token A.
    pub a: u128,
    /// Scaled (8-decimal) reserve of token B.
    pub b: u128,
    pub fee_bps: u32,
    pub active: bool,
}

/// An exact-input swap quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    pub amount_in: u128,
    pub in_scaled: u128,
    pub fee_scaled: u128,
    pub out_scaled: u128,
    pub amount_out: u128,
    /// The swap's own price impact (fee excluded), in basis points: net / (R_in + net).
    pub impact_bps: u32,
}

/// The exact v4 quote (matches the contract to the unit).
pub fn quote_exact_in(
    reserve_in: u128,
    reserve_out: u128,
    fee_bps: u32,
    scale_in: Scale,
    scale_out: Scale,
    amount_in: u128,
) -> Result<Quote, String> {
    if reserve_in == 0 || reserve_out == 0 {
        return Err("the pool is empty".into());
    }
    if fee_bps >= 10_000 {
        return Err(format!("pool fee {fee_bps} bps is not plausible"));
    }
    let in_scaled = scale_in.up(amount_in).ok_or("amount too large")?;
    if in_scaled == 0 {
        return Err("the amount rounds to zero at 8 decimals".into());
    }
    let kept = scale_in.down(in_scaled).ok_or("amount too large")?;
    let cut = amount_in.saturating_sub(kept);
    if cut.saturating_mul(100) > amount_in.saturating_mul(MAX_TRUNCATION_PCT) {
        return Err("more than 10% of the amount is lost to 8-decimal scaling".into());
    }
    let fee_scaled = mul_div(in_scaled, u128::from(fee_bps), 10_000).ok_or("overflow")?;
    let net = in_scaled - fee_scaled;
    if net == 0 {
        return Err("the amount is all fee".into());
    }
    let denom = reserve_in.checked_add(net).ok_or("overflow")?;
    let out_scaled = mul_div(net, reserve_out, denom).ok_or("overflow")?;
    let amount_out = scale_out.down(out_scaled).ok_or("overflow")?;
    if amount_out == 0 {
        return Err("the output rounds to zero".into());
    }
    let impact = mul_div(net, 10_000, denom).ok_or("overflow")?;
    Ok(Quote { amount_in, in_scaled, fee_scaled, out_scaled, amount_out, impact_bps: impact.min(10_000) as u32 })
}

/// The swap transaction, byte-for-byte as the Saturn website builds it.
#[allow(clippy::too_many_arguments)]
pub fn swap_tx(
    network: Network,
    from: &Address,
    pool_id: u64,
    token_in: &str,
    token_out: &str,
    amount_in: u128,
    min_out: u128,
    expiration: u32,
) -> Transaction {
    let (pool, amount, min) = (pool_id.to_string(), amount_in.to_string(), min_out.to_string());
    let mut sb = ScriptBuilder::new();
    sb.allow_gas(Arg::Addr(from), GAS_PRICE, GAS_LIMIT_PER_HOP);
    sb.call_contract(
        SWAP_CONTRACT,
        "swap",
        &[Arg::Addr(from), Arg::Str(&pool), Arg::Str(&amount), Arg::Str(token_in), Arg::Str(token_out), Arg::Str(&min)],
    );
    sb.spend_gas(Arg::Addr(from));
    Transaction {
        nexus: network.nexus().to_string(),
        chain: network.chain().to_string(),
        script: sb.end(),
        expiration,
        payload: format!("SATURN_swap {token_in}>{token_out}").into_bytes(),
    }
}

/// The amount a confirmed swap paid out: from saturnswap's event (checked
/// against what was sent), else from the transaction's result.
pub fn swap_amount_out(
    r: &TxReceipt,
    from: &Address,
    pool_id: u64,
    token_in: &str,
    token_out: &str,
    amount_in: u128,
) -> Option<u128> {
    let me = from.text();
    for e in r.events.iter().filter(|e| e.contract == SWAP_CONTRACT && e.address == me) {
        let Ok(v) = vm::decode_hex(&e.data) else { continue };
        let num = |k: &str| v.field(k).and_then(|x| x.as_u128().ok());
        let text = |k: &str| v.field(k).and_then(VmValue::as_str);
        if num("poolId") == Some(u128::from(pool_id))
            && text("tokenIn") == Some(token_in)
            && text("tokenOut") == Some(token_out)
            && num("amountIn") == Some(amount_in)
        {
            return num("amountOut");
        }
    }
    vm::decode_hex(&r.result).ok()?.as_u128().ok().filter(|&x| x > 0)
}

fn view(sb: &mut ScriptBuilder, contract: &str, method: &str, args: &[&str]) {
    let args: Vec<Arg<'_>> = args.iter().map(|a| Arg::Str(a)).collect();
    sb.call_contract(contract, method, &args);
}

/// Run read-only calls in one request; exactly one answer per call.
fn views(rpc: &Rpc, script: &[u8], n: usize) -> Result<Vec<VmValue>, String> {
    let out = rpc.invoke_raw_script(script).map_err(|e| e.to_string())?;
    if out.len() < n {
        return Err(format!("the node answered {} of {n} reads", out.len()));
    }
    Ok(out)
}

/// The one-request script read every tick: reserves A and B, active flag, fee.
pub fn reserves_script(pool_id: u64) -> Vec<u8> {
    let id = pool_id.to_string();
    let mut sb = ScriptBuilder::new();
    for m in ["getPoolReserveA", "getPoolReserveB", "getPoolActive", "getPoolFee"] {
        view(&mut sb, POOLS_CONTRACT, m, &[&id]);
    }
    sb.end()
}

pub fn read_reserves(rpc: &Rpc, script: &[u8]) -> Result<Reserves, String> {
    let r = views(rpc, script, 4)?;
    let fee = r[3].as_u128()?;
    Ok(Reserves {
        a: r[0].as_u128()?,
        b: r[1].as_u128()?,
        active: r[2].as_u128()? == 1,
        fee_bps: u32::try_from(fee).ok().filter(|&f| f < 10_000).ok_or_else(|| format!("implausible pool fee {fee}"))?,
    })
}

pub fn read_pool(rpc: &Rpc, pool_id: u64) -> Result<Pool, String> {
    let id = pool_id.to_string();
    let mut sb = ScriptBuilder::new();
    view(&mut sb, POOLS_CONTRACT, "getPoolTokenA", &[&id]);
    view(&mut sb, POOLS_CONTRACT, "getPoolTokenB", &[&id]);
    let r = views(rpc, &sb.end(), 2).map_err(|e| format!("pool v4:{pool_id}: {e}"))?;
    let (a, b) = (r[0].as_str().unwrap_or_default(), r[1].as_str().unwrap_or_default());
    if !valid_symbol(a) || !valid_symbol(b) {
        return Err(format!("pool v4:{pool_id} does not exist on this network"));
    }
    let reserves = read_reserves(rpc, &reserves_script(pool_id))?;
    Ok(Pool { id: pool_id, token_a: a.to_string(), token_b: b.to_string(), reserves })
}

pub fn read_token(rpc: &Rpc, symbol: &str) -> Result<Token, String> {
    if !valid_symbol(symbol) {
        return Err(format!("not a token symbol: {symbol:?}"));
    }
    let decimals = rpc.token_decimals(symbol).map_err(|e| e.to_string())?;
    let mut sb = ScriptBuilder::new();
    view(&mut sb, POOLS_CONTRACT, "getScaleFactor", &[symbol]);
    view(&mut sb, POOLS_CONTRACT, "getScaleDivisor", &[symbol]);
    view(&mut sb, ROUTER_CONTRACT, "getMinRawForSwap", &[symbol]);
    let r = views(rpc, &sb.end(), 3).map_err(|e| format!("{symbol}: {e}"))?;
    let (factor, divisor, min_swap_raw) = (r[0].as_u128()?, r[1].as_u128()?, r[2].as_u128()?);
    let expected = Scale::for_decimals(decimals).ok_or_else(|| format!("{symbol}: {decimals} decimals unsupported"))?;
    // An unset factor (0) means no scaling, which only fits a token of exactly 8 decimals.
    let consistent = (factor == 0 && decimals == TARGET_DECIMALS) || (Scale { factor, divisor }) == expected;
    if !consistent {
        return Err(format!(
            "{symbol}: saturnpools scales it by {factor}/{divisor}, which does not fit its {decimals} decimals; refusing to trade it"
        ));
    }
    let scale = expected;
    Ok(Token { symbol: symbol.to_string(), decimals, scale, min_swap_raw })
}

pub fn read_versions(rpc: &Rpc) -> Result<(String, String), String> {
    let mut sb = ScriptBuilder::new();
    view(&mut sb, SWAP_CONTRACT, "getContractVersion", &[]);
    view(&mut sb, POOLS_CONTRACT, "getContractVersion", &[]);
    let r = views(rpc, &sb.end(), 2)?;
    Ok((r[0].as_str().unwrap_or_default().to_string(), r[1].as_str().unwrap_or_default().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_match_the_chain() {
        // Read from mainnet saturnpools: TAZ (9) 1/10, KCAL (10) 1/100, SOUL (8) 1/1.
        assert_eq!(Scale::for_decimals(9), Some(Scale { factor: 1, divisor: 10 }));
        assert_eq!(Scale::for_decimals(10), Some(Scale { factor: 1, divisor: 100 }));
        assert_eq!(Scale::for_decimals(8), Some(Scale { factor: 1, divisor: 1 }));
        assert_eq!(Scale::for_decimals(6), Some(Scale { factor: 100, divisor: 1 }));
    }

    #[test]
    fn quote_reproduces_a_real_fill() {
        // Public mainnet swap 3DE618B3…E053: 100 TAZ -> RA in pool v4:33 (RA/TAZ, fee 30 bps)
        // paid exactly 2.156190850 RA. Pre-swap reserves rebuilt from its PoolReservesChanged event.
        let (res_ra, res_taz) = (360_516_380_390u128, 16_659_928_689_662u128);
        let s9 = Scale::for_decimals(9).unwrap();
        let q = quote_exact_in(res_taz, res_ra, 30, s9, s9, 100_000_000_000).unwrap();
        assert_eq!(q.in_scaled, 10_000_000_000);
        assert_eq!(q.fee_scaled, 30_000_000);
        assert_eq!(q.out_scaled, 215_619_085);
        assert_eq!(q.amount_out, 2_156_190_850);
        assert_eq!(q.impact_bps, 5); // 0.0598% -> 5 bps (rounded down)
    }

    #[test]
    fn quote_refusals() {
        let s = Scale::for_decimals(8).unwrap();
        assert!(quote_exact_in(0, 10, 30, s, s, 1_000).is_err());
        assert!(quote_exact_in(10, 10, 30, s, s, 0).is_err());
        let s10 = Scale::for_decimals(10).unwrap();
        // 150 raw KCAL scales to 1 (50 cut = 33%): the contract would refuse.
        assert!(quote_exact_in(1_000_000, 1_000_000, 30, s10, s, 150).is_err());
    }

    #[test]
    fn swap_script_matches_the_website() {
        // The script of public mainnet swap 3DE618B3…E053, as the Saturn website built it.
        let from = Address::from_text("P2KCSArwVBhCRkkJqq4HWaQAUkqQKyvVs8nMr4Hw2XWxqgd").unwrap();
        let tx = swap_tx(Network::Mainnet, &from, 33, "TAZ", "RA", 100_000_000_000, 2_091_505_124, 1_794_679_448);
        assert_eq!(tx.hash(), "3DE618B351D0CEAB8EA0BB88E4D39AAD20D5A998D833B8D8AB6885445F37E053");
        assert_eq!(tx.payload, b"SATURN_swap TAZ>RA");
    }

    #[test]
    fn ids_and_symbols() {
        assert_eq!(parse_pool_id("v4:33"), Ok(33));
        assert_eq!(parse_pool_id("33"), Ok(33));
        assert!(parse_pool_id("v2:TAZ_KCAL").is_err());
        assert!(parse_pool_id("v4:0").is_err());
        assert!(valid_symbol("TAZ") && valid_symbol("C") && valid_symbol("GHOSTDOGS"));
        assert!(!valid_symbol("taz") && !valid_symbol("") && !valid_symbol("ABCDEFGHIJK") && !valid_symbol("1A"));
    }
}
