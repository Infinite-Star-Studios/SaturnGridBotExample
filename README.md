# Saturn Grid Bot

A fast, safety-first **grid trading bot for [Saturn](https://ops.saturnx.cc) DEX** (v4 pools) on the
Phantasma blockchain, written in **Rust**. It keeps buying low and selling high inside a price range you
choose, swapping directly against a Saturn pool.

- **Native and exact.** It reads the pool straight from the chain, quotes swaps with the contract's own
  integer math, and signs and sends the same swap transaction the Saturn website builds. All of this is
  checked byte for byte in the tests.
- **Paper first.** `run` without `--live` trades on live prices with simulated fills and never needs a key.
- **Crash-safe.** Every swap is written to disk before it is sent and resolved on chain after a restart.
  It is never sent twice, and a dropped connection is never taken to mean "no swap".

> **Use at your own risk.** This is example software. It comes as-is with no warranty (see `LICENSE`)
> and it is not financial advice. A grid bot loses money when the price leaves its range and stays out.
> Start on paper, then with small amounts.

## Why Rust

Rust is memory-safe without a garbage collector, so there are no GC pauses and no use-after-free
or data-race classes of bug. It compiles to a small native binary (about 3 MB on disk, about 6 MB of RAM while running).
The type system makes mistakes like mixing raw and scaled units hard to write.

The money math is integer-only, with checked 256-bit intermediates. A float never touches an amount
that gets signed.

Built and tested with **Rust 1.100.0** (stable, November 2026), and the latest releases of its few dependencies:

| crate | version | used for |
|---|---|---|
| `ed25519-dalek` | 3.0.0 | signing (keys wiped from memory on drop) |
| `sha2` | 0.11.1 | hashes, WIF checksums |
| `ureq` (rustls) | 3.5.2 | HTTPS JSON-RPC with keep-alive |
| `serde` / `serde_json` / `toml` | 1.0.229 / 1.0.154 / 1.2.1 | config and state |
| `zeroize`, `getrandom` | 1.9.1, 0.4.3 | key hygiene, key generation |

The minimum supported Rust version is 1.89.

## How a grid works on a DEX

The range `[lower, upper]` is cut into `levels` slots. Each slot holds either the **quote** token
(it waits to **buy** when the price falls to its lower line) or the **base** token (it waits to **sell**
when the price rises to its upper line):

```
 price ↑   52 ─┬─ slot 11  holds RA  → SELL when price ≥ 52
               …
           47.6 ┼─ slot 7   holds RA  → SELL when price ≥ 47.6
  now 46.2 ···· │  slot 6   holds TAZ → BUY  when price ≤ 45.6
           45.6 ┼─ slot 5   holds TAZ → BUY  when price ≤ 44.6
               …
           40 ─┴─ slot 0   holds TAZ → BUY  when price ≤ 40
```

When a slot buys, it then waits to sell one line higher. When it sells, it waits to buy one line lower.
Every buy-then-sell earns the gap between two lines, minus the pool fee twice and the price impact.
`saturn-grid plan` simulates this on the pool's real depth and refuses a grid that would not profit.

A DEX has no order book, so the bot watches the pool price. Each tick costs one batched read of the
reserves, the fee and the active flag. When the price crosses a line, the bot swaps.

## Quick start

```bash
git clone https://github.com/Infinite-Star-Studios/SaturnGridBotExample.git
```

```bash
cd SaturnGridBotExample && cargo build --release
```

```bash
cp saturn-grid.example.toml saturn-grid.toml
```

Edit `saturn-grid.toml` (pool, range, order size), then:

```bash
./target/release/saturn-grid plan
```

```bash
./target/release/saturn-grid run
```

`plan` prints every line and slot, the funds the grid needs, the price impact of one order and the
estimated profit per round trip. `run` paper-trades on live prices until you stop it (Ctrl-C, or create
the `STOP` file it names).

To trade for real:

```bash
./target/release/saturn-grid keygen
```

This writes `keys/mainnet.json` (mode 0600) and prints its address. Send that address the grid's tokens
(`plan` lists them), **a little KCAL for gas** (about 0.05 KCAL per swap; at least 0.9 KCAL must be free
when a swap is signed) and **at least 1 SOUL** (Phantasma charges a small data fee in SOUL; Saturn's site
asks for 1 SOUL too). Then check and start:

```bash
./target/release/saturn-grid balances
```

```bash
./target/release/saturn-grid run --live
```

On mainnet it asks you to type `yes`. Without a terminal (systemd, cron) it needs
`SATURN_GRID_ALLOW_MAINNET=1` in its environment. To get the base token the upper slots need, make one
swap with `saturn-grid swap --sell TAZ --amount 20` (it shows the quote and asks first), or swap on
Saturn's site.

Try it on **devnet** first if you can get test tokens: set `network = "devnet"` and pick a devnet pool
(`saturn-grid pools --pair SOUL,KCAL --network devnet`).

## Commands

| command | what it does |
|---|---|
| `plan` | read the pool on chain; show the grid, funds needed, impact and estimated profit |
| `run` | paper-trade on live prices (no key, nothing sent) |
| `run --live` | trade for real from the key file |
| `status [--paper]` | the saved grid: each slot, round trips, realized profit, gas, any swap in flight |
| `balances` | the wallet's liquid balances |
| `quote --sell SYM --amount N` | an exact quote in the configured pool |
| `swap --sell SYM --amount N [--yes]` | one swap in the configured pool (asks first) |
| `keygen [--out FILE]` | a new key file (mode 0600, never overwrites) and its address |
| `address` | the key file's address |
| `pools --pair A,B [--network …]` | Saturn v4 pools of a pair, deepest first (from Saturn's API) |

Stop a running bot cleanly by creating the `STOP` file next to its state file. Stopping it any other
way is safe too (see below).

## Safety

**Keys**
- Keys are read only from a key file (`{"wif": "…"}`, the format of Saturn's P2P bot kit), never from
  arguments, the environment or the config.
- A key file that other users can read is refused, and so is any argument that looks like a WIF.
- Keys are wiped from memory after use and never logged. `Debug` shows only the address.

**Every swap**
- It is quoted from fresh on-chain reserves with the contract's exact math.
- It is sent with `minAmountOut` set by `slippage_bps` (capped at 3%) and a short expiry (`tx_ttl_secs`).
- It is skipped if it would move the pool more than `max_price_impact_bps`.
- The wallet must hold the input, gas KCAL and 1 SOUL before it is signed.

**Crash safety**
- Each swap is saved as *pending* (fsync) before it is broadcast; its hash is computed locally.
- After a crash or kill, the next start finds it on chain by hash before doing anything else.
- Re-broadcasting the same signed bytes is harmless: same hash, executed at most once.
- A swap counts as not executed only when the node says "not found" well after its expiry.

**Grid integrity**
- The state file is locked, so two bots never trade one grid.
- A state made with different grid settings is never resumed silently (the grid settings are fingerprinted).
- A grid whose estimated round trip is below `min_profit_bps`, or whose orders are below Saturn's
  minimum swap, is refused.

**Circuit breakers**
- It stops after `max_consecutive_failures` failed swaps in a row.
- It pauses beyond `max_trades_per_hour`, while the pool is inactive, and while the price is more than
  `stop_outside_range_bps` outside the range.

**Chain and network checks**
- It only talks to allowlisted HTTPS hosts and never follows redirects.
- It refuses to run when the machine's clock is off by more than `max_clock_skew_secs`.
- It refuses to trade unless the contracts report `saturnswap-4.x` and `saturnpools-4.x`, the
  interface it was verified against.
- It refuses a token whose on-chain scale factor does not match its decimals.

**Typos**
- An unknown key in the config is an error.

## Speed

Trading latency is set by the network and the chain, not the CPU.

- **Reads:** the bot reads the chain directly, so it never waits for an indexer (Saturn's API refreshes
  every ~15 s). Each tick is one batched `invokeRawScript` over a kept-alive TLS connection.
- **Trades:** after a fill it re-reads the pool immediately.
- **CPU:** quoting and signing take microseconds.
- **Polling:** `poll_ms` defaults to 1 s and is capped at 4 reads a second because the public RPC is
  shared. Phantasma only makes blocks when there are transactions, so faster polling buys nothing.

## How it was verified

`cargo test` runs 37 tests, including golden vectors:

- **Phantasma encoding.** 24 escrow transactions from Saturn's P2P bot kit (produced by
  `phantasma-sdk-ts`) are rebuilt and signed **byte for byte**, together with their keys, addresses and
  WIFs.
- **The website's swap.** A real mainnet Saturn swap made on the website (100 TAZ → RA, pool v4:33)
  has the same script and hash, and the website's signature verifies over our unsigned bytes.
- **The quote.** The quote reproduces that swap's fill **to the last unit** (2.156190850 RA).
- **Decoding.** VM answers and swap events are decoded from bytes recorded on mainnet.
- **Arithmetic.** The 256-bit arithmetic is checked against big-integer results, and the grid
  invariants hold over a 5,000-step random walk.

Against the live chain (November 2026): `plan`, `quote` and paper `run` on mainnet pool v4:33. A signed
swap from an empty devnet wallet was refused only for lack of gas, which shows the node parsed it and
accepted its signature. A tampered one was refused as "Not signed by gas payer".

## Saturn facts it relies on (checked November 2026)

- **Swap call.** A swap is `saturnswap.swap(from, poolId, amountIn, tokenIn, tokenOut, minAmountOut)`
  with raw amounts.
- **Gas and payload.** The call is wrapped in `AllowGas(from, Null, 100000, 90000)` … `SpendGas(from)`,
  with the payload `SATURN_swap IN>OUT`.
- **Scaling and quote.** `saturnpools` keeps reserves at 8 decimals. The output is
  `⌊net·R_out/(R_in+net)⌋` with `net = in − ⌊in·fee/10000⌋`, scaled back to the output token.
- **RPC.** Mainnet RPC is `https://pharpc1.phantasma.info/rpc` (nexus `mainnet`); devnet is
  `https://devnet.phantasma.info/rpc` (nexus `testnet`).

## Files

```
src/
  phantasma/  base58, keys (WIF, Ed25519, addresses), script builder, transactions, VM decoder, RPC
  saturn.rs   pool reads, exact quote, swap transaction, contract checks
  grid.rs     the strategy (pure logic)
  engine.rs   plan, poll loop, crash-safe execution
  state.rs    atomic, locked state file
  config.rs   validated TOML config
  main.rs     the command line
tests/vectors.rs   golden vectors (SDK and website byte-for-byte checks)
saturn-grid.example.toml
```

`keys/`, `state/` and your `saturn-grid.toml` are git-ignored. Never commit a key.

## Credits

The transaction test vectors come from Saturn's P2P bot kit (MIT, Infinite Star Studios). The bot's
interface to Saturn follows the Saturn Operations API guide (https://apiops.saturnx.cc/llms.txt) and the
contracts' on-chain ABIs.

## License

MIT, see `LICENSE`.
