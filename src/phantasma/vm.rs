//! Decoder for Phantasma VM objects (the answers of `invokeRawScript`, the
//! `result` of a transaction and the data of contract events).
//!
//! Layout: one type byte, then the value. Numbers are length-prefixed
//! little-endian two's-complement integers; strings and byte arrays are
//! length-prefixed; a struct is a count of (key, value) pairs.

use super::keys::Address;

const T_NONE: u8 = 0;
const T_STRUCT: u8 = 1;
const T_BYTES: u8 = 2;
const T_NUMBER: u8 = 3;
const T_STRING: u8 = 4;
const T_TIMESTAMP: u8 = 5;
const T_BOOL: u8 = 6;
const T_ENUM: u8 = 7;
const T_OBJECT: u8 = 8;

/// Nested structs deeper than this are refused (input comes from the network).
const MAX_DEPTH: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub enum VmValue {
    None,
    Struct(Vec<(VmValue, VmValue)>),
    Bytes(Vec<u8>),
    /// Little-endian two's complement, as on the wire.
    Number(Vec<u8>),
    Str(String),
    Timestamp(u32),
    Bool(bool),
    Enum(u32),
    Object(Vec<u8>),
}

pub struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader { b, i: 0 }
    }

    pub fn at_end(&self) -> bool {
        self.i >= self.b.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.i.checked_add(n).filter(|&e| e <= self.b.len()).ok_or("VM value cut short")?;
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    pub fn varint(&mut self) -> Result<u64, String> {
        Ok(match self.byte()? {
            0xfd => u64::from(u16::from_le_bytes(self.take(2)?.try_into().unwrap())),
            0xfe => u64::from(u32::from_le_bytes(self.take(4)?.try_into().unwrap())),
            0xff => u64::from_le_bytes(self.take(8)?.try_into().unwrap()),
            x => u64::from(x),
        })
    }

    pub fn var_bytes(&mut self) -> Result<&'a [u8], String> {
        let n = usize::try_from(self.varint()?).map_err(|_| "VM length too large")?;
        self.take(n)
    }

    pub fn var_string(&mut self) -> Result<String, String> {
        String::from_utf8(self.var_bytes()?.to_vec()).map_err(|_| "VM string is not UTF-8".to_string())
    }

    pub fn value(&mut self) -> Result<VmValue, String> {
        self.value_at(0)
    }

    fn value_at(&mut self, depth: usize) -> Result<VmValue, String> {
        if depth > MAX_DEPTH {
            return Err("VM struct nested too deep".into());
        }
        Ok(match self.byte()? {
            T_NONE => VmValue::None,
            T_STRUCT => {
                let n = self.varint()?;
                if n > 4096 {
                    return Err("VM struct too large".into());
                }
                let mut fields = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let k = self.value_at(depth + 1)?;
                    let v = self.value_at(depth + 1)?;
                    fields.push((k, v));
                }
                VmValue::Struct(fields)
            }
            T_BYTES => VmValue::Bytes(self.var_bytes()?.to_vec()),
            T_NUMBER => VmValue::Number(self.var_bytes()?.to_vec()),
            T_STRING => VmValue::Str(self.var_string()?),
            T_TIMESTAMP => VmValue::Timestamp(u32::from_le_bytes(self.take(4)?.try_into().unwrap())),
            T_BOOL => VmValue::Bool(self.byte()? != 0),
            T_ENUM => VmValue::Enum(u32::from_le_bytes(self.take(4)?.try_into().unwrap())),
            T_OBJECT => VmValue::Object(self.var_bytes()?.to_vec()),
            t => return Err(format!("unsupported VM type {t}")),
        })
    }
}

/// Decode one VM value from hex.
pub fn decode_hex(hex: &str) -> Result<VmValue, String> {
    let bytes = super::tx::from_hex(hex).ok_or("VM answer is not hex")?;
    Reader::new(&bytes).value()
}

/// A non-negative little-endian two's-complement integer as u128.
fn le_twos_to_u128(b: &[u8]) -> Result<u128, String> {
    if b.last().is_some_and(|&x| x & 0x80 != 0) {
        return Err("expected a non-negative number, got a negative one".into());
    }
    let len = b.iter().rposition(|&x| x != 0).map_or(0, |p| p + 1);
    if len > 16 {
        return Err("number too large".into());
    }
    let mut v: u128 = 0;
    for &x in b[..len].iter().rev() {
        v = (v << 8) | u128::from(x);
    }
    Ok(v)
}

