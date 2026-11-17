//! saturn-grid: a grid trading bot for Saturn DEX v4 pools on Phantasma.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use saturn_grid::config::Config;
use saturn_grid::engine::{self, Engine, Outcome, funds_needed, shortfalls};
use saturn_grid::grid::{Holding, Side};
use saturn_grid::saturn::{self, Network};
use saturn_grid::state::Pending;
use saturn_grid::units::{format_price, format_units, format_units_short, mul_div, parse_units};
use saturn_grid::{api, clock, info, keyfile, state};

const USAGE: &str = "\
saturn-grid: a grid trading bot for Saturn DEX (v4 pools) on Phantasma

USAGE: saturn-grid [--config FILE] <COMMAND> [OPTIONS]

COMMANDS
  plan                         Read the pool on chain and show the grid, the funds it
                               needs and the estimated profit per round trip
  run                          Paper-trade the grid on live prices (no keys, no swaps)
  run --live                   Trade for real: sign and send swaps from the key file
  status [--paper]             Show the saved grid and its results
  balances                     Show the wallet's balances
  quote --sell SYM --amount N  Quote a swap in the configured pool
  swap --sell SYM --amount N   Make one swap in the configured pool (asks first)
  keygen [--out FILE]          Create a new key file (mode 0600) and print its address
  address                      Print the key file's address
  pools --pair A,B [--network mainnet|devnet]
                               List Saturn v4 pools for a pair (from Saturn's API)
  version | help

OPTIONS
  --config FILE   Config file (default: saturn-grid.toml)
  --yes           Skip the confirmation question (swap; devnet only unless
                  SATURN_GRID_ALLOW_MAINNET=1)

Stop a running bot cleanly by creating the file STOP next to its state file.
Keys are read only from the key file, never from arguments or the environment.";

struct Args {
    cmd: String,
    config: PathBuf,
    live: bool,
    paper: bool,
    yes: bool,
    sell: Option<String>,
    amount: Option<String>,
    out: Option<PathBuf>,
    pair: Option<String>,
    network: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Refuse anything that looks like a private key on the command line (shell history, ps).
    if argv.iter().any(|a| looks_like_wif(a)) {
        return Err("an argument looks like a private key. Keys are read only from the key file; remove it (and clear your shell history).".into());
    }
    let mut a = Args {
        cmd: String::new(),
        config: PathBuf::from("saturn-grid.toml"),
        live: false,
        paper: false,
        yes: false,
        sell: None,
        amount: None,
        out: None,
        pair: None,
        network: None,
    };
    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--config" | "-c" => a.config = PathBuf::from(val("--config")?),
            "--live" => a.live = true,
            "--paper" => a.paper = true,
            "--yes" | "-y" => a.yes = true,
            "--sell" => a.sell = Some(val("--sell")?.to_ascii_uppercase()),
            "--amount" => a.amount = Some(val("--amount")?),
            "--out" => a.out = Some(PathBuf::from(val("--out")?)),
            "--pair" => a.pair = Some(val("--pair")?.to_ascii_uppercase()),
            "--network" => a.network = Some(val("--network")?),
            "-h" | "--help" => a.cmd = "help".into(),
            "-V" | "--version" => a.cmd = "version".into(),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s if a.cmd.is_empty() => a.cmd = s.to_string(),
            s => return Err(format!("unexpected argument {s}")),
        }
    }
    if a.cmd.is_empty() {
        a.cmd = "help".into();
    }
    Ok(a)
}

fn looks_like_wif(s: &str) -> bool {
    let b58 = |c: char| c.is_ascii_alphanumeric() && !"0OIl".contains(c);
    s.split(|c: char| !b58(c))
        .any(|w| (w.len() == 52 && (w.starts_with('K') || w.starts_with('L'))) || (w.len() == 51 && w.starts_with('5')))
}

