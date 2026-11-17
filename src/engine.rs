//! The trading engine: startup checks, the plan, the poll loop, and swap
//! execution (paper or live).
//!
//! Live execution order, so a crash at any point is safe:
//!   1. quote from fresh on-chain reserves, check impact, funds and limits;
//!   2. build and sign the swap (its hash is known before it is sent);
//!   3. save it as `pending` (fsync) BEFORE broadcasting;
//!   4. broadcast, then poll the chain by hash until it is in a block or
//!      provably expired (re-broadcasting the same bytes is harmless);
//!   5. apply the fill to the grid and save.
//!
//! On restart, a saved `pending` swap is resolved on chain before anything else.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::clock::now_unix;
use crate::config::Config;
use crate::grid::{self, Holding, Side, Slot};
use crate::phantasma::keys::KeyPair;
use crate::phantasma::rpc::{Rpc, RpcError, TxReceipt};
use crate::saturn::{
    self, GAS_KCAL_RAW, POOLS_VERSION_PREFIX, Pool, Quote, Reserves, SOUL_FEE_RESERVE_RAW, SOUL_SELL_EXTRA_RAW,
    SWAP_VERSION_PREFIX, Token,
};
use crate::state::{Pending, STATE_VERSION, State, StateFile, Stats};
use crate::units::{PRICE_ONE, format_price, format_units_short, mul_div, parse_units};

/// How long a slot waits after a failed or refused swap before it may try again.
const SLOT_COOLDOWN: Duration = Duration::from_secs(60);
/// After the expiration, how long the chain is still watched before a swap counts as never executed.
const EXPIRY_GRACE_SECS: i64 = 60;
/// How often an unconfirmed swap is re-broadcast (same bytes, same hash).
const REBROADCAST_EVERY_SECS: i64 = 15;

pub fn connect(cfg: &Config) -> Result<Rpc, String> {
    Rpc::new(&cfg.rpc_url, cfg.network.chain(), &cfg.rpc_allowed_hosts, cfg.tls_roots)
}

/// The pool and its two tokens, oriented as base/quote.
pub struct Market {
    pub pool_id: u64,
    pub base: Token,
    pub quote: Token,
    pub base_is_a: bool,
    pub reserves_script: Vec<u8>,
}

impl Market {
    /// (base reserve, quote reserve), both scaled to 8 decimals.
    pub fn split(&self, r: &Reserves) -> (u128, u128) {
        if self.base_is_a { (r.a, r.b) } else { (r.b, r.a) }
    }

    /// Spot price, quote per base (fixed point).
    pub fn spot(&self, r: &Reserves) -> Option<u128> {
        let (rb, rq) = self.split(r);
        crate::units::price_from_reserves(rq, rb)
    }

    pub fn tokens(&self, side: Side) -> (&Token, &Token) {
        match side {
            Side::Buy => (&self.quote, &self.base),
            Side::Sell => (&self.base, &self.quote),
        }
    }

    pub fn quote(&self, r: &Reserves, side: Side, amount_in: u128) -> Result<Quote, String> {
        let (rb, rq) = self.split(r);
        match side {
            Side::Buy => saturn::quote_exact_in(rq, rb, r.fee_bps, self.quote.scale, self.base.scale, amount_in),
            Side::Sell => saturn::quote_exact_in(rb, rq, r.fee_bps, self.base.scale, self.quote.scale, amount_in),
        }
    }

    pub fn read_reserves(&self, rpc: &Rpc) -> Result<Reserves, String> {
        saturn::read_reserves(rpc, &self.reserves_script)
    }

    /// Execution price (quote per base, fixed point) of a fill.
    pub fn exec_price(&self, side: Side, amount_in: u128, amount_out: u128) -> Option<u128> {
        let (base_raw, quote_raw) = match side {
            Side::Buy => (amount_out, amount_in),
            Side::Sell => (amount_in, amount_out),
        };
        let q = self.quote.scale.up(quote_raw)?;
        let b = self.base.scale.up(base_raw)?;
        mul_div(q, PRICE_ONE, b)
    }

