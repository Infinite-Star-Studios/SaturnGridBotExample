//! Phantasma addresses and Ed25519 key pairs (WIF import, signing).
//!
//! A user address is 34 bytes: kind `1` (user), a reserved `0`, then the
//! 32-byte Ed25519 public key. Its text form is `P` + base58 of those bytes.
//! A WIF is base58 of `0x80`, the 32-byte private key, `0x01` and the first
//! four bytes of the double SHA-256 of those 34 bytes.

use std::fmt;

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use super::base58;

/// Address kinds (first byte of the 34 address bytes).
const KIND_USER: u8 = 1;
const KIND_SYSTEM: u8 = 2;
const KIND_INTEROP: u8 = 3;

/// A Phantasma address (34 bytes).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address([u8; 34]);

impl Address {
    /// The null address (34 zero bytes), used as the gas target.
    pub const NULL: Address = Address([0u8; 34]);

    /// The user address of an Ed25519 public key.
    pub fn from_public_key(pk: &[u8; 32]) -> Self {
        let mut b = [0u8; 34];
        b[0] = KIND_USER;
        b[2..].copy_from_slice(pk);
        Address(b)
    }

    pub fn from_bytes(b: [u8; 34]) -> Self {
        Address(b)
    }

    pub fn as_bytes(&self) -> &[u8; 34] {
        &self.0
    }

    pub fn is_user(&self) -> bool {
        self.0[0] == KIND_USER
    }

    /// Parse the text form (`P…`, `S…` or `X…`). The prefix must match the kind byte.
    pub fn from_text(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (prefix, rest) = s.split_at_checked(1).ok_or("empty address")?;
        let kind = match prefix {
            "P" => KIND_USER,
            "S" => KIND_SYSTEM,
            "X" => KIND_INTEROP,
            _ => return Err(format!("not a Phantasma address: {s}")),
        };
        let raw = base58::decode(rest).ok_or_else(|| format!("not a Phantasma address: {s}"))?;
        let bytes: [u8; 34] = raw.try_into().map_err(|_| format!("not a Phantasma address (wrong length): {s}"))?;
        if bytes[0] != kind {
            return Err(format!("not a Phantasma address (kind mismatch): {s}"));
        }
        Ok(Address(bytes))
    }

    pub fn text(&self) -> String {
        let prefix = match self.0[0] {
            KIND_USER => 'P',
            KIND_SYSTEM => 'S',
            KIND_INTEROP => 'X',
            _ => '?',
        };
        format!("{prefix}{}", base58::encode(&self.0))
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address({})", self.text())
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// An Ed25519 key pair. The private key is wiped from memory on drop
/// (ed25519-dalek's `zeroize` feature); `Debug` shows only the address.
pub struct KeyPair {
    signing: SigningKey,
    address: Address,
}

impl KeyPair {
    pub fn from_private_key(sk: &[u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(sk);
        let address = Address::from_public_key(&signing.verifying_key().to_bytes());
        KeyPair { signing, address }
    }

    /// Read a WIF (compressed form: starts with K or L), checksum verified.
    pub fn from_wif(wif: &str) -> Result<Self, String> {
        let wif = wif.trim();
        let mut raw = Zeroizing::new(base58::decode(wif).ok_or("the WIF is not base58")?);
        if raw.len() != 38 || raw[0] != 0x80 || raw[33] != 0x01 {
            return Err("the WIF has the wrong shape (expected a compressed Phantasma WIF)".into());
        }
        let check = sha256(&sha256(&raw[..34]));
        if check[..4] != raw[34..38] {
            return Err("the WIF checksum is wrong".into());
        }
        let mut sk = [0u8; 32];
        sk.copy_from_slice(&raw[1..33]);
        let kp = KeyPair::from_private_key(&sk);
        sk.zeroize();
        raw.zeroize();
        Ok(kp)
    }

    /// The WIF of this key (only for writing a new key file).
    pub fn to_wif(&self) -> Zeroizing<String> {
        let mut body = Zeroizing::new(Vec::with_capacity(38));
        body.push(0x80);
        body.extend_from_slice(self.signing.as_bytes());
        body.push(0x01);
        let check = sha256(&sha256(&body));
        body.extend_from_slice(&check[..4]);
        Zeroizing::new(base58::encode(&body))
    }

    pub fn address(&self) -> &Address {
        &self.address
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// Ed25519 signature of `msg` (Phantasma signs the unsigned transaction bytes).
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }
}

impl fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyPair({})", self.address.text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_text_round_trip() {
        let kp = KeyPair::from_private_key(&[7u8; 32]);
        let text = kp.address().text();
        assert!(text.starts_with("P2K"), "{text}");
        assert_eq!(Address::from_text(&text).unwrap(), *kp.address());
        assert!(Address::from_text("S2K").is_err());
        assert!(Address::from_text("").is_err());
    }

    #[test]
    fn wif_round_trip_and_checksum() {
        let kp = KeyPair::from_private_key(&[9u8; 32]);
        let wif = kp.to_wif();
        assert!(wif.starts_with('K') || wif.starts_with('L'));
        let back = KeyPair::from_wif(&wif).unwrap();
        assert_eq!(back.address(), kp.address());
        // Flip one character: the checksum (or the alphabet) must catch it.
        let mut bad: Vec<char> = wif.chars().collect();
        bad[10] = if bad[10] == 'a' { 'b' } else { 'a' };
        assert!(KeyPair::from_wif(&bad.into_iter().collect::<String>()).is_err());
    }

    #[test]
    fn debug_never_shows_the_key() {
        let kp = KeyPair::from_private_key(&[1u8; 32]);
        let dbg = format!("{kp:?}");
        assert!(dbg.starts_with("KeyPair(P"));
        assert!(!dbg.contains(kp.to_wif().as_str()));
    }
}
