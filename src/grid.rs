//! The grid strategy: pure logic, no I/O (unit-tested below).
//!
//! The price range [lower, upper] is cut into `n` slots by `n + 1` lines.
//! Each slot holds either quote (waiting to BUY base when the price falls to
//! its lower line) or base (waiting to SELL when the price rises to its upper
//! line). A buy turns the slot into a base slot; a sell turns it back. Every
//! completed buy-then-sell earns the gap between two neighbouring lines.
//!
//! On a DEX there is no order book: the bot watches the pool price and swaps
//! when it crosses a line. Prices are fixed-point integers (quote per base
//! × 10^18), so every comparison here is exact.

use serde::{Deserialize, Serialize};

use crate::units::{PRICE_ONE, mul_div};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Spacing {
    /// Equal percentage steps (the usual choice for crypto).
    Geometric,
    /// Equal absolute steps.
    Arithmetic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Holding {
    Quote,
    Base,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn word(self) -> &'static str {
        match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        }
    }
}

/// Serialize u128 as a decimal string (JSON numbers lose precision past 2^53).
pub mod u128_str {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Serialize i128 as a decimal string.
pub mod i128_str {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &i128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i128, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    /// Lower line: buy base when the price is at or below it.
    #[serde(with = "u128_str")]
    pub buy_at: u128,
    /// Upper line: sell base when the price is at or above it.
    #[serde(with = "u128_str")]
    pub sell_at: u128,
    pub holding: Holding,
    /// Raw units of the token held (quote or base).
    #[serde(with = "u128_str")]
    pub amount: u128,
    /// Quote (raw) paid for the base held; 0 while holding quote.
    #[serde(with = "u128_str")]
    pub cost_quote: u128,
    pub round_trips: u32,
}

/// `n + 1` strictly increasing lines from `lower` to `upper` (fixed point).
pub fn grid_lines(lower: u128, upper: u128, n: usize, spacing: Spacing) -> Result<Vec<u128>, String> {
    if lower == 0 || upper <= lower {
        return Err("the grid needs 0 < lower < upper".into());
    }
    if !(1..=500).contains(&n) {
        return Err("the grid needs 1 to 500 levels".into());
    }
    let mut lines = Vec::with_capacity(n + 1);
    lines.push(lower);
    for i in 1..n {
        let p = match spacing {
            Spacing::Arithmetic => lower + mul_div(upper - lower, i as u128, n as u128).ok_or("overflow")?,
            Spacing::Geometric => {
                // Floating point only places the lines; all trading math is integer.
                let (lo, hi) = (lower as f64, upper as f64);
                (lo * (hi / lo).powf(i as f64 / n as f64)).round() as u128
            }
        };
        lines.push(p.clamp(lower, upper));
    }
    lines.push(upper);
    if lines.windows(2).any(|w| w[1] <= w[0]) {
        return Err("the range is too narrow for that many levels".into());
    }
    Ok(lines)
}

/// The base (raw) that `quote_raw` buys at `price`, with the two tokens' decimals.
pub fn base_for_quote(quote_raw: u128, price: u128, base_decimals: u8, quote_decimals: u8) -> Option<u128> {
    // base = quote / price, rescaled from quote decimals to base decimals.
    let at_base_decimals = if base_decimals >= quote_decimals {
        quote_raw.checked_mul(10u128.checked_pow(u32::from(base_decimals - quote_decimals))?)?
    } else {
        quote_raw / 10u128.checked_pow(u32::from(quote_decimals - base_decimals))?
    };
    mul_div(at_base_decimals, PRICE_ONE, price)
}

/// The quote (raw) that `base_raw` is worth at `price`.
pub fn quote_for_base(base_raw: u128, price: u128, base_decimals: u8, quote_decimals: u8) -> Option<u128> {
    let q = mul_div(base_raw, price, PRICE_ONE)?;
    if quote_decimals >= base_decimals {
        q.checked_mul(10u128.checked_pow(u32::from(quote_decimals - base_decimals))?)
    } else {
        Some(q / 10u128.checked_pow(u32::from(base_decimals - quote_decimals))?)
    }
}

/// Fresh slots for a new grid at the current price. A slot whose lower line is
/// below the price holds quote (buys on a dip); the others hold base (sell on
/// a rise), sized to be worth `quote_per_slot` at their sell line.
pub fn init_slots(
    lines: &[u128],
    spot: u128,
    quote_per_slot: u128,
    base_decimals: u8,
    quote_decimals: u8,
) -> Result<Vec<Slot>, String> {
    let mut slots = Vec::with_capacity(lines.len().saturating_sub(1));
    for w in lines.windows(2) {
        let (buy_at, sell_at) = (w[0], w[1]);
        let slot = if buy_at < spot {
            Slot { buy_at, sell_at, holding: Holding::Quote, amount: quote_per_slot, cost_quote: 0, round_trips: 0 }
        } else {
            let base = base_for_quote(quote_per_slot, sell_at, base_decimals, quote_decimals).ok_or("amount overflow")?;
            // Cost basis of inventory brought in: its value at the current price.
            let cost = quote_for_base(base, spot, base_decimals, quote_decimals).ok_or("amount overflow")?;
            Slot { buy_at, sell_at, holding: Holding::Base, amount: base, cost_quote: cost, round_trips: 0 }
        };
        slots.push(slot);
    }
    Ok(slots)
}

/// What to do at `spot`, if anything: sell the lowest base slot whose sell
/// line the price reached, else buy with the highest quote slot whose buy line
/// the price reached. `skip` excludes slots (cooling down after a failure).
pub fn next_action(slots: &[Slot], spot: u128, skip: impl Fn(usize) -> bool) -> Option<(usize, Side)> {
    let sell = slots
        .iter()
        .enumerate()
        .filter(|(i, s)| s.holding == Holding::Base && spot >= s.sell_at && s.amount > 0 && !skip(*i))
        .min_by_key(|(_, s)| s.sell_at)
        .map(|(i, _)| (i, Side::Sell));
    sell.or_else(|| {
        slots
            .iter()
            .enumerate()
            .filter(|(i, s)| s.holding == Holding::Quote && spot <= s.buy_at && s.amount > 0 && !skip(*i))
            .max_by_key(|(_, s)| s.buy_at)
            .map(|(i, _)| (i, Side::Buy))
    })
}

/// Record a buy: the slot spent all its quote and now holds `base_received`.
pub fn apply_buy(slot: &mut Slot, quote_spent: u128, base_received: u128) {
    slot.holding = Holding::Base;
    slot.amount = base_received;
    slot.cost_quote = quote_spent;
}

/// Record a sell: returns the realized profit in quote (raw, may be negative)
/// and the quote banked outside the grid when not compounding.
pub fn apply_sell(slot: &mut Slot, quote_received: u128, compound: bool, quote_per_slot: u128) -> (i128, u128) {
    let pnl = quote_received as i128 - slot.cost_quote as i128;
    let keep = if compound { quote_received } else { quote_received.min(quote_per_slot) };
    slot.holding = Holding::Quote;
    slot.amount = keep;
    slot.cost_quote = 0;
    slot.round_trips += 1;
    (pnl, quote_received - keep)
}

/// Total (quote, base) the slots hold, raw.
pub fn holdings(slots: &[Slot]) -> (u128, u128) {
    slots.iter().fold((0, 0), |(q, b), s| match s.holding {
        Holding::Quote => (q + s.amount, b),
        Holding::Base => (q, b + s.amount),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::parse_units;

    fn p(s: &str) -> u128 {
        parse_units(s, 18).unwrap()
    }

    #[test]
    fn lines_are_exact_at_the_ends_and_increasing() {
        let g = grid_lines(p("40"), p("52"), 12, Spacing::Geometric).unwrap();
        assert_eq!(g.len(), 13);
        assert_eq!((g[0], g[12]), (p("40"), p("52")));
        // Equal ratios: each step ≈ (52/40)^(1/12) = 1.02210…
        let r = g[1] as f64 / g[0] as f64;
        assert!((r - 1.022_105).abs() < 1e-5, "{r}");
        let a = grid_lines(p("1"), p("2"), 4, Spacing::Arithmetic).unwrap();
        assert_eq!(a, vec![p("1"), p("1.25"), p("1.5"), p("1.75"), p("2")]);
        assert!(grid_lines(p("2"), p("1"), 4, Spacing::Arithmetic).is_err());
        assert!(grid_lines(1, 3, 10, Spacing::Arithmetic).is_err(), "too narrow");
    }

    #[test]
    fn init_splits_at_the_price() {
        let lines = grid_lines(p("90"), p("120"), 3, Spacing::Arithmetic).unwrap(); // 90 100 110 120
        let s = init_slots(&lines, p("105"), 1_000, 8, 8).unwrap();
        assert_eq!(s.iter().map(|s| s.holding).collect::<Vec<_>>(), [Holding::Quote, Holding::Quote, Holding::Base]);
        // The base slot sells at 120 and holds 1000/120 base units.
        assert_eq!(s[2].amount, 8);
        // Nothing triggers at the starting price.
        assert_eq!(next_action(&s, p("105"), |_| false), None);
    }

    #[test]
    fn full_cycle_down_and_up() {
        let lines = grid_lines(p("90"), p("120"), 3, Spacing::Arithmetic).unwrap();
        let mut s = init_slots(&lines, p("105"), 100_000_000, 8, 8).unwrap();
        // Price dips to 100: the [100,110] slot buys.
        assert_eq!(next_action(&s, p("100"), |_| false), Some((1, Side::Buy)));
        apply_buy(&mut s[1], 100_000_000, 990_000);
        assert_eq!(next_action(&s, p("100"), |_| false), None);
        // Down to 89: the [90,100] slot buys (the one above is already base).
        assert_eq!(next_action(&s, p("89"), |_| false), Some((0, Side::Buy)));
        apply_buy(&mut s[0], 100_000_000, 1_100_000);
        // Back up to 110: the [100,110] slot sells first (lowest sell line reached).
        assert_eq!(next_action(&s, p("110"), |_| false), Some((0, Side::Sell)));
        let (pnl, banked) = apply_sell(&mut s[0], 108_000_000, false, 100_000_000);
        assert_eq!((pnl, banked), (8_000_000, 8_000_000));
        assert_eq!(s[0].amount, 100_000_000, "fixed order size when not compounding");
        assert_eq!(next_action(&s, p("110"), |_| false), Some((1, Side::Sell)));
        // A skipped slot is passed over.
        assert_eq!(next_action(&s, p("110"), |i| i == 1), None);
    }

    #[test]
    fn invariant_quote_slots_stay_below_base_slots() {
        // Random walk: after every action, no base slot sits below a quote slot.
        let lines = grid_lines(p("1"), p("2"), 20, Spacing::Geometric).unwrap();
        let mut s = init_slots(&lines, p("1.5"), 1_000_000, 8, 8).unwrap();
        let mut price = 1.5f64;
        let mut seed = 42u64;
        for _ in 0..5_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let step = ((seed >> 33) as f64 / (1u64 << 31) as f64 - 0.5) * 0.06;
            price = (price * (1.0 + step)).clamp(0.8, 2.4);
            let spot = (price * 1e18) as u128;
            while let Some((i, side)) = next_action(&s, spot, |_| false) {
                match side {
                    Side::Buy => {
                        let spent = s[i].amount;
                        apply_buy(&mut s[i], spent, 1_000);
                    }
                    Side::Sell => {
                        apply_sell(&mut s[i], 1_000_000, false, 1_000_000);
                    }
                }
            }
            let first_base = s.iter().position(|x| x.holding == Holding::Base).unwrap_or(s.len());
            assert!(s[first_base..].iter().all(|x| x.holding == Holding::Base), "quote slot above a base slot");
        }
    }

    #[test]
    fn decimal_conversions() {
        // 10 TAZ (9 dec) at 46 TAZ/RA buys 0.217391304 RA (9 dec).
        assert_eq!(base_for_quote(10_000_000_000, p("46"), 9, 9), Some(217_391_304));
        // 1 SOUL (8 dec) at 9.5 KCAL/SOUL is 9.5 KCAL (10 dec).
        assert_eq!(quote_for_base(100_000_000, p("9.5"), 8, 10), Some(95_000_000_000));
        // 1 C (6 dec) quote -> base with 8 decimals at price 2: 0.5 base.
        assert_eq!(base_for_quote(1_000_000, p("2"), 8, 6), Some(50_000_000));
    }
}