    pub fn fmt_base(&self, raw: u128) -> String {
        format!("{} {}", format_units_short(raw, self.base.decimals, 8), self.base.symbol)
    }

    pub fn fmt_quote(&self, raw: u128) -> String {
        format!("{} {}", format_units_short(raw, self.quote.decimals, 8), self.quote.symbol)
    }

    pub fn fmt_token(&self, t: &Token, raw: u128) -> String {
        format!("{} {}", format_units_short(raw, t.decimals, 8), t.symbol)
    }
}

/// Check the contracts, the clock and the pool; read both tokens.
pub fn preflight(rpc: &Rpc, cfg: &Config) -> Result<(Market, Pool), String> {
    let (swap_v, pools_v) = saturn::read_versions(rpc).map_err(|e| format!("cannot reach {}: {e}", rpc.url()))?;
    if !swap_v.starts_with(SWAP_VERSION_PREFIX) || !pools_v.starts_with(POOLS_VERSION_PREFIX) {
        return Err(format!(
            "Saturn's contracts are {swap_v:?} and {pools_v:?}; this bot was verified against {SWAP_VERSION_PREFIX}x and {POOLS_VERSION_PREFIX}x. \
             Refusing to trade against an interface it was not checked with."
        ));
    }
    if let Some(server) = rpc.last_server_time() {
        let skew = now_unix() - server;
        if skew.abs() > cfg.max_clock_skew_secs {
            return Err(format!(
                "this machine's clock is {skew:+} s off the node's. Swaps carry an expiry time, so sync the clock (NTP) first."
            ));
        }
    }
    let pool = saturn::read_pool(rpc, cfg.pool_id)?;
    let base_is_a = if pool.token_a == cfg.base && pool.token_b == cfg.quote {
        true
    } else if pool.token_b == cfg.base && pool.token_a == cfg.quote {
        false
    } else {
        return Err(format!("pool v4:{} trades {}/{}, not {}/{}", cfg.pool_id, pool.token_a, pool.token_b, cfg.base, cfg.quote));
    };
    let base = saturn::read_token(rpc, &cfg.base)?;
    let quote = saturn::read_token(rpc, &cfg.quote)?;
    info!(
        "contracts {swap_v} / {pools_v}; pool v4:{} {}/{} fee {} bps",
        cfg.pool_id, base.symbol, quote.symbol, pool.reserves.fee_bps
    );
    Ok((Market { pool_id: cfg.pool_id, base, quote, base_is_a, reserves_script: saturn::reserves_script(cfg.pool_id) }, pool))
}

/// The grid as it would start now, with its costs.
pub struct Plan {
    pub lines: Vec<u128>,
    pub slots: Vec<Slot>,
    pub spot: u128,
    pub quote_per_order: u128,
    pub need_quote: u128,
    pub need_base: u128,
    /// Estimated profit of one full buy-then-sell, per slot, in bps of the order (f64: an estimate only).
    pub round_trip_bps: Vec<f64>,
    pub buy_impact_bps: u32,
    pub problems: Vec<String>,
    pub warnings: Vec<String>,
}

/// Constant-product output (whole-token units; for estimates only).
fn sim_out(r_in: f64, r_out: f64, fee: f64, amount_in: f64) -> f64 {
    let net = amount_in * (1.0 - fee);
    net * r_out / (r_in + net)
}

