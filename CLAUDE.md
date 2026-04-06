# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Solana copy-trading bot written in Rust. Monitors target wallets via gRPC (RabbitStream pre-execution), detects Pump.fun trades, mirrors buy transactions with same-block Jito Backrun Bundles, and auto-sells with TP/SL/trailing stop. Primary language is Chinese in comments and logs.

## Build & Run

```bash
# All commands run from: rust-solana--gemini/solana-copy-trader/
cd rust-solana--gemini/solana-copy-trader

cargo build              # Debug build
cargo build --release    # Release build (opt-level=3, LTO, single codegen unit)
cargo run                # Debug run
cargo run --release      # Production run

cargo check              # Fast type-check without codegen
cargo clippy             # Linting
```

Binary name is `copy-trader` (defined in Cargo.toml `[[bin]]`). CI builds on push to main, artifact: `copy-trader-linux`.

## Configuration

All config via `.env` file (loaded by `dotenvy`). Required env vars: `PRIVATE_KEY`, `TARGET_WALLETS` (comma-separated Pubkeys). See `src/config.rs` for all fields and defaults.

Key optional env vars:
- `JITO_AUTH_UUID` — Jito authentication UUID (`uuidgen` to generate). Without it, Jito rate limits are extremely low.
- `JITO_BLOCK_ENGINE_URL` — Comma-separated Jito endpoints. `bundles.jito.wtf` auto-appended as relay.
- `RUST_LOG` — Logging filter; default: `info,solana_copy_trader=debug`.

## Architecture

Single-binary async application on Tokio. The crate root is `rust-solana--gemini/solana-copy-trader/`.

### Data Flow

```
gRPC stream → DetectedTrade → signature dedup → extract mint → consensus engine → execute_buy → fire-and-forget TX
                                                                                      ↓
                                                                              AutoSellManager → SellSignal → SellExecutor
```

### Two Operating Modes

- **Instant mode** (`CONSENSUS_MIN_WALLETS=1`): Inline buy execution on first signal, skips consensus channel hop for lowest latency.
- **Consensus mode** (`CONSENSUS_MIN_WALLETS>1`): Collects signals from N distinct wallets within a time window before triggering.

### Key Modules

- **`grpc/`** - Yellowstone gRPC subscription. `GrpcSubscriber` for transaction stream (RabbitStream pre-execution push), `AccountSubscriber` for real-time bonding curve + ATA balance updates. Two-phase instruction scanning (outer + CPI inner instructions) with ALT address resolution. Meta presence detection: `meta=None` → pre-execution (Backrun viable), `meta=Some` → Processed level.
- **`processor/`** - `TradeProcessor` trait. Each DEX processor (`pumpfun`, `pumpswap`, `raydium_amm`, `raydium_cpmm`) implements `build_mirror_instructions()` returning `MirrorInstruction`. `PrefetchCache` pre-computes PDAs on detection.
- **`tx/`** - `BlockhashCache` (400ms background refresh, sync read), `TxBuilder` (VersionedTransaction V0 with ALT support + ComputeBudget + priority fee + optional Jito tip), `TxSender` (HTTP JSON-RPC multi-channel: Shyft RPC + Helius RPC + Jito Bundle/TX with auth + endpoint rotation, fire-and-forget), `BuyConfirmer` (post-send on-chain confirmation), `SellExecutor`.
- **`autosell/`** - `AutoSellManager` with gRPC-driven real-time price monitoring + fallback polling. Position lifecycle: Submitted → Confirming → Confirmed. Sell triggers: take-profit, stop-loss, trailing stop, max hold timeout.
- **`consensus/`** - `ConsensusEngine` using `DashMap`. Per-token signal aggregation with wallet dedup and expiry cleanup.
- **`config.rs`** - `AppConfig` (static, from .env) + `DynConfig` (runtime-mutable via Telegram /set, lock-free atomics).
- **`telegram.rs`** - Telegram bot for runtime control (/start, /stop, /set params) and trade notifications.

### Buy Instruction Priority (2-tier, zero-RPC only)

1. Target instruction data extrapolation (zero RPC, zero cache dependency, fastest)
2. Bonding curve cache hit → AMM formula (zero RPC)

No RPC fallback — any RPC call on the buy hot path destroys same-block opportunity.

### TX Building

All transactions use `VersionedTransaction` with V0 messages (`v0::Message::try_compile`). `TxBuilder` accepts `&[AddressLookupTableAccount]` for future ALT support. Currently passes `&[]` (no ALTs deployed yet).

### TX Sending Strategy

- **Pre-execution detected** (`is_pre_execution=true`, meta absent): Jito Backrun Bundle `[target_tx, our_tx]` for same-block execution, plus RPC direct send as fallback.
- **Processed level** (`is_pre_execution=false`, meta present): Fire-and-forget across RPC channels only (Backrun impossible, target TX already on-chain).
- All Jito requests include `x-jito-auth` header when `JITO_AUTH_UUID` configured.
- Jito endpoints rotate via atomic counter (`next_jito_url_pair()`), `bundles.jito.wtf` relay auto-appended.
- `TxSender` uses HTTP JSON-RPC directly (no `RpcClient`) for VersionedTransaction compatibility.

### Latency-Critical Design Rules

- **Zero RPC on buy hot path** — no `await` between detection and tx send except `tokio::spawn`.
- **Blockhash pre-cached** — `get_sync()` reads from `RwLock` (~50ns), background refresh every 400ms.
- **DynConfig lock-free** — all runtime params via `AtomicU64` with bit-cast for f64.
- **Lazy serialization** — `raw_transaction_bytes` only serialized after instruction match (not for every gRPC message).
- **Fire-and-forget** — tx send returns immediately, confirmation runs in background task.
