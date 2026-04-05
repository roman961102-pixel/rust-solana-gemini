# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Solana copy-trading bot written in Rust. Monitors target wallets via gRPC, detects DEX trades (Pump.fun, PumpSwap, Raydium AMM/CPMM), mirrors buy transactions, and auto-sells with TP/SL/trailing stop. Primary language is Chinese in comments and logs.

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

Binary name is `copy-trader` (defined in Cargo.toml `[[bin]]`).

## Configuration

All config via `.env` file (loaded by `dotenvy`). Required env vars: `PRIVATE_KEY`, `TARGET_WALLETS` (comma-separated Pubkeys). See `src/config.rs` for all fields and defaults.

Logging controlled by `RUST_LOG` env var; default filter: `info,solana_copy_trader=debug`.

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

- **`grpc/`** - Yellowstone gRPC subscription. `GrpcSubscriber` for transaction stream, `AccountSubscriber` for real-time bonding curve + ATA balance updates. Two-phase instruction scanning (outer + CPI inner instructions) with ALT address resolution.
- **`processor/`** - `TradeProcessor` trait. Each DEX processor (`pumpfun`, `pumpswap`, `raydium_amm`, `raydium_cpmm`) implements `build_mirror_instructions()` returning `MirrorInstruction`. `PrefetchCache` pre-computes PDAs on detection.
- **`tx/`** - `BlockhashCache` (400ms background refresh, sync read), `TxBuilder` (ComputeBudget + priority fee + optional Jito tip), `TxSender` (multi-channel concurrent: Shyft RPC + Helius RPC + Jito Bundle, fire-and-forget), `BuyConfirmer` (post-send on-chain confirmation), `SellExecutor`.
- **`autosell/`** - `AutoSellManager` with gRPC-driven real-time price monitoring + fallback polling. Position lifecycle: Submitted → Confirming → Confirmed. Sell triggers: take-profit, stop-loss, trailing stop, max hold timeout.
- **`consensus/`** - `ConsensusEngine` using `DashMap`. Per-token signal aggregation with wallet dedup and expiry cleanup.
- **`config.rs`** - `AppConfig` (static, from .env) + `DynConfig` (runtime-mutable via Telegram /set, lock-free atomics).
- **`telegram.rs`** - Telegram bot for runtime control (/start, /stop, /set params) and trade notifications.

### Buy Instruction Priority (3-tier, zero-RPC preferred)

1. Bonding curve cache hit → AMM formula (zero RPC)
2. Target instruction data extrapolation (zero RPC, lower precision)
3. Bonding curve RPC fetch (fallback)

### TX Sending Strategy

Fire-and-forget across multiple channels concurrently (Shyft + optional Helius + optional Jito). When Jito enabled with target tx bytes available, uses backrun bundling for same-block execution.