pub fn make_plan(cfg: &Config, m: &Market, r: &Reserves) -> Result<Plan, String> {
    let spot = m.spot(r).ok_or("the pool is empty")?;
    let quote_per_order =
        parse_units(&cfg.quote_per_order, m.quote.decimals).map_err(|e| format!("grid.quote_per_order: {e}"))?;
    let lines = grid::grid_lines(cfg.lower, cfg.upper, cfg.levels, cfg.spacing)?;
    let slots = grid::init_slots(&lines, spot, quote_per_order, m.base.decimals, m.quote.decimals)?;
    let (need_quote, need_base) = grid::holdings(&slots);
    let mut problems = Vec::new();
    let mut warnings = Vec::new();

    // Price impact of one order now (the largest single buy).
    let buy_impact_bps = match m.quote(r, Side::Buy, quote_per_order) {
        Ok(q) => q.impact_bps,
        Err(e) => {
            problems.push(format!("an order of {} cannot be swapped: {e}", m.fmt_quote(quote_per_order)));
            10_000
        }
    };
    if buy_impact_bps > cfg.max_price_impact_bps {
        problems.push(format!(
            "one order moves the pool price {:.2}% (risk.max_price_impact_bps allows {:.2}%): use smaller orders or a deeper pool",
            f64::from(buy_impact_bps) / 100.0,
            f64::from(cfg.max_price_impact_bps) / 100.0
        ));
    }
    if quote_per_order < m.quote.min_swap_raw {
        problems.push(format!("grid.quote_per_order is below Saturn's minimum swap of {}", m.fmt_quote(m.quote.min_swap_raw)));
    }
    if let Some(s) = slots.iter().find(|s| s.holding == Holding::Base && s.amount < m.base.min_swap_raw) {
        problems.push(format!(
            "a sell of {} is below Saturn's minimum swap of {}",
            m.fmt_base(s.amount),
            m.fmt_base(m.base.min_swap_raw)
        ));
    }

    // Round trip per slot, simulated on this pool's depth at each line (constant product k).
    let (rb, rq) = m.split(r);
    let k = (rb as f64 / 1e8) * (rq as f64 / 1e8);
    let fee = f64::from(r.fee_bps) / 10_000.0;
    let q_in = quote_per_order as f64 / 10f64.powi(i32::from(m.quote.decimals));
    let round_trip_bps: Vec<f64> = slots
        .iter()
        .map(|s| {
            let (buy, sell) = (s.buy_at as f64 / 1e18, s.sell_at as f64 / 1e18);
            let base = sim_out((k * buy).sqrt(), (k / buy).sqrt(), fee, q_in);
            let back = sim_out((k / sell).sqrt(), (k * sell).sqrt(), fee, base);
            (back / q_in - 1.0) * 10_000.0
        })
        .collect();
    let worst = round_trip_bps.iter().copied().fold(f64::INFINITY, f64::min);
    if worst < cfg.min_profit_bps as f64 {
        problems.push(format!(
            "a round trip may earn only {:.1} bps after fees and impact (risk.min_profit_bps = {}): widen the range spacing, use fewer levels or smaller orders",
            worst, cfg.min_profit_bps
        ));
    }
    if spot < cfg.lower || spot > cfg.upper {
        warnings.push(format!(
            "the price {} is outside the grid {}..{}: the grid trades once the price comes back inside",
            format_price(spot),
            format_price(cfg.lower),
            format_price(cfg.upper)
        ));
    }
    if !r.active {
        warnings.push("the pool is not active right now: no swaps until it is".into());
    }
    Ok(Plan { lines, slots, spot, quote_per_order, need_quote, need_base, round_trip_bps, buy_impact_bps, problems, warnings })
}

/// What the wallet must hold for a swap of `amount_in` of `token_in` to be signed.
pub fn funds_needed(token_in: &str, amount_in: u128) -> Vec<(String, u128)> {
    let mut need: Vec<(String, u128)> = Vec::new();
    let mut add = |sym: &str, amt: u128| match need.iter_mut().find(|(s, _)| s == sym) {
        Some((_, a)) => *a = a.saturating_add(amt),
        None => need.push((sym.to_string(), amt)),
    };
    add(token_in, amount_in);
    add("KCAL", GAS_KCAL_RAW);
    add("SOUL", SOUL_FEE_RESERVE_RAW);
    if token_in == "SOUL" {
        add("SOUL", SOUL_SELL_EXTRA_RAW);
    }
    need
}

