//! Exact integer money math: decimal parsing/formatting, 256-bit mul-div,
//! and fixed-point prices. No floating point touches an amount that is signed.

/// Prices are fixed-point integers: quote per base (whole tokens) × 10^18.
pub const PRICE_DECIMALS: u8 = 18;
pub const PRICE_ONE: u128 = 1_000_000_000_000_000_000;

const MASK64: u128 = u64::MAX as u128;

pub fn pow10(n: u8) -> Option<u128> {
    10u128.checked_pow(u32::from(n))
}

/// The full 256-bit product `a × b` as (high, low) 128-bit halves.
fn mul_256(a: u128, b: u128) -> (u128, u128) {
    let (a1, a0) = (a >> 64, a & MASK64);
    let (b1, b0) = (b >> 64, b & MASK64);
    let p00 = a0 * b0;
    let p01 = a0 * b1;
    let p10 = a1 * b0;
    let p11 = a1 * b1;
    let (mid, c1) = p01.overflowing_add(p10);
    let (mid, c2) = mid.overflowing_add(p00 >> 64);
    let lo = (mid << 64) | (p00 & MASK64);
    let hi = p11 + (mid >> 64) + ((u128::from(c1) + u128::from(c2)) << 64);
    (hi, lo)
}

/// (quotient, remainder) of the 256-bit (hi, lo) by `d`, or `None` when `d`
/// is zero or the quotient does not fit in 128 bits.
fn div_256(hi: u128, lo: u128, d: u128) -> Option<(u128, u128)> {
    if d == 0 {
        return None;
    }
    if hi == 0 {
        return Some((lo / d, lo % d));
    }
    if hi >= d {
        return None;
    }
    let (mut rem, mut q) = (hi, 0u128);
    for i in (0..128).rev() {
        let carry = rem >> 127;
        rem = (rem << 1) | ((lo >> i) & 1);
        q <<= 1;
        if carry == 1 || rem >= d {
            rem = rem.wrapping_sub(d);
            q |= 1;
        }
    }
    Some((q, rem))
}

/// ⌊a × b / d⌋ without intermediate overflow; `None` if `d == 0` or the result overflows.
pub fn mul_div(a: u128, b: u128, d: u128) -> Option<u128> {
    let (hi, lo) = mul_256(a, b);
    div_256(hi, lo, d).map(|(q, _)| q)
}

/// ⌈a × b / d⌉ without intermediate overflow.
pub fn mul_div_ceil(a: u128, b: u128, d: u128) -> Option<u128> {
    let (hi, lo) = mul_256(a, b);
    let (q, r) = div_256(hi, lo, d)?;
    if r == 0 { Some(q) } else { q.checked_add(1) }
}

/// Parse a non-negative decimal string ("12", "0.5", "1_000.25") into raw
/// units with `decimals` places. More fractional digits than that is an error,
/// never a silent rounding.
pub fn parse_units(s: &str, decimals: u8) -> Result<u128, String> {
    let t: String = s.trim().chars().filter(|&c| c != '_').collect();
    let (int, frac) = t.split_once('.').unwrap_or((&t, ""));
    if (int.is_empty() && frac.is_empty()) || !int.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit()) {
        return Err(format!("not a plain non-negative decimal number: {s:?}"));
    }
    if frac.len() > usize::from(decimals) {
        return Err(format!("{s:?} has more than {decimals} decimal places"));
    }
    let overflow = || format!("{s:?} is too large");
    let mut v: u128 = 0;
    for c in int.bytes() {
        v = v.checked_mul(10).and_then(|v| v.checked_add(u128::from(c - b'0'))).ok_or_else(overflow)?;
    }
    let fb = frac.as_bytes();
    for i in 0..usize::from(decimals) {
        let digit = fb.get(i).map_or(0, |c| c - b'0');
        v = v.checked_mul(10).and_then(|v| v.checked_add(u128::from(digit))).ok_or_else(overflow)?;
    }
    Ok(v)
}

/// Raw units as a decimal string, trailing zeros trimmed ("1.5", "100").
pub fn format_units(raw: u128, decimals: u8) -> String {
    let Some(unit) = pow10(decimals) else { return raw.to_string() };
    let (int, frac) = (raw / unit, raw % unit);
    if frac == 0 {
        return int.to_string();
    }
    let mut f = format!("{frac:0width$}", width = usize::from(decimals));
    while f.ends_with('0') {
        f.pop();
    }
    format!("{int}.{f}")
}

