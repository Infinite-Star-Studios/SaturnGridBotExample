//! Golden vectors: this crate's native Phantasma code must produce the same
//! keys, scripts and signed transactions as phantasma-sdk-ts, byte for byte,
//! and the same swap transaction as the Saturn website.
//!
//! - tests/fixtures/sdk_escrow_vectors.json comes from Saturn's own P2P bot kit
//!   (saturn-p2p-bot-kit 0.3.0, MIT, Infinite Star Studios), whose vectors were
//!   produced by phantasma-sdk-ts. Test keys there are SHA-256 of a label.
//! - The swap vector is a public mainnet Saturn swap, read from the chain.

use ed25519_dalek::{Signature, VerifyingKey};
use saturn_grid::phantasma::keys::{Address, KeyPair};
use saturn_grid::phantasma::script::{Arg, ScriptBuilder};
use saturn_grid::phantasma::tx::{Transaction, from_hex, hash_of_unsigned, to_hex};
use saturn_grid::saturn::{Network, swap_tx};
use serde_json::Value;
use sha2::{Digest, Sha256};

fn fixture() -> Value {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sdk_escrow_vectors.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn key_of(label: &str) -> KeyPair {
    let sk: [u8; 32] = Sha256::digest(label.as_bytes()).into();
    KeyPair::from_private_key(&sk)
}

/// A 0x-prefixed 256-bit hex number as a decimal string (what the escrow takes).
fn hex_to_decimal(h: &str) -> String {
    let mut bytes = from_hex(h.trim_start_matches("0x")).unwrap();
    let mut digits = Vec::new();
    while bytes.iter().any(|&b| b != 0) {
        let mut rem = 0u32;
        for b in bytes.iter_mut() {
            let cur = (rem << 8) | u32::from(*b);
            *b = (cur / 10) as u8;
            rem = cur % 10;
        }
        digits.push(char::from(b'0' + rem as u8));
    }
    if digits.is_empty() {
        return "0".into();
    }
    digits.iter().rev().collect()
}

#[test]
fn keys_and_addresses_match_the_sdk() {
    let v = fixture();
    for k in v["keys"].as_array().unwrap() {
        let kp = key_of(k["label"].as_str().unwrap());
        assert_eq!(to_hex(&kp.public_key()).to_lowercase(), k["publicKey"].as_str().unwrap());
        assert_eq!(kp.address().text(), k["address"].as_str().unwrap());
        // And through a WIF, the way a key file is read.
        let back = KeyPair::from_wif(&kp.to_wif()).unwrap();
        assert_eq!(back.address(), kp.address());
    }
}

#[test]
fn escrow_transactions_match_the_sdk_byte_for_byte() {
    let v = fixture();
    let keys = v["keys"].as_array().unwrap();
    let configs = v["configs"].as_array().unwrap();
    let vectors = v["escrow"].as_array().unwrap();
    assert_eq!(vectors.len(), 24);
    for (i, e) in vectors.iter().enumerate() {
        let kp = key_of(keys[e["key"].as_u64().unwrap() as usize]["label"].as_str().unwrap());
        let cfg = &configs[e["config"].as_u64().unwrap() as usize];
        let from = kp.address().text();
        let method = e["method"].as_str().unwrap();
        let (args, label): (Vec<String>, &str) = match method {
            "lock" => {
                let recipient = key_of(keys[e["recipient"].as_u64().unwrap() as usize]["label"].as_str().unwrap());
                (
                    vec![
                        from.clone(),
                        hex_to_decimal(e["h"].as_str().unwrap()),
                        e["symbol"].as_str().unwrap().to_string(),
                        e["amount"].as_str().unwrap().to_string(),
                        recipient.address().text(),
                        e["deadline"].as_u64().unwrap().to_string(),
                    ],
                    "SATURN_p2pLock",
                )
            }
            "claim" => (vec![hex_to_decimal(e["secret"].as_str().unwrap())], "SATURN_p2pClaim"),
            "refund" => (vec![hex_to_decimal(e["h"].as_str().unwrap())], "SATURN_p2pRefund"),
            other => panic!("unknown method {other}"),
        };
        let mut sb = ScriptBuilder::new();
        sb.allow_gas(Arg::Str(&from), 100_000, 300_000);
        let args: Vec<Arg<'_>> = args.iter().map(|a| Arg::Str(a)).collect();
        sb.call_contract("saturnescrow", method, &args);
        sb.spend_gas(Arg::Str(&from));
        let tx = Transaction {
            nexus: cfg["nexus"].as_str().unwrap().to_string(),
            chain: cfg["chain"].as_str().unwrap().to_string(),
            script: sb.end(),
            expiration: (e["expiresAtMs"].as_u64().unwrap() / 1000) as u32,
            payload: label.as_bytes().to_vec(),
        };
        assert_eq!(
            to_hex(&tx.unsigned_bytes()),
            e["unsigned"].as_str().unwrap().to_uppercase(),
            "vector {i} ({method}): unsigned"
        );
        assert_eq!(tx.sign(&kp).hex, e["signed"].as_str().unwrap().to_uppercase(), "vector {i} ({method}): signed");
    }
}

/// Public mainnet transaction 3DE618B3…E053: a 100 TAZ -> RA swap in pool v4:33
/// made on the Saturn website, as `getTransaction` returned it.
const SWAP_HASH: &str = "3DE618B351D0CEAB8EA0BB88E4D39AAD20D5A998D833B8D8AB6885445F37E053";
const SWAP_SENDER: &str = "P2KCSArwVBhCRkkJqq4HWaQAUkqQKyvVs8nMr4Hw2XWxqgd";
const SWAP_SCRIPT: &str = "0D000405393030303003000D00040631303030303003000D000223220000000000000000000000000000000000000000000000000000000000000000000003000D000223220100809F329AC03F27B81870A0154EB25FDF4CD9A9E1DE0DB9CD0C8FA929E975F12203000D000408416C6C6F7747617303000D0004036761732D00012E010D00040A3230393135303531323403000D000402524103000D00040354415A03000D00040C31303030303030303030303003000D000402333303000D000223220100809F329AC03F27B81870A0154EB25FDF4CD9A9E1DE0DB9CD0C8FA929E975F12203000D0004047377617003000D00040A73617475726E737761702D00012E010D000223220100809F329AC03F27B81870A0154EB25FDF4CD9A9E1DE0DB9CD0C8FA929E975F12203000D0004085370656E6447617303000D0004036761732D00012E010B";
const SWAP_SIGNATURE: &str = "9C00B8286099D2D0C973AE5841488D884AA744365FF65909F6E6591807BDF1702630B1A557CC13442997DE64A539A241CFD23BD72C9C5179F33D08EA3C60BB0C";

#[test]
fn swap_transaction_matches_a_real_website_swap() {
    let from = Address::from_text(SWAP_SENDER).unwrap();
    let tx = swap_tx(Network::Mainnet, &from, 33, "TAZ", "RA", 100_000_000_000, 2_091_505_124, 1_794_679_448);
    assert_eq!(to_hex(&tx.script), SWAP_SCRIPT, "script");
    assert_eq!(String::from_utf8(tx.payload.clone()).unwrap(), "SATURN_swap TAZ>RA");
    let unsigned = tx.unsigned_bytes();
    assert_eq!(hash_of_unsigned(&unsigned), SWAP_HASH, "hash");
    // The chain's signature verifies over exactly our unsigned bytes.
    let pk: [u8; 32] = from.as_bytes()[2..].try_into().unwrap();
    let vk = VerifyingKey::from_bytes(&pk).unwrap();
    let sig = Signature::from_bytes(&from_hex(SWAP_SIGNATURE).unwrap().try_into().unwrap());
    vk.verify_strict(&unsigned, &sig).expect("the website's signature covers our unsigned bytes");
}

#[test]
fn fixture_holds_no_key_material() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sdk_escrow_vectors.json")).unwrap();
    let lower = text.to_lowercase();
    assert!(!lower.contains("\"wif\"") && !lower.contains("privatekey") && !lower.contains("private_key"));
}