/// The shortfalls of `have` against `need`, as text ("" when funded).
pub fn shortfalls(
    rpc: &Rpc,
    kp: &KeyPair,
    need: &[(String, u128)],
    decimals_of: impl Fn(&str) -> u8,
) -> Result<Vec<String>, String> {
    let bals = rpc.balances(kp.address()).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (sym, amt) in need {
        let have = bals.iter().find(|b| &b.symbol == sym).map_or(0, |b| b.amount);
        if have < *amt {
            let d = decimals_of(sym);
            out.push(format!(
                "{sym}: has {}, needs {} (missing {})",
                format_units_short(have, d, 8),
                format_units_short(*amt, d, 8),
                format_units_short(amt - have, d, 8)
            ));
        }
    }
    Ok(out)
}

pub fn decimals_for(m: &Market) -> impl Fn(&str) -> u8 + '_ {
    move |sym: &str| match sym {
        s if s == m.base.symbol => m.base.decimals,
        s if s == m.quote.symbol => m.quote.decimals,
        "KCAL" => 10,
        "SOUL" => 8,
        _ => 8,
    }
}

/// The outcome of one broadcast swap.
pub enum Outcome {
    Filled {
        amount_out: u128,
        fee_kcal: u128,
        exact: bool,
    },
    Failed(String),
    /// Not in any block after its expiry: it never executed.
    Expired,
}

/// Broadcast (if needed) and wait for `p` to be decided on chain.
///
/// Never guesses: a swap counts as not executed only when the node answers
/// "not found" after its expiry plus a grace period. While the node cannot be
/// read, this keeps waiting (the grid must not drift from what is on chain).
pub fn confirm(
    rpc: &Rpc,
    cfg: &Config,
    kp_addr: &crate::phantasma::keys::Address,
    m: &Market,
    p: &Pending,
    already_sent: bool,
) -> Outcome {
    let mut next_broadcast = if already_sent { now_unix() + REBROADCAST_EVERY_SECS } else { 0 };
    let mut last_warn: Option<Instant> = None;
    let mut warn_every = |msg: String| {
        if last_warn.is_none_or(|t| t.elapsed() >= Duration::from_secs(15)) {
            warn!("{msg}");
            last_warn = Some(Instant::now());
        }
    };
    loop {
        let now = now_unix();
        if now >= next_broadcast && now < i64::from(p.expiration) - 3 {
            match rpc.send_raw_transaction(&p.signed_hex) {
                Ok(h) if h != p.hash => warn!("the node named the swap {h}, expected {}", p.hash),
                Ok(_) => {}
                // Refused the first time: it was never accepted, so it can never execute.
                Err(RpcError::Node(msg)) if next_broadcast == 0 => return Outcome::Failed(msg),
                Err(e) => warn_every(format!("broadcast of {} not confirmed ({e}); watching the chain", short(&p.hash))),
            }
            next_broadcast = now + REBROADCAST_EVERY_SECS;
        }
        sleep(Duration::from_millis(cfg.confirm_poll_ms));
        match rpc.transaction(&p.hash) {
            Ok(Some(r)) => return decide(&r, kp_addr, m, p),
            Ok(None) if now_unix() > i64::from(p.expiration) + EXPIRY_GRACE_SECS => return Outcome::Expired,
            Ok(None) => {}
            Err(e) => warn_every(format!("cannot read swap {} yet ({e}); still waiting for a definite answer", short(&p.hash))),
        }
    }
}

fn decide(r: &TxReceipt, kp_addr: &crate::phantasma::keys::Address, m: &Market, p: &Pending) -> Outcome {
    if !r.succeeded() {
        let why = if r.debug_comment.is_empty() { r.state.clone() } else { format!("{}: {}", r.state, r.debug_comment) };
        return Outcome::Failed(why);
    }
    let (tin, tout) = m.tokens(p.side);
    let fee_kcal = r.fee.parse::<u128>().unwrap_or(0);
    match saturn::swap_amount_out(r, kp_addr, m.pool_id, &tin.symbol, &tout.symbol, p.amount_in) {
        Some(out) => Outcome::Filled { amount_out: out, fee_kcal, exact: true },
        // It ran (Halt) but its payout could not be read: count the guaranteed minimum.
        None => Outcome::Filled { amount_out: p.min_out, fee_kcal, exact: false },
    }
}

fn short(h: &str) -> &str {
    h.get(..12).unwrap_or(h)
}