/// Raw units with at most `max_frac` fractional digits (rounded down), for display.
pub fn format_units_short(raw: u128, decimals: u8, max_frac: u8) -> String {
    if decimals <= max_frac {
        return format_units(raw, decimals);
    }
    let cut = pow10(decimals - max_frac).unwrap_or(1);
    format_units(raw / cut * cut, decimals)
}

/// A fixed-point price as a short human string with ~`sig` significant digits.
pub fn format_price(p: u128) -> String {
    let int_digits = (p / PRICE_ONE).to_string().len() as u8;
    let sig: u8 = 8;
    let frac = if p >= PRICE_ONE {
        sig.saturating_sub(int_digits)
    } else {
        // Leading zeros after the point, plus the significant digits.
        let frac_str = format!("{:018}", p % PRICE_ONE);
        let zeros = frac_str.bytes().take_while(|&c| c == b'0').count() as u8;
        (zeros + sig).min(PRICE_DECIMALS)
    };
    format_units_short(p, PRICE_DECIMALS, frac)
}

/// The spot price (quote per base, fixed point) of two reserves at the same scale.
pub fn price_from_reserves(reserve_quote: u128, reserve_base: u128) -> Option<u128> {
    mul_div(reserve_quote, PRICE_ONE, reserve_base)
}

