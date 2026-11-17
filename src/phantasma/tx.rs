//! Phantasma transactions: serialization, Ed25519 signing and the hash.
//!
//! Unsigned layout: varstring(nexus) varstring(chain) varbytes(script)
//! u32le(expiration, unix seconds) varbytes(payload). The signed form
//! appends varint(1) and one signature: kind `1` (Ed25519) + varbytes(64).
//! The signature covers the unsigned bytes; the transaction hash is the
//! SHA-256 of the unsigned bytes, byte-reversed, as upper-case hex.

use sha2::{Digest, Sha256};

use super::keys::KeyPair;
use super::script::{write_var_bytes, write_varint};

const SIG_ED25519: u8 = 1;

#[derive(Clone, Debug)]
pub struct Transaction {
    pub nexus: String,
    pub chain: String,
    pub script: Vec<u8>,
    /// Unix seconds after which the chain refuses the transaction.
    pub expiration: u32,
    pub payload: Vec<u8>,
}

/// A signed transaction, ready to broadcast.
#[derive(Clone, Debug)]
pub struct SignedTx {
    /// Upper-case hex of the signed bytes (what `sendRawTransaction` takes).
    pub hex: String,
    /// The transaction hash (upper-case hex), known before broadcasting.
    pub hash: String,
}

pub fn to_hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

impl Transaction {
    pub fn unsigned_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.script.len() + self.payload.len() + 32);
        write_var_bytes(&mut out, self.nexus.as_bytes());
        write_var_bytes(&mut out, self.chain.as_bytes());
        write_var_bytes(&mut out, &self.script);
        out.extend_from_slice(&self.expiration.to_le_bytes());
        write_var_bytes(&mut out, &self.payload);
        out
    }

    /// The hash the chain will give this transaction.
    pub fn hash(&self) -> String {
        hash_of_unsigned(&self.unsigned_bytes())
    }

    pub fn sign(&self, key: &KeyPair) -> SignedTx {
        let unsigned = self.unsigned_bytes();
        let sig = key.sign(&unsigned);
        let mut signed = unsigned.clone();
        write_varint(&mut signed, 1);
        signed.push(SIG_ED25519);
        write_var_bytes(&mut signed, &sig);
        SignedTx { hex: to_hex(&signed), hash: hash_of_unsigned(&unsigned) }
    }
}

pub fn hash_of_unsigned(unsigned: &[u8]) -> String {
    let mut h: [u8; 32] = Sha256::digest(unsigned).into();
    h.reverse();
    to_hex(&h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let b = [0u8, 1, 0xab, 0xff];
        assert_eq!(to_hex(&b), "0001ABFF");
        assert_eq!(from_hex("0001abff").unwrap(), b);
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
    }
}