/// Ask for an explicit "yes" (at a terminal), or, without a terminal, require
/// SATURN_GRID_ALLOW_MAINNET=1 for mainnet.
fn confirm_or_refuse(what: &str, network: Network, yes_flag: bool) -> Result<(), String> {
    let env_ok = std::env::var("SATURN_GRID_ALLOW_MAINNET").is_ok_and(|v| v == "1");
    if std::io::stdin().is_terminal() && !(yes_flag && network == Network::Devnet) {
        eprint!("{what}\nType yes to continue: ");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
        return if line.trim() == "yes" { Ok(()) } else { Err("not confirmed: nothing was sent".into()) };
    }
    match network {
        Network::Devnet if yes_flag || !std::io::stdin().is_terminal() => Ok(()),
        Network::Mainnet if env_ok => Ok(()),
        Network::Mainnet => Err("mainnet without a terminal needs SATURN_GRID_ALLOW_MAINNET=1 in the environment".into()),
        Network::Devnet => Err("not confirmed".into()),
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            saturn_grid::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(a: &Args) -> Result<(), String> {
    match a.cmd.as_str() {
        "help" => {
            println!("{USAGE}");
            Ok(())
        }
        "version" => {
            println!("saturn-grid {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "keygen" => cmd_keygen(a),
        "pools" => cmd_pools(a),
        "address" => {
            let cfg = Config::load(&a.config)?;
            println!("{}", keyfile::load(&cfg.key_file)?.address());
            Ok(())
        }
        "plan" => cmd_plan(&Config::load(&a.config)?),
        "run" => cmd_run(a, Config::load(&a.config)?),
        "status" => cmd_status(a, &Config::load(&a.config)?),
        "balances" => cmd_balances(&Config::load(&a.config)?),
        "quote" => cmd_quote(a, &Config::load(&a.config)?, false),
        "swap" => cmd_quote(a, &Config::load(&a.config)?, true),
        other => Err(format!("unknown command {other:?} (try: saturn-grid help)")),
    }
}

fn cmd_keygen(a: &Args) -> Result<(), String> {
    let path = match &a.out {
        Some(p) => p.clone(),
        None => match Config::load(&a.config) {
            Ok(c) => c.key_file,
            Err(_) => PathBuf::from("keys/wallet.json"),
        },
    };
    let kp = keyfile::create(&path)?;
    println!("new key file: {} (mode 0600; back it up, never share it)", path.display());
    println!("address:      {}", kp.address());
    Ok(())
}

fn cmd_pools(a: &Args) -> Result<(), String> {
    let pair = a.pair.as_deref().ok_or("pools needs --pair A,B")?;
    let (x, y) = pair.split_once(',').ok_or("--pair takes two symbols: --pair RA,TAZ")?;
    let (network, roots) = match Config::load(&a.config) {
        Ok(c) => (a.network.as_deref().map(Network::parse).transpose()?.unwrap_or(c.network), c.tls_roots),
        Err(_) => (Network::parse(a.network.as_deref().unwrap_or("mainnet"))?, saturn_grid::phantasma::rpc::TlsRoots::WebPki),
    };
    let rows = api::pools_for_pair(network, x.trim(), y.trim(), roots)?;
    if rows.is_empty() {
        println!("no active v4 pool for {x}/{y} on {}", network.name());
        return Ok(());
    }
    println!("{:<10} {:<16} {:>8} {:>14} {:>7}", "pool", "pair", "fee", "TVL (USD)", "burned");
    for r in rows {
        let tvl = r.tvl_usd.map_or("?".to_string(), |t| format!("{t:.2}"));
        println!(
            "{:<10} {:<16} {:>7.2}% {:>14} {:>7}",
            r.id,
            format!("{}/{}", r.token_a, r.token_b),
            r.fee_bps as f64 / 100.0,
            tvl,
            if r.burned { "yes" } else { "" }
        );
    }
    println!("(market data from Saturn's API; the bot itself reads the pool on chain)");
    Ok(())
}

fn cmd_plan(cfg: &Config) -> Result<(), String> {
    let rpc = engine::connect(cfg)?;
    let (m, pool) = engine::preflight(&rpc, cfg)?;
    let r = pool.reserves;
    let plan = engine::make_plan(cfg, &m, &r)?;
    let (rb, rq) = m.split(&r);
    println!("network     {}  (RPC {})", cfg.network.name(), rpc.url());
    println!(
        "pool        v4:{}  {}/{}  fee {:.2}%  {}",
        m.pool_id,
        m.base.symbol,
        m.quote.symbol,
        f64::from(r.fee_bps) / 100.0,
        if r.active { "active" } else { "NOT ACTIVE" }
    );
    println!(
        "reserves    {} {} / {} {}",
        format_units_short(rb, 8, 4),
        m.base.symbol,
        format_units_short(rq, 8, 4),
        m.quote.symbol
    );
    println!("price       {} {} per {}", format_price(plan.spot), m.quote.symbol, m.base.symbol);
    println!(
        "grid        {} levels, {:?}, {}..{}, {} per order{}",
        cfg.levels,
        cfg.spacing,
        format_price(cfg.lower),
        format_price(cfg.upper),
        m.fmt_quote(plan.quote_per_order),
        if cfg.compound { ", compounding" } else { "" }
    );
    println!();
    println!("  {:>4}  {:>16}  {:>16}  {:<6} {:>24}  {:>10}", "slot", "buy at", "sell at", "holds", "amount", "round trip");
    for (i, s) in plan.slots.iter().enumerate().rev() {
        let (holds, amt) = match s.holding {
            Holding::Quote => ("quote", m.fmt_quote(s.amount)),
            Holding::Base => ("base", m.fmt_base(s.amount)),
        };
        let mark = if s.buy_at < plan.spot && plan.spot < s.sell_at { " <- price" } else { "" };
        println!(
            "  {:>4}  {:>16}  {:>16}  {:<6} {:>24}  {:>8.1}bp{mark}",
            i,
            format_price(s.buy_at),
            format_price(s.sell_at),
            holds,
            amt,
            plan.round_trip_bps[i]
        );
    }
    println!();
    println!(
        "one order moves the price ~{:.2}% (limit {:.2}%)",
        f64::from(plan.buy_impact_bps) / 100.0,
        f64::from(cfg.max_price_impact_bps) / 100.0
    );
    println!(
        "funds for the grid: {} and {}, plus at least {} KCAL for gas (~0.05 KCAL used per swap) and 1 SOUL free for Phantasma's data fee",
        m.fmt_quote(plan.need_quote),
        m.fmt_base(plan.need_base),
        format_units(saturn::GAS_KCAL_RAW, 10)
    );
    for w in &plan.warnings {
        println!("WARNING: {w}");
    }
    for p in &plan.problems {
        println!("PROBLEM: {p}");
    }
    if plan.problems.is_empty() {
        println!("plan OK. Next: `saturn-grid run` (paper), then `saturn-grid run --live`.");
        Ok(())
    } else {
        Err("fix the problems above before running this grid".into())
    }
}

fn cmd_run(a: &Args, cfg: Config) -> Result<(), String> {
    let key = if a.live {
        let kp = keyfile::load(&cfg.key_file)?;
        confirm_or_refuse(
            &format!(
                "LIVE grid on {} pool v4:{} ({}/{}) from wallet {}. Swaps will be signed and sent.",
                cfg.network.name().to_uppercase(),
                cfg.pool_id,
                cfg.base,
                cfg.quote,
                kp.address()
            ),
            cfg.network,
            false,
        )?;
        Some(kp)
    } else {
        info!("PAPER mode: live prices, simulated fills, nothing is signed or sent (add --live to trade)");
        None
    };
    let mut engine = Engine::start(cfg, key)?;
    engine.run()
}

fn cmd_status(a: &Args, cfg: &Config) -> Result<(), String> {
    let path = if a.paper { &cfg.paper_state_file } else { &cfg.state_file };
    let Some(s) = state::load(path)? else {
        println!("no grid saved at {}", path.display());
        return Ok(());
    };
    let (bd, qd) = (s.base_decimals, s.quote_decimals);
    println!(
        "{} grid, {} v4:{} {}/{}, started {}",
        if s.paper { "PAPER" } else { "LIVE" },
        s.network,
        s.pool_id,
        s.base,
        s.quote,
        clock::fmt_utc(s.created_at)
    );
    println!("  {:>4}  {:>16}  {:>16}  {:<6} {:>22}  {:>6}", "slot", "buy at", "sell at", "holds", "amount", "trips");
    for (i, sl) in s.slots.iter().enumerate().rev() {
        let (holds, amt) = match sl.holding {
            Holding::Quote => ("quote", format!("{} {}", format_units_short(sl.amount, qd, 8), s.quote)),
            Holding::Base => ("base", format!("{} {}", format_units_short(sl.amount, bd, 8), s.base)),
        };
        println!(
            "  {:>4}  {:>16}  {:>16}  {:<6} {:>22}  {:>6}",
            i,
            format_price(sl.buy_at),
            format_price(sl.sell_at),
            holds,
            amt,
            sl.round_trips
        );
    }
    let st = &s.stats;
    let pnl = st.realized_pnl_quote;
    println!(
        "buys {}  sells {}  round trips {}  failed {}  realized {}{} {}  banked {} {}  gas {} KCAL",
        st.buys,
        st.sells,
        st.round_trips,
        st.failed,
        if pnl < 0 { "-" } else { "" },
        format_units_short(pnl.unsigned_abs(), qd, 8),
        s.quote,
        format_units_short(st.banked_quote, qd, 8),
        s.quote,
        format_units_short(st.gas_kcal, 10, 6)
    );
    if st.last_trade_at > 0 {
        println!("last trade {}", clock::fmt_utc(st.last_trade_at));
    }
    if let Some(p) = &s.pending {
        println!("IN FLIGHT: swap {} (slot {}, {}), resolved on the next start", p.hash, p.slot, p.side.word());
    }
    Ok(())
}

fn cmd_balances(cfg: &Config) -> Result<(), String> {
    let kp = keyfile::load(&cfg.key_file)?;
    let rpc = engine::connect(cfg)?;
    let bals = rpc.balances(kp.address()).map_err(|e| e.to_string())?;
    println!("{} on {}", kp.address(), cfg.network.name());
    if bals.is_empty() {
        println!("  (no liquid balances)");
    }
    for b in bals {
        println!("  {:>28} {}", format_units(b.amount, b.decimals), b.symbol);
    }
    Ok(())
}

/// `quote` (read-only) and `swap` (one live swap, confirmed first).
fn cmd_quote(a: &Args, cfg: &Config, send: bool) -> Result<(), String> {
    let sell = a.sell.as_deref().ok_or("needs --sell SYMBOL")?;
    let amount = a.amount.as_deref().ok_or("needs --amount N")?;
    let rpc = engine::connect(cfg)?;
    let (m, pool) = engine::preflight(&rpc, cfg)?;
    let side = if sell == m.base.symbol {
        Side::Sell
    } else if sell == m.quote.symbol {
        Side::Buy
    } else {
        return Err(format!("--sell must be {} or {}", m.base.symbol, m.quote.symbol));
    };
    let (tin, tout) = m.tokens(side);
    let amount_in = parse_units(amount, tin.decimals).map_err(|e| format!("--amount: {e}"))?;
    let r = pool.reserves;
    let q = m.quote(&r, side, amount_in)?;
    let min_out = mul_div(q.amount_out, u128::from(10_000 - cfg.slippage_bps), 10_000).unwrap_or(0).max(1);
    let spot = m.spot(&r).unwrap_or(0);
    let exec = m.exec_price(side, q.amount_in, q.amount_out).unwrap_or(0);
    println!(
        "pool v4:{} {}/{}  price {}  fee {:.2}%",
        m.pool_id,
        m.base.symbol,
        m.quote.symbol,
        format_price(spot),
        f64::from(r.fee_bps) / 100.0
    );
    println!("sell     {}", m.fmt_token(tin, q.amount_in));
    println!(
        "receive  {}  (at least {} with {:.2}% slippage)",
        m.fmt_token(tout, q.amount_out),
        m.fmt_token(tout, min_out),
        f64::from(cfg.slippage_bps) / 100.0
    );
    println!(
        "price    {} {} per {} (impact {:.2}%)",
        format_price(exec),
        m.quote.symbol,
        m.base.symbol,
        f64::from(q.impact_bps) / 100.0
    );
    if q.amount_in < tin.min_swap_raw {
        return Err(format!("below Saturn's minimum swap of {}", m.fmt_token(tin, tin.min_swap_raw)));
    }
    if !send {
        return Ok(());
    }
    if !r.active {
        return Err("the pool is not active".into());
    }
    let kp = keyfile::load(&cfg.key_file)?;
    let short = shortfalls(&rpc, &kp, &funds_needed(&tin.symbol, amount_in), engine::decimals_for(&m))?;
    if !short.is_empty() {
        return Err(format!("not enough funds:\n  - {}", short.join("\n  - ")));
    }
    confirm_or_refuse(
        &format!(
            "Swap {} for at least {} on {} from {}?",
            m.fmt_token(tin, amount_in),
            m.fmt_token(tout, min_out),
            cfg.network.name().to_uppercase(),
            kp.address()
        ),
        cfg.network,
        a.yes,
    )?;
    let expiration = u32::try_from(clock::now_unix() + i64::from(cfg.tx_ttl_secs)).map_err(|_| "clock out of range")?;
    let tx = saturn::swap_tx(cfg.network, kp.address(), m.pool_id, &tin.symbol, &tout.symbol, amount_in, min_out, expiration);
    let signed = tx.sign(&kp);
    let p = Pending {
        hash: signed.hash.clone(),
        slot: 0,
        side,
        amount_in,
        min_out,
        expiration,
        signed_hex: signed.hex,
        created_at: clock::now_unix(),
    };
    info!("sending {} ({})", p.hash, cfg.network.explorer_tx(&p.hash));
    match engine::confirm(&rpc, cfg, kp.address(), &m, &p, false) {
        Outcome::Filled { amount_out, fee_kcal, exact } => {
            println!(
                "done: received {}{} (gas {} KCAL)  {}",
                m.fmt_token(tout, amount_out),
                if exact { "" } else { " (at least; exact payout unreadable)" },
                format_units_short(fee_kcal, 10, 6),
                cfg.network.explorer_tx(&p.hash)
            );
            Ok(())
        }
        Outcome::Failed(why) => Err(format!("the swap failed: {why}")),
        Outcome::Expired => Err("the swap expired without executing (nothing was traded)".into()),
    }
}