/// Basis points by which `a` exceeds `b` (negative when below), for display and limits.
pub fn bps_diff(a: u128, b: u128) -> i64 {
    if b == 0 {
        return i64::MAX;
    }
    let (num, neg) = if a >= b { (a - b, false) } else { (b - a, true) };
    let bps = mul_div(num, 10_000, b).map_or(i64::MAX, |x| i64::try_from(x).unwrap_or(i64::MAX));
    if neg { -bps } else { bps }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (a, b, d, ⌊a·b/d⌋ or None on overflow), computed with Python's arbitrary-precision integers.
    const MUL_DIV_VECTORS: &[(u128, u128, u128, Option<u128>)] = &[
        (
            165307331290540082820841990268669583972,
            58433433997497034883249940012358768007,
            39560890902940989808003279146549325277,
            Some(244167277626957000484388715701625254027),
        ),
        (
            126788180772750066613321360072558757455,
            81939395576549283110604900360593474945,
            43330792800576529769790048009620247795,
            Some(239758984945947711428351694269144330303),
        ),
        (
            29342448239274894371986653076164255759,
            113048519090406492074124240124849498948,
            81533465221927993431910462898374298910,
            Some(40684157246450630727405301383254957856),
        ),
        (
            45882840773800576900431581635267209435,
            1102393747177890077414905724450,
            825251916550973113757293533107630110,
            Some(61291535054159767114725342942758),
        ),
        (
            66013997694611004050606302056517981164,
            89212309287484009584845210270,
            157436031683283816636916369721172331,
            Some(37407327386670919360151349865665),
        ),
        (
            333496723512717530156135206212790609394,
            361546897569250960228239141359,
            918533013596627866138067965812952448,
            Some(131268777442640140122960723792427),
        ),
        (
            406342485962404491834965410,
            703097725500245086801574938,
            984690022847268578,
            Some(290140522423669809509145409982834740),
        ),
        (
            433325292316991260045677798,
            175454902924064833550014618,
            561088824916959865,
            Some(135502693551723972669535271255700779),
        ),
        (
            902153792987369555758169519,
            1181307915050464431903615999,
            590710975335154037,
            Some(1804133426916801788864933297979640172),
        ),
        (
            69727989327964661514233455201871616184,
            192841827805295692210194003073345532258,
            280884120108384930393407987202093157633,
            Some(47871958393391047069039988306321687080),
        ),
        (
            257602903967990230624391422414221505589,
            226700710220976353589316608103998599827,
            1016873559088250680269284112321691183,
            None,
        ),
        (
            88035116277123455786332525727755611871,
            191541809064368823453725825799626082898,
            172792844424203018792892715113267435437,
            Some(97587405827497212481927699919393056304),
        ),
        (15075383936102195926, 2236448855756078371, 1, Some(33715325153979320970296081115252916546)),
        (14546918357074907050, 3309227302724303258, 1, Some(48139059397733647628861641626362168900)),
        (6117283643339956340, 13788977938614728931, 1, Some(84351089202263389840837383550474872540)),
        (579718595179432543722452081925, 95345734, 40684860995713751513862040491, Some(1358581389)),
        (1138870549961846129626735107209, 43379833, 1237271119332501527140265812465, Some(39929820)),
        (1018204046298071576473950305699, 233720596, 631569231788295585704255530779, Some(376799952)),
        (64573838934061329593085574328278575842, 0, 1, Some(0)),
        (152740951912824407328883378011354560062, 1, 1, Some(152740951912824407328883378011354560062)),
        (
            306140114982837747817154896885068,
            907431196088504865232447386427554,
            1082116644730345884418476996353,
            Some(256720097655252773123824061607111760),
        ),
        (
            946683983016481292921060643364682,
            31850555793849894278072627615376,
            1054594874424282141279670265483,
            Some(28591463652496034925949061719352256),
        ),
        (
            601405293338900611398636078018044,
            1104191469372666774856011074218632,
            508708048201040941144267574078,
            Some(1305398247361602307061486774236290780),
        ),
        (98765432109876543210987654321, 12345678901234567890, 1000000000039, Some(1219326311322664226108073159405531526)),
    ];

    #[test]
    fn mul_div_matches_big_integers() {
        for &(a, b, d, want) in MUL_DIV_VECTORS {
            assert_eq!(mul_div(a, b, d), want, "{a} * {b} / {d}");
            assert_eq!(mul_div(b, a, d), want, "commutes: {b} * {a} / {d}");
        }
        let m = u128::MAX;
        assert_eq!(mul_div(m, m, m), Some(m));
        assert_eq!(mul_div(m, 2, 2), Some(m));
        assert_eq!(mul_div(m, 3, 2), None);
        assert_eq!(mul_div(1, 1, 0), None);
        // ⌊(2^127 + 1)·(2^127 + 3) / (2^126 + 5)⌋
        let (a, b, d) = ((1u128 << 127) + 1, (1u128 << 127) + 3, (1u128 << 126) + 5);
        assert_eq!(mul_div(a, b, d), Some(340_282_366_920_938_463_463_374_607_431_768_211_444));
        assert_eq!(mul_div_ceil(10, 10, 3), Some(34));
        assert_eq!(mul_div_ceil(9, 10, 3), Some(30));
        assert_eq!(mul_div_ceil(m, 1, 1), Some(m));
    }

    #[test]
    fn parse_and_format() {
        assert_eq!(parse_units("10", 9).unwrap(), 10_000_000_000);
        assert_eq!(parse_units("0.5", 8).unwrap(), 50_000_000);
        assert_eq!(parse_units(".25", 2).unwrap(), 25);
        assert_eq!(parse_units("1_000", 0).unwrap(), 1000);
        assert!(parse_units("0.123", 2).is_err(), "never rounds silently");
        assert!(parse_units("-1", 8).is_err());
        assert!(parse_units("1e5", 8).is_err());
        assert!(parse_units("", 8).is_err());
        assert!(parse_units(".", 8).is_err());
        assert!(parse_units("999999999999999999999999999999999999999", 8).is_err());
        assert_eq!(format_units(1_500_000_000, 9), "1.5");
        assert_eq!(format_units(100, 0), "100");
        assert_eq!(format_units(7, 8), "0.00000007");
        assert_eq!(format_units_short(123_456_789, 8, 2), "1.23");
    }

    #[test]
    fn prices() {
        // Pool v4:33 (RA/TAZ) as read from mainnet: 46.194068995756870665 TAZ per RA.
        let p = price_from_reserves(16_656_844_276_088, 360_584_045_489).unwrap();
        assert_eq!(p, 46_194_068_995_756_870_665);
        assert_eq!(format_price(p), "46.194068");
        assert_eq!(format_price(parse_units("0.000012345678912", PRICE_DECIMALS).unwrap()), "0.000012345678");
        assert_eq!(bps_diff(101, 100), 100);
        assert_eq!(bps_diff(99, 100), -100);
    }
}