impl VmValue {
    /// A non-negative integer (a Number, or a String of decimal digits).
    pub fn as_u128(&self) -> Result<u128, String> {
        match self {
            VmValue::Number(b) => le_twos_to_u128(b),
            VmValue::Str(s) if !s.is_empty() && s.len() <= 39 && s.bytes().all(|c| c.is_ascii_digit()) => {
                s.parse::<u128>().map_err(|e| e.to_string())
            }
            VmValue::Bool(b) => Ok(u128::from(*b)),
            other => Err(format!("expected a number, got {other:?}")),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            VmValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// An address held as an object or bytes (optionally length-prefixed), or as text.
    pub fn as_address(&self) -> Option<Address> {
        match self {
            VmValue::Object(b) | VmValue::Bytes(b) => {
                let raw = if b.len() == 35 && b[0] == 34 { &b[1..] } else { &b[..] };
                let arr: [u8; 34] = raw.try_into().ok()?;
                Some(Address::from_bytes(arr))
            }
            VmValue::Str(s) => Address::from_text(s).ok(),
            _ => None,
        }
    }

    /// A struct field by its string key.
    pub fn field(&self, name: &str) -> Option<&VmValue> {
        match self {
            VmValue::Struct(fields) => fields.iter().find(|(k, _)| k.as_str() == Some(name)).map(|(_, v)| v),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_from_the_live_chain() {
        // Answers recorded from Phantasma mainnet (saturnpools reads).
        assert_eq!(decode_hex("030100").unwrap().as_u128().unwrap(), 0);
        assert_eq!(decode_hex("03020100").unwrap().as_u128().unwrap(), 1);
        assert_eq!(decode_hex("03021E00").unwrap().as_u128().unwrap(), 30);
        assert_eq!(decode_hex("0306B1E37BF45300").unwrap().as_u128().unwrap(), 360_584_045_489);
        assert_eq!(decode_hex("030778414939260F00").unwrap().as_u128().unwrap(), 16_656_844_276_088);
        assert_eq!(decode_hex("030582DC848000").unwrap().as_u128().unwrap(), 2_156_190_850);
        assert!(decode_hex("0301FF").unwrap().as_u128().is_err(), "-1 is refused");
    }

    #[test]
    fn strings_and_addresses() {
        let v = decode_hex("041073617475726E737761702D342E342E33").unwrap();
        assert_eq!(v.as_str(), Some("saturnswap-4.4.3"));
        let a = decode_hex("082322010070D9B7FE961FEC9AA51DFBBD2CCB1BEA6D0124B8B1DC347E71C684677674A9F6")
            .unwrap()
            .as_address()
            .unwrap();
        assert_eq!(a.text(), "P2KBNc6TpAqLi5LEBHgX62BA3uqa6d87NefwRmJ825fbJ8D");
    }

    #[test]
    fn swap_event_struct() {
        // The saturnswap event of public mainnet transaction
        // 3DE618B351D0CEAB8EA0BB88E4D39AAD20D5A998D833B8D8AB6885445F37E053 (100 TAZ -> RA, pool v4:33).
        let hex = "01080406706F6F6C4964030221000407746F6B656E496E040354415A0408746F6B656E4F7574040252410408\
                   616D6F756E74496E030600E8764817000409616D6F756E744F7574030582DC848000040C7265616C41646D696E\
                   46656503050087930300040F7265616C50726F7669646572466565030580C3C90100040C7265616C5265696E\
                   76657374030600DA4F411700";
        let hex: String = hex.split_whitespace().collect();
        let v = decode_hex(&hex).unwrap();
        assert_eq!(v.field("poolId").unwrap().as_u128().unwrap(), 33);
        assert_eq!(v.field("tokenIn").unwrap().as_str(), Some("TAZ"));
        assert_eq!(v.field("tokenOut").unwrap().as_str(), Some("RA"));
        assert_eq!(v.field("amountIn").unwrap().as_u128().unwrap(), 100_000_000_000);
        assert_eq!(v.field("amountOut").unwrap().as_u128().unwrap(), 2_156_190_850);
        assert_eq!(v.field("realAdminFee").unwrap().as_u128().unwrap(), 60_000_000);
        assert_eq!(v.field("realProviderFee").unwrap().as_u128().unwrap(), 30_000_000);
        assert_eq!(v.field("realReinvest").unwrap().as_u128().unwrap(), 99_880_000_000);
    }

    #[test]
    fn hostile_input_is_refused_not_panicking() {
        for hex in ["", "03", "03FF", "04FDFFFF", "0109", "01FFFFFFFFFFFFFFFFFF", "09", "0302"] {
            let _ = decode_hex(hex);
        }
        let mut deep = String::new();
        for _ in 0..40 {
            deep.push_str("0101");
        }
        assert!(decode_hex(&deep).is_err());
    }
}
