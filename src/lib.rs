//! Saturn Grid: a fast, safety-first grid trading bot for Saturn DEX v4 pools
//! on the Phantasma blockchain.
//!
//! - `phantasma`: native keys, scripts, transactions, VM decoding and RPC
//!   (byte-for-byte compatible with phantasma-sdk-ts; see tests/vectors.rs);
//! - `saturn`: on-chain pool reads, the exact v4 swap quote and the swap
//!   transaction exactly as the Saturn website builds it;
//! - `grid`: the strategy (pure logic);
//! - `engine`: the plan, the poll loop and crash-safe execution.

#[macro_use]
pub mod log;

pub mod api;
pub mod clock;
pub mod config;
pub mod engine;
pub mod grid;
pub mod keyfile;
pub mod phantasma;
pub mod saturn;
pub mod state;
pub mod units;
