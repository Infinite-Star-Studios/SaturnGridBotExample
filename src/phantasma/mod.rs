//! Native Phantasma support: keys, scripts, transactions, VM answers and RPC.
//! Checked byte-for-byte against phantasma-sdk-ts (tests/vectors.rs).

pub mod base58;
pub mod keys;
pub mod rpc;
pub mod script;
pub mod tx;
pub mod vm;