/// The running bot.
pub struct Engine {
    cfg: Config,
    rpc: Rpc,
    market: Market,
    key: Option<KeyPair>,
    store: StateFile,
    state: State,
    quote_per_order: u128,
    cooldown: HashMap<usize, Instant>,
    trades: VecDeque<i64>,
    failures: u32,
    last_note: HashMap<&'static str, Instant>,
    stop_file: std::path::PathBuf,
}

impl Engine {
    /// Paper mode when `key` is `None`; live (signs and sends) otherwise.
    pub fn start(cfg: Config, key: Option<KeyPair>) -> Result<Self, String> {
        let live = key.is_some();
        let rpc = connect(&cfg)?;
        let (market, pool) = preflight(&rpc, &cfg)?;
        let path = if live { &cfg.state_file } else { &cfg.paper_state_file };
        let store = StateFile::open(path)?;
        let stop_file = path.parent().unwrap_or(Path::new(".")).join("STOP");
        let plan = make_plan(&cfg, &market, &pool.reserves)?;
        let state = match store.load()? {
            Some(s) => {
                if s.fingerprint != cfg.fingerprint()
                    || s.paper == live
                    || s.pool_id != cfg.pool_id
                    || s.base != cfg.base
                    || s.quote != cfg.quote
                {
                    return Err(format!(
                        "{} was made for other grid settings. Restore those settings, or move the file away to start a new grid \
                         (a live grid's slots may hold tokens: check `status` first).",
                        store.path().display()
                    ));
                }
                info!(
                    "resuming {} ({} slots, {} round trips so far)",
                    store.path().display(),
                    s.slots.len(),
                    s.stats.round_trips
                );
                s
            }
            None => {
                if !plan.problems.is_empty() {
                    return Err(format!("the plan has problems (run `saturn-grid plan`):\n  - {}", plan.problems.join("\n  - ")));
                }
                for w in &plan.warnings {
                    warn!("{w}");
                }
                if let Some(kp) = &key {
                    let mut need = vec![
                        (market.quote.symbol.clone(), plan.need_quote),
                        (market.base.symbol.clone(), plan.need_base),
                        ("KCAL".to_string(), GAS_KCAL_RAW),
                        ("SOUL".to_string(), SOUL_FEE_RESERVE_RAW),
                    ];
                    merge(&mut need);
                    let short = shortfalls(&rpc, kp, &need, decimals_for(&market))?;
                    if !short.is_empty() {
                        let msg = format!("the wallet {} cannot fund this grid:\n  - {}", kp.address(), short.join("\n  - "));
                        if cfg.allow_unfunded_start {
                            warn!("{msg}\n(risk.allow_unfunded_start = true: starting anyway; unfunded swaps will wait)");
                        } else {
                            return Err(msg);
                        }
                    }
                }
                let s = State {
                    version: STATE_VERSION,
                    paper: !live,
                    network: cfg.network.name().to_string(),
                    pool_id: cfg.pool_id,
                    base: cfg.base.clone(),
                    quote: cfg.quote.clone(),
                    base_decimals: market.base.decimals,
                    quote_decimals: market.quote.decimals,
                    fingerprint: cfg.fingerprint(),
                    created_at: now_unix(),
                    slots: plan.slots.clone(),
                    pending: None,
                    stats: Stats::default(),
                };
                store.save(&s)?;
                info!("new grid saved to {}", store.path().display());
                s
            }
        };
        if state.base_decimals != market.base.decimals || state.quote_decimals != market.quote.decimals {
            return Err("a token's decimals changed on chain since this grid started; refusing to continue".into());
        }
        Ok(Engine {
            quote_per_order: plan.quote_per_order,
            cfg,
            rpc,
            market,
            key,
            store,
            state,
            cooldown: HashMap::new(),
            trades: VecDeque::new(),
            failures: 0,
            last_note: HashMap::new(),
            stop_file,
        })
    }

    fn live(&self) -> bool {
        self.key.is_some()
    }

