//! Base58 (Bitcoin alphabet), as used by Phantasma addresses and WIF keys.

const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// The value of one base58 character, or `None` outside the alphabet.
fn digit(c: u8) -> Option<u32> {
    ALPHABET.iter().position(|&a| a == c).map(|p| p as u32)
}

/// Encode bytes as base58. Leading zero bytes become leading `1`s.
pub fn encode(input: &[u8]) -> String {
    let zeros = input.iter().take_while(|&&b| b == 0).count();
    // Little-endian base-58 digits of the big number.
    let mut digits: Vec<u8> = Vec::with_capacity(input.len() * 138 / 100 + 1);
    for &byte in &input[zeros..] {
        let mut carry = u32::from(byte);
        for d in digits.iter_mut() {
            carry += u32::from(*d) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut out = String::with_capacity(zeros + digits.len());
    out.extend(std::iter::repeat_n('1', zeros));
    out.extend(digits.iter().rev().map(|&d| ALPHABET[d as usize] as char));
    out
}

/// Decode base58 text, or `None` when it holds a character outside the alphabet.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let zeros = bytes.iter().take_while(|&&c| c == b'1').count();
    // Little-endian base-256 digits of the big number.
    let mut acc: Vec<u8> = Vec::with_capacity(bytes.len());
    for &c in &bytes[zeros..] {
        let mut carry = digit(c)?;
        for b in acc.iter_mut() {
            carry += u32::from(*b) * 58;
            *b = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            acc.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let mut out = vec![0u8; zeros];
    out.extend(acc.iter().rev());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for case in [&b""[..], &[0], &[0, 0, 1], &[1, 2, 3, 255], &[255; 40]] {
            assert_eq!(decode(&encode(case)).unwrap(), case);
        }
    }

    #[test]
    fn known_vectors() {
        assert_eq!(encode(b"hello world"), "StV1DL6CwTryKyV");
        assert_eq!(encode(&[0, 0, 0x28, 0x7f, 0xb4, 0xcd]), "11233QC4");
        assert_eq!(decode("StV1DL6CwTryKyV").unwrap(), b"hello world");
        assert!(decode("0OIl").is_none());
    }
}