    /// Log `msg` at most once a minute per `topic` (keeps the log readable while waiting).
    fn note(&mut self, topic: &'static str, msg: String) {
        let due = self.last_note.get(topic).is_none_or(|t| t.elapsed() >= Duration::from_secs(60));
        if due {
            warn!("{msg}");
            self.last_note.insert(topic, Instant::now());
        }
    }

    fn save(&self) -> Result<(), String> {
        self.store.save(&self.state)
    }

    /// Resolve a swap left in flight by an earlier run.
    fn resolve_pending(&mut self) -> Result<(), String> {
        let Some(p) = self.state.pending.clone() else { return Ok(()) };
        let Some(kp) = &self.key else {
            self.state.pending = None;
            return self.save();
        };
        info!("resolving swap {} left from the last run…", p.hash);
        let addr = *kp.address();
        let outcome = confirm(&self.rpc, &self.cfg, &addr, &self.market, &p, true);
        self.apply_outcome(&p, outcome)
    }

    /// Run until stopped (STOP file, too many failures, or an error).
    pub fn run(&mut self) -> Result<(), String> {
        let mode = if self.live() { "LIVE" } else { "PAPER" };
        info!(
            "{mode} grid on {} pool v4:{} {}/{}: {} levels {}..{}, {} per order",
            self.cfg.network.name(),
            self.market.pool_id,
            self.market.base.symbol,
            self.market.quote.symbol,
            self.cfg.levels,
            format_price(self.cfg.lower),
            format_price(self.cfg.upper),
            self.market.fmt_quote(self.quote_per_order)
        );
        if let Some(kp) = &self.key {
            info!("wallet {}", kp.address());
        }
        info!("to stop cleanly, create the file {}", self.stop_file.display());
        self.resolve_pending()?;
        let mut backoff = Duration::from_millis(self.cfg.poll_ms);
        let mut last_spot = 0u128;
        loop {
            if self.stop_file.exists() {
                info!("{} found: stopping", self.stop_file.display());
                return Ok(());
            }
            let r = match self.market.read_reserves(&self.rpc) {
                Ok(r) => {
                    backoff = Duration::from_millis(self.cfg.poll_ms);
                    r
                }
                Err(e) => {
                    warn!("reading the pool: {e} (retrying in {} ms)", backoff.as_millis());
                    sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            };
            if !r.active {
                self.note("inactive", "the pool is not active: waiting".into());
                sleep(Duration::from_millis(self.cfg.poll_ms));
                continue;
            }
            let Some(spot) = self.market.spot(&r) else {
                self.note("empty", "the pool is empty: waiting".into());
                sleep(Duration::from_millis(self.cfg.poll_ms));
                continue;
            };
            if spot != last_spot {
                info!("price {} {}/{}", format_price(spot), self.market.quote.symbol, self.market.base.symbol);
                last_spot = spot;
            }
            if self.outside_stop_band(spot) {
                self.note("band", format!("price {} is far outside the grid: paused until it comes back", format_price(spot)));
                sleep(Duration::from_millis(self.cfg.poll_ms));
                continue;
            }
            let cooling = |i: usize| self.cooldown.get(&i).is_some_and(|t| t.elapsed() < SLOT_COOLDOWN);
            let Some((slot, side)) = grid::next_action(&self.state.slots, spot, cooling) else {
                sleep(Duration::from_millis(self.cfg.poll_ms));
                continue;
            };
            let hour_ago = now_unix() - 3600;
            while self.trades.front().is_some_and(|&t| t < hour_ago) {
                self.trades.pop_front();
            }
            if self.trades.len() >= self.cfg.max_trades_per_hour as usize {
                self.note("rate", format!("{} trades in the last hour (risk.max_trades_per_hour): waiting", self.trades.len()));
                sleep(Duration::from_millis(self.cfg.poll_ms));
                continue;
            }
            self.execute(slot, side, &r, spot)?;
            if self.failures >= self.cfg.max_consecutive_failures {
                return Err(format!("{} swaps in a row failed (risk.max_consecutive_failures): stopping", self.failures));
            }
            // No sleep after a trade: re-read the pool at once.
        }
    }

    fn outside_stop_band(&self, spot: u128) -> bool {
        if self.cfg.stop_outside_range_bps == 0 {
            return false;
        }
        let b = u128::from(self.cfg.stop_outside_range_bps);
        let floor = mul_div(self.cfg.lower, 10_000u128.saturating_sub(b.min(10_000)), 10_000).unwrap_or(0);
        let ceil = mul_div(self.cfg.upper, 10_000 + b, 10_000).unwrap_or(u128::MAX);
        spot < floor || spot > ceil
    }

    fn execute(&mut self, i: usize, side: Side, r: &Reserves, spot: u128) -> Result<(), String> {
        let slot = self.state.slots[i].clone();
        let (tin, tout) = self.market.tokens(side);
        let (tin, tout) = (tin.clone(), tout.clone());
        let line = if side == Side::Buy { slot.buy_at } else { slot.sell_at };
        if slot.amount < tin.min_swap_raw {
            self.cooldown.insert(i, Instant::now());
            self.note(
                "dust",
                format!("slot {i}: {} is below Saturn's minimum swap; skipping it", self.market.fmt_token(&tin, slot.amount)),
            );
            return Ok(());
        }
        let q = match self.market.quote(r, side, slot.amount) {
            Ok(q) => q,
            Err(e) => {
                self.cooldown.insert(i, Instant::now());
                warn!("slot {i}: cannot quote {} {}: {e}", side.word(), self.market.fmt_token(&tin, slot.amount));
                return Ok(());
            }
        };
        if q.impact_bps > self.cfg.max_price_impact_bps {
            self.cooldown.insert(i, Instant::now());
            warn!(
                "slot {i}: {} of {} would move the price {:.2}% (limit {:.2}%): skipped for now",
                side.word(),
                self.market.fmt_token(&tin, slot.amount),
                f64::from(q.impact_bps) / 100.0,
                f64::from(self.cfg.max_price_impact_bps) / 100.0
            );
            return Ok(());
        }
        let min_out = mul_div(q.amount_out, u128::from(10_000 - self.cfg.slippage_bps), 10_000).unwrap_or(0).max(1);
        let exec = self.market.exec_price(side, q.amount_in, q.amount_out).unwrap_or(0);
        info!(
            "slot {i}: price {} crossed the {} line {}: {} {} for ~{} (exec ~{}, impact {:.2}%)",
            format_price(spot),
            side.word(),
            format_price(line),
            side.word(),
            self.market.fmt_token(&tin, q.amount_in),
            self.market.fmt_token(&tout, q.amount_out),
            format_price(exec),
            f64::from(q.impact_bps) / 100.0
        );

        if !self.live() {
            self.fill(i, side, q.amount_in, q.amount_out, 0, None);
            self.trades.push_back(now_unix());
            return self.save();
        }

        let need = funds_needed(&tin.symbol, q.amount_in);
        let kp = self.key.as_ref().expect("live");
        match shortfalls(&self.rpc, kp, &need, decimals_for(&self.market)) {
            Ok(s) if s.is_empty() => {}
            Ok(s) => {
                self.cooldown.insert(i, Instant::now());
                self.note("funds", format!("slot {i}: not enough funds to {}: {}", side.word(), s.join("; ")));
                return Ok(());
            }
            Err(e) => {
                warn!("cannot read balances: {e}");
                return Ok(());
            }
        }
        let expiration = u32::try_from(now_unix() + i64::from(self.cfg.tx_ttl_secs)).map_err(|_| "clock out of range")?;
        let tx = saturn::swap_tx(
            self.cfg.network,
            kp.address(),
            self.market.pool_id,
            &tin.symbol,
            &tout.symbol,
            q.amount_in,
            min_out,
            expiration,
        );
        let signed = tx.sign(kp);
        let pending = Pending {
            hash: signed.hash.clone(),
            slot: i,
            side,
            amount_in: q.amount_in,
            min_out,
            expiration,
            signed_hex: signed.hex,
            created_at: now_unix(),
        };
        // Journal first: after a crash the swap is found by its hash, never sent twice.
        self.state.pending = Some(pending.clone());
        self.save()?;
        info!("slot {i}: sending {} {} ({})", side.word(), short(&pending.hash), self.cfg.network.explorer_tx(&pending.hash));
        let addr = *kp.address();
        let outcome = confirm(&self.rpc, &self.cfg, &addr, &self.market, &pending, false);
        self.trades.push_back(now_unix());
        self.apply_outcome(&pending, outcome)
    }

    fn apply_outcome(&mut self, p: &Pending, outcome: Outcome) -> Result<(), String> {
        match outcome {
            Outcome::Filled { amount_out, fee_kcal, exact } => {
                if !exact {
                    warn!("swap {} ran but its payout was unreadable: counting the guaranteed minimum", short(&p.hash));
                }
                self.fill(p.slot, p.side, p.amount_in, amount_out, fee_kcal, Some(&p.hash));
                self.failures = 0;
            }
            Outcome::Failed(why) => {
                self.state.stats.failed += 1;
                self.failures += 1;
                self.cooldown.insert(p.slot, Instant::now());
                error!("swap {} failed: {why}", short(&p.hash));
            }
            Outcome::Expired => {
                self.state.stats.failed += 1;
                self.failures += 1;
                self.cooldown.insert(p.slot, Instant::now());
                warn!("swap {} expired without executing", short(&p.hash));
            }
        }
        self.state.pending = None;
        self.save()
    }

    /// Apply a fill to slot `i` and the stats.
    fn fill(&mut self, i: usize, side: Side, amount_in: u128, amount_out: u128, fee_kcal: u128, hash: Option<&str>) {
        let exec = self.market.exec_price(side, amount_in, amount_out).unwrap_or(0);
        let qpo = self.quote_per_order;
        let compound = self.cfg.compound;
        let slot = &mut self.state.slots[i];
        let tag = if hash.is_some() { "" } else { "[paper] " };
        match side {
            Side::Buy => {
                grid::apply_buy(slot, amount_in, amount_out);
                self.state.stats.buys += 1;
                trade!(
                    "{tag}slot {i} BOUGHT {} for {} at {} (sells at {})",
                    self.market.fmt_base(amount_out),
                    self.market.fmt_quote(amount_in),
                    format_price(exec),
                    format_price(slot.sell_at)
                );
            }
            Side::Sell => {
                let had_cost = slot.cost_quote;
                let (pnl, banked) = grid::apply_sell(slot, amount_out, compound, qpo);
                self.state.stats.sells += 1;
                self.state.stats.round_trips += 1;
                self.state.stats.realized_pnl_quote += pnl;
                self.state.stats.banked_quote += banked;
                let sign = if pnl < 0 { "-" } else { "+" };
                trade!(
                    "{tag}slot {i} SOLD {} for {} at {} (cost {}, {sign}{}; buys again at {})",
                    self.market.fmt_base(amount_in),
                    self.market.fmt_quote(amount_out),
                    format_price(exec),
                    self.market.fmt_quote(had_cost),
                    self.market.fmt_quote(pnl.unsigned_abs()),
                    format_price(slot.buy_at)
                );
            }
        }
        self.state.stats.gas_kcal += fee_kcal;
        self.state.stats.last_trade_at = now_unix();
        let total = self.state.stats.realized_pnl_quote;
        let sign = if total < 0 { "-" } else { "" };
        info!(
            "realized {sign}{} over {} round trips; slots hold {}",
            self.market.fmt_quote(total.unsigned_abs()),
            self.state.stats.round_trips,
            holdings_text(&self.market, &self.state.slots)
        );
    }
}

fn merge(need: &mut Vec<(String, u128)>) {
    let mut out: Vec<(String, u128)> = Vec::new();
    for (s, a) in need.drain(..) {
        match out.iter_mut().find(|(x, _)| *x == s) {
            Some((_, b)) => *b = b.saturating_add(a),
            None => out.push((s, a)),
        }
    }
    *need = out;
}

pub fn holdings_text(m: &Market, slots: &[Slot]) -> String {
    let (q, b) = grid::holdings(slots);
    format!("{} + {}", m.fmt_quote(q), m.fmt_base(b))
}
