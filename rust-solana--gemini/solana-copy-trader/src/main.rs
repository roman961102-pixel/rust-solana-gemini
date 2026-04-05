mod autosell;
mod config;
mod consensus;
mod grpc;
mod processor;
mod telegram;
mod tx;
mod utils;

use anyhow::Result;
use dashmap::DashMap;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use autosell::{AutoSellManager, Position, SellSignal};
use config::{AppConfig, DynConfig};
use consensus::{
    ConsensusEngine,
    engine::{BuySignal, ConsensusTrigger},
};
use grpc::{AccountSubscriber, AccountUpdate, AtaBalanceCache, BondingCurveCache, GrpcSubscriber};
use processor::DetectedTrade;
use processor::pumpfun::PumpfunProcessor;
use processor::prefetch::PrefetchCache;
use telegram::{TgBot, TgEvent, TgNotifier, TgStats};
use tx::{blockhash, builder::TxBuilder, confirm::{BuyConfirmer, format_price_gmgn, format_mcap_usd}, sell_executor::SellExecutor, sender::TxSender};
use utils::sol_price::SolUsdPrice;
use utils::token_info;

type SignatureCache = Arc<DashMap<String, Instant>>;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    info!("==============================================");
    info!("   Solana 跟单交易系统 v1.3.5");
    info!("   gRPC + Pump.fun 直连 | fire-and-forget");
    info!("==============================================");

    let config = AppConfig::from_env()?;
    let dyn_config = DynConfig::from_config(&config);
    info!("钱包地址: {}", config.pubkey);
    info!("跟单目标: {} 个钱包", config.target_wallets.len());
    info!(
        "买入金额: {} SOL | 买入滑点: {} bps | 卖出滑点: {} bps",
        config.buy_sol_amount, config.slippage_bps, config.sell_slippage_bps
    );
    info!(
        "共识条件: {} 个钱包在 {}s 内买入同一 token",
        config.consensus_min_wallets, config.consensus_timeout_secs
    );
    info!(
        "发送通道: Shyft RPC + {} + {} (T+0 全并发)",
        if config.secondary_rpc_url.is_some() {
            "Helius RPC"
        } else {
            "无备用RPC"
        },
        if config.jito_enabled {
            "Jito Bundle + Jito TX"
        } else {
            "无Jito"
        },
    );
    if config.consensus_min_wallets <= 1 {
        info!("⚡ 同区块模式: min_wallets=1, 内联执行 (跳过 channel 跳转)");
    }
    if config.jito_enabled {
        info!(
            "Jito 小费: 买入={} lamports | 卖出={} lamports",
            config.jito_buy_tip_lamports,
            config.jito_sell_tip_lamports,
        );
    }
    info!(
        "自动卖出: {} | 止盈={}% 止损={}% 追踪止损={}% 超时={}s",
        if config.auto_sell_enabled {
            "开启 (gRPC实时价格)"
        } else {
            "关闭"
        },
        config.take_profit_percent,
        config.stop_loss_percent,
        config.trailing_stop_percent,
        config.max_hold_seconds,
    );

    // ============================================
    // 初始化核心组件
    // ============================================
    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        config.rpc_url.clone(),
        solana_sdk::commitment_config::CommitmentConfig::confirmed(),
    ));

    let balance = rpc_client.get_balance(&config.pubkey)?;
    info!("SOL 余额: {:.4} SOL", balance as f64 / 1e9);
    if balance < config.buy_lamports() * 2 {
        warn!("余额不足: 低于买入金额的 2 倍");
    }

    // Blockhash 缓存（400ms 刷新，同步读取零延迟）
    let blockhash_cache = blockhash::init_blockhash_cache(&rpc_client).await?;
    let _bh_task =
        blockhash_cache.start_refresh_task(rpc_client.clone(), Duration::from_millis(400));

    // SOL/USD 汇率（启动时同步获取 + 后台每 30 秒刷新）
    let sol_usd = SolUsdPrice::new();
    sol_usd.init(config.default_sol_usd_price).await;
    let _sol_usd_task = sol_usd.start_refresh_task();

    // gRPC 驱动的实时缓存
    let bc_cache = BondingCurveCache::new();
    let ata_cache = AtaBalanceCache::new();

    // 预取缓存（检测信号时预计算 PDA）
    let prefetch_cache = Arc::new(PrefetchCache::new(bc_cache.clone()));

    // 多通道发送器（4 通道 T+0 全并发 fire-and-forget, VersionedTransaction V0）
    let tx_sender = Arc::new(TxSender::new(
        config.rpc_url.clone(),
        config.secondary_rpc_url.clone(),
        config.jito_block_engine_urls.clone(),
        config.jito_enabled,
    ));

    // Pump.fun 处理器
    let pumpfun = Arc::new(PumpfunProcessor::new(rpc_client.clone()));

    // 共识引擎
    let consensus_engine = Arc::new(ConsensusEngine::new(
        config.consensus_min_wallets,
        config.consensus_timeout_secs,
    ));
    let _cleanup_task = consensus_engine.start_cleanup_task();

    // 自动卖出管理器（gRPC 实时价格驱动 + 确认超时兜底）
    let auto_sell_manager = Arc::new(AutoSellManager::new(
        config.clone(),
        dyn_config.clone(),
        bc_cache.clone(),
        rpc_client.clone(),
        sol_usd.clone(),
    ));

    // 运行控制（TG /start /stop）— 默认停止，需要 TG /start 启动监听
    let is_running = Arc::new(AtomicBool::new(false));
    let tg_stats = Arc::new(TgStats::new());

    // gRPC 账户订阅器（bonding curve + ATA 实时推送）
    // 使用独立的 gRPC URL（RabbitStream 不支持账户订阅）
    let account_subscriber = Arc::new(AccountSubscriber::new(
        config.grpc_account_url.clone(),
        config.grpc_account_token.clone(),
        bc_cache.clone(),
        ata_cache.clone(),
    ));
    if config.grpc_account_url != config.grpc_url {
        info!("账户监控 gRPC: {} (独立于交易流)", config.grpc_account_url);
    }

    // 签名去重
    let sig_cache: SignatureCache = Arc::new(DashMap::new());

    // ============================================
    // Channels
    // ============================================
    let (trade_tx, mut trade_rx) = mpsc::unbounded_channel::<DetectedTrade>();
    let (consensus_tx, mut consensus_rx) = mpsc::unbounded_channel::<ConsensusTrigger>();
    let (sell_signal_tx, mut sell_signal_rx) = mpsc::unbounded_channel::<SellSignal>();
    let (account_update_tx, account_update_rx) = mpsc::unbounded_channel::<AccountUpdate>();

    // 卖出执行器 + Telegram Bot（互相依赖，先创建 notifier 再创建 executor）
    // 先创建 notifier channel（轻量），executor 需要 notifier，bot 需要 executor
    let (tg_event_tx, tg_event_rx) = mpsc::unbounded_channel::<telegram::TgEvent>();
    let tg_notifier = if config.telegram_bot_token.is_some() && config.telegram_chat_id.is_some() {
        TgNotifier::from_sender(tg_event_tx)
    } else {
        TgNotifier::noop()
    };

    let sell_executor = Arc::new(SellExecutor::new(
        config.clone(),
        dyn_config.clone(),
        rpc_client.clone(),
        pumpfun.clone(),
        tx_sender.clone(),
        blockhash_cache.clone(),
        auto_sell_manager.clone(),
        ata_cache.clone(),
        prefetch_cache.clone(),
        account_subscriber.clone(),
        tg_notifier.clone(),
    ));

    let tg_bot = if config.telegram_bot_token.is_some() && config.telegram_chat_id.is_some() {
        let bot = TgBot::from_parts(
            config.clone(),
            dyn_config.clone(),
            auto_sell_manager.clone(),
            consensus_engine.clone(),
            sell_signal_tx.clone(),
            sell_executor.clone(),
            is_running.clone(),
            tg_stats.clone(),
            tg_event_rx,
        );
        info!("Telegram Bot V2 已配置 | chat_id: {}", config.telegram_chat_id.as_deref().unwrap_or(""));
        Some(bot)
    } else {
        info!("Telegram Bot 未配置（设置 TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID 启用）");
        None
    };

    // ============================================
    // 后台任务
    // ============================================

    // 0. Telegram Bot
    if let Some(bot) = tg_bot {
        tokio::spawn(async move { bot.run().await });
    }

    // 1. gRPC 驱动的自动卖出监控（实时价格 + 超时兜底）
    if config.auto_sell_enabled {
        let _grpc_monitor =
            auto_sell_manager.start_grpc_monitor(account_update_rx, sell_signal_tx.clone());
        let _fallback_monitor =
            auto_sell_manager.start_fallback_monitor(sell_signal_tx.clone());
        info!(
            "自动卖出监控已启动 (gRPC 实时价格 + {}s 超时兜底)",
            config.price_check_interval_secs,
        );
    }

    // 2. 签名缓存清理
    let sig_cache_clone = sig_cache.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            sig_cache_clone.retain(|_, t: &mut Instant| t.elapsed() < Duration::from_secs(10));
        }
    });

    // 3. 预取缓存清理
    let prefetch_clone = prefetch_cache.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            prefetch_clone.cleanup(300);
        }
    });

    // 4. gRPC 交易订阅（自动重连）
    let grpc_sub = GrpcSubscriber::new(
        config.grpc_url.clone(),
        config.grpc_token.clone(),
        config.target_wallets.clone(),
    );
    let trade_tx_clone = trade_tx.clone();
    tokio::spawn(async move {
        loop {
            info!("正在连接 gRPC 交易流...");
            match grpc_sub.subscribe(trade_tx_clone.clone()).await {
                Ok(()) => warn!("gRPC 交易流断开，2s 后重连..."),
                Err(e) => error!("gRPC 交易流错误: {}，2s 后重连...", e),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    // 5. gRPC 账户订阅（自动重连）
    let acct_sub_clone = account_subscriber.clone();
    let acct_update_tx_clone = account_update_tx.clone();
    tokio::spawn(async move {
        loop {
            info!("正在连接 gRPC 账户流...");
            match acct_sub_clone.subscribe(acct_update_tx_clone.clone()).await {
                Ok(()) => warn!("gRPC 账户流断开，2s 后重连..."),
                Err(e) => error!("gRPC 账户流错误: {}，2s 后重连...", e),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    // ============================================
    // 卖出信号处理（Pump.fun 直接卖出 + 3 次重试）
    // ============================================
    let sell_exec = sell_executor.clone();
    tokio::spawn(async move {
        while let Some(signal) = sell_signal_rx.recv().await {
            let exec = sell_exec.clone();
            tokio::spawn(async move {
                exec.handle_sell_signal(signal).await;
            });
        }
    });

    // ============================================
    // 共识触发 → Pump.fun 直接买入（仅 min_wallets > 1 时使用）
    // ============================================
    let exec_config = config.clone();
    let exec_rpc = rpc_client.clone();
    let exec_pumpfun = pumpfun.clone();
    let exec_blockhash = blockhash_cache.clone();
    let exec_tx_sender = tx_sender.clone();
    let exec_sol_usd = sol_usd.clone();
    let exec_auto_sell = auto_sell_manager.clone();
    let exec_prefetch = prefetch_cache.clone();
    let exec_bc_cache = bc_cache.clone();
    let exec_ata_cache = ata_cache.clone();
    let exec_acct_sub = account_subscriber.clone();
    let exec_dyn_config = dyn_config.clone();
    let exec_tg = tg_notifier.clone();
    let exec_tg_stats = tg_stats.clone();
    tokio::spawn(async move {
        while let Some(trigger) = consensus_rx.recv().await {
            // TG 推送共识达成
            exec_tg.send(TgEvent::ConsensusReached {
                mint: trigger.token_mint,
                wallets: trigger.wallets.clone(),
            });

            execute_buy(
                &trigger.token_mint,
                &trigger.wallets,
                trigger.triggered_at,
                &[], // 共识模式无目标指令数据
                &[], // 共识模式无目标交易字节
                false, // 共识模式不做 Backrun
                &exec_config,
                &exec_rpc,
                &exec_pumpfun,
                &exec_blockhash,
                &exec_tx_sender,
                &exec_sol_usd,
                &exec_auto_sell,
                &exec_prefetch,
                &exec_bc_cache,
                &exec_ata_cache,
                &exec_acct_sub,
                &exec_dyn_config,
                &exec_tg,
                &exec_tg_stats,
            ).await;
        }
    });

    // ============================================
    // 主循环：检测 → 过滤 → 预取 → 执行/共识
    // ============================================
    let min_buy_lamports = (config.min_target_buy_sol * 1e9) as u64;
    if min_buy_lamports > 0 {
        info!(
            "买入量过滤: 忽略目标钱包 < {} SOL 的买入信号",
            config.min_target_buy_sol,
        );
    }
    let is_instant_mode = config.consensus_min_wallets <= 1;
    if is_running.load(Ordering::Relaxed) {
        info!("主循环已启动，等待 Pump.fun 内盘信号...\n");
    } else {
        info!("主循环已启动（监听暂停，等待 TG /start 命令）...\n");
    }

    // 优雅关闭：Ctrl+C / kill 时发送 TG 下线通知
    let shutdown_token = config.telegram_bot_token.clone();
    let shutdown_chat = config.telegram_chat_id.clone();
    tokio::spawn(async move {
        // 等待 Ctrl+C (SIGINT) 或 SIGTERM
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to create SIGTERM handler");
        #[cfg(unix)]
        tokio::select! {
            _ = ctrl_c => {},
            _ = sigterm.recv() => {},
        };
        #[cfg(not(unix))]
        ctrl_c.await.ok();

        warn!("收到关闭信号，发送 TG 下线通知...");
        if let (Some(token), Some(chat)) = (&shutdown_token, &shutdown_chat) {
            telegram::send_shutdown_notification(token, chat).await;
        }
        info!("下线通知已发送，退出进程");
        std::process::exit(0);
    });

    while let Some(trade) = trade_rx.recv().await {
        // TG /stop 控制：停止处理新交易
        if !is_running.load(Ordering::Relaxed) {
            continue;
        }

        // gRPC 事件计数
        tg_stats.grpc_events.fetch_add(1, Ordering::Relaxed);

        // 去重
        if sig_cache.contains_key(&trade.signature) {
            continue;
        }
        sig_cache.insert(trade.signature.clone(), Instant::now());

        // 只处理买入
        if !trade.is_buy {
            continue;
        }

        // 买入量过滤（纯本地）
        if min_buy_lamports > 0 && trade.sol_amount_lamports > 0 {
            if trade.sol_amount_lamports < min_buy_lamports {
                debug!(
                    "跳过小额买入: {}.. 买入 {:.4} SOL < 阈值 {} SOL",
                    &trade.source_wallet.to_string()[..8],
                    trade.sol_amount_lamports as f64 / 1e9,
                    config.min_target_buy_sol,
                );
                continue;
            }
        }

        // 纯本地提取 mint + token_program
        let (token_mint, token_program) = match extract_token_info(&trade) {
            Some(info) => info,
            None => {
                debug!("无法从 accountKeys 提取 mint，跳过");
                continue;
            }
        };

        // 黑名单检查
        if dyn_config.is_blocked(&token_mint) {
            debug!("跳过黑名单代币: {}", &token_mint.to_string()[..12]);
            continue;
        }

        // 预取 PDA + 注册 gRPC bonding curve 监控
        let prefetched = prefetch_cache.prefetch_token(
            &token_mint,
            &token_program,
            &trade.instruction_accounts,
            &trade.source_wallet,
            &config,
        );
        account_subscriber.track_bonding_curve(token_mint, prefetched.bonding_curve);

        // ⚡ 同区块模式: min_wallets=1 时直接内联执行，跳过 channel 跳转
        if is_instant_mode {
            // 不再等待 BC RPC 预取！
            // 优先用目标指令数据推算价格（零 RPC），BC cache 作为补充
            if bc_cache.get(&token_mint).is_none() {
                // 后台异步预取 BC（不阻塞，给后续持仓监控用）
                let pf = pumpfun.clone();
                let bc = prefetched.bonding_curve;
                let mint_copy = token_mint;
                let bc_cache_copy = bc_cache.clone();
                tokio::spawn(async move {
                    if let Ok(state) = pf.prefetch_bonding_curve(&bc).await {
                        bc_cache_copy.update(&mint_copy, state);
                    }
                });
            }

            // 内联执行买入（跳过共识 channel 跳转）
            let wallets = vec![trade.source_wallet];
            execute_buy(
                &token_mint,
                &wallets,
                trade.detected_at,
                &trade.instruction_data,
                &trade.raw_transaction_bytes,
                trade.is_pre_execution,
                &config,
                &rpc_client,
                &pumpfun,
                &blockhash_cache,
                &tx_sender,
                &sol_usd,
                &auto_sell_manager,
                &prefetch_cache,
                &bc_cache,
                &ata_cache,
                &account_subscriber,
                &dyn_config,
                &tg_notifier,
                &tg_stats,
            ).await;
        } else {
            // 多钱包共识模式：后台预取 BC，提交共识
            if bc_cache.get(&token_mint).is_none() {
                let pf = pumpfun.clone();
                let bc = prefetched.bonding_curve;
                let mint_copy = token_mint;
                let bc_cache_copy = bc_cache.clone();
                tokio::spawn(async move {
                    if let Ok(state) = pf.prefetch_bonding_curve(&bc).await {
                        bc_cache_copy.update(&mint_copy, state);
                        debug!(
                            "并行预取 bonding curve 完成: {}",
                            &mint_copy.to_string()[..12],
                        );
                    }
                });
            }

            consensus_engine.submit_signal(
                BuySignal {
                    token_mint,
                    wallet: trade.source_wallet,
                    detected_at: Instant::now(),
                    signature: trade.signature.clone(),
                },
                &consensus_tx,
            );
        }
    }

    warn!("交易通道关闭，系统退出");
    Ok(())
}

// ============================================
// 买入执行（内联 + 共识通用）
// ============================================

/// 核心买入执行逻辑
/// 构建优先级: 目标指令推算(零RPC) → BC cache(零RPC) → BC RPC(兜底)
#[allow(clippy::too_many_arguments)]
async fn execute_buy(
    mint: &Pubkey,
    wallets: &[Pubkey],
    detected_at: Instant,
    target_instruction_data: &[u8],
    target_raw_tx: &[u8],
    is_pre_execution: bool,
    config: &AppConfig,
    rpc_client: &Arc<RpcClient>,
    pumpfun: &Arc<PumpfunProcessor>,
    blockhash_cache: &blockhash::BlockhashCache,
    tx_sender: &Arc<TxSender>,
    sol_usd: &SolUsdPrice,
    auto_sell_manager: &Arc<AutoSellManager>,
    prefetch_cache: &Arc<PrefetchCache>,
    bc_cache: &BondingCurveCache,
    ata_cache: &AtaBalanceCache,
    account_subscriber: &Arc<AccountSubscriber>,
    dyn_config: &Arc<DynConfig>,
    tg: &TgNotifier,
    tg_stats: &Arc<TgStats>,
) {
    let start = Instant::now();
    let detect_to_exec = detected_at.elapsed();

    info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    info!(
        "执行跟单: {} | {} 个钱包 | 检测→执行: {:?}",
        &mint.to_string()[..12],
        wallets.len(),
        detect_to_exec,
    );

    // 使用预取数据
    let prefetched = prefetch_cache.get(mint);

    // 从 DynConfig 读取动态参数
    let buy_sol = dyn_config.buy_sol_amount();
    let buy_lamports = dyn_config.buy_lamports();
    let sol_price = sol_usd.get();

    // ===== 2 层优先级构建买入指令（零 RPC，无兜底）=====
    // 1. 目标指令推算（零 RPC，从目标钱包指令数据推算价格，最快）
    // 2. BC cache 命中（零 RPC，AMM 公式精确计算）
    // 无 BC RPC fetch — 任何 RPC 调用都会毁掉同区块机会
    let buy_result: Result<(processor::MirrorInstruction, u64), anyhow::Error> = if let Some(ref pf) = prefetched {
        if pf.mirror_accounts.is_empty() {
            Err(anyhow::anyhow!("无 mirror_accounts，无法构建指令"))
        } else if target_instruction_data.len() >= 24 {
            // ⚡ 最快: 从目标指令数据推算价格（零 RPC，零缓存依赖）
            pumpfun.buy_from_target_instruction(
                mint, &pf.user_ata, &pf.token_program, &pf.source_wallet,
                &pf.mirror_accounts, target_instruction_data, config,
            )
        } else if let Some(bc_state) = bc_cache.get(mint) {
            // 次选: BC cache 命中，用 AMM 公式精确计算
            let token_amount = bc_state.sol_to_token_quote(buy_lamports);
            pumpfun.buy_from_cached_state(
                mint, &pf.user_ata, &pf.token_program, &pf.source_wallet,
                &pf.mirror_accounts, &bc_state, config,
            ).map(|mirror| (mirror, token_amount))
        } else {
            Err(anyhow::anyhow!("无目标指令数据且 BC 缓存未命中，跳过（不做 RPC 兜底）"))
        }
    } else {
        Err(anyhow::anyhow!("无预取数据，跳过"))
    };

    // 阶段一入场价: buyAmountSol / (estimatedTokens / 1e6)
    let (estimated_tokens_raw, entry_price_sol, entry_mcap_sol) = match &buy_result {
        Ok((_, est_tokens)) if *est_tokens > 0 => {
            let display_tokens = *est_tokens as f64 / 1e6;
            let price = if display_tokens > 0.0 { buy_sol / display_tokens } else { 0.0 };
            let mcap = if let Some(bc_state) = bc_cache.get(mint) {
                bc_state.market_cap_sol()
            } else {
                // 从目标指令推算市值: price × 10 亿
                price * processor::pumpfun::PUMP_TOTAL_SUPPLY
            };
            (*est_tokens, price, mcap)
        }
        _ => {
            if let Some(bc_state) = bc_cache.get(mint) {
                let out = bc_state.sol_to_token_quote(buy_lamports);
                let display = out as f64 / 1e6;
                let price = if display > 0.0 { buy_sol / display } else { 0.0 };
                (out, price, bc_state.market_cap_sol())
            } else {
                (0, 0.0, 0.0)
            }
        }
    };

    let entry_price_usd = entry_price_sol * sol_price;
    let entry_mcap_usd = entry_mcap_sol * sol_price;

    let mut position = Position::new(
        *mint, buy_lamports, entry_price_sol, wallets[0],
    );

    match buy_result {
        Ok((mirror, _)) => {
            let build_elapsed = start.elapsed();
            info!("指令构建: {:?}", build_elapsed);

            // ⚡ 同步读取 blockhash（零 await 开销）
            let (blockhash, _) = blockhash_cache.get_sync();

            let tx_result = if config.jito_enabled {
                let tip = tx_sender.random_jito_tip_account();
                TxBuilder::build_jito_bundle_transaction(
                    &mirror,
                    config,
                    &config.keypair,
                    blockhash,
                    &tip,
                    dyn_config.jito_buy_tip_lamports(),
                    &[], // ALT: 暂无预部署，后续可接入
                )
            } else {
                TxBuilder::build_transaction(
                    &mirror,
                    config,
                    &config.keypair,
                    blockhash,
                    &[], // ALT: 暂无预部署，后续可接入
                )
            };

            match tx_result {
                Ok(transaction) => {
                    // ⚡ fire-and-forget 发送策略:
                    // 预执行推送 + 有目标tx + Jito 开启 → Backrun Bundle（同区块）
                    // 已执行推送（Processed）→ 普通多通道发送（Backrun 必失败）
                    let send_result = if is_pre_execution && !target_raw_tx.is_empty() && config.jito_enabled {
                        tx_sender.fire_and_forget_backrun(target_raw_tx, &transaction)
                    } else {
                        tx_sender.fire_and_forget(&transaction)
                    };
                    match send_result {
                        Ok(sig) => {
                            let elapsed = start.elapsed();
                            let sig_str = sig.to_string();

                            let buy_usd = sol_usd.sol_to_usd(buy_sol);

                            // 后台获取代币名称
                            let rpc_for_info = rpc_client.clone();
                            let mint_for_info = *mint;
                            tokio::spawn(async move {
                                let ti = token_info::fetch_token_info(&rpc_for_info, &mint_for_info).await;
                                info!("代币名称: {} ({})", ti.name, ti.symbol);
                            });

                            let total_latency = detected_at.elapsed();
                            info!(
                                "⚡ ���入已发射 | {:.4} SOL (${:.2}) | 预估 {:.0} tokens | 成本价(预估): {} | 市值(预估): {} | 构建: {:?} | 全链路: {:?} | sig: {}",
                                buy_sol,
                                buy_usd,
                                estimated_tokens_raw as f64 / 1e6,
                                format_price_gmgn(entry_price_usd),
                                format_mcap_usd(entry_mcap_usd),
                                elapsed,
                                total_latency,
                                if sig_str.len() > 16 { &sig_str[..16] } else { &sig_str },
                            );

                            // TG 推送买入提交 + 统计
                            tg_stats.buy_attempts.fetch_add(1, Ordering::Relaxed);
                            tg.send(TgEvent::BuySubmitted {
                                mint: *mint,
                                sol_amount: buy_sol,
                                latency_ms: total_latency.as_millis() as u64,
                            });

                            // 立即建仓 + 注册 gRPC 监控
                            if config.auto_sell_enabled {
                                position.mark_submitted(sig_str);
                                position.mark_confirming();
                                auto_sell_manager.add_position(position.clone());

                                // 使用 prefetch 的正确 ATA（兼容 Token-2022）
                                let user_ata = if let Some(ref pf) = prefetched {
                                    account_subscriber
                                        .track_bonding_curve(*mint, pf.bonding_curve);
                                    pf.user_ata
                                } else {
                                    let program_id = Pubkey::from_str(
                                        "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",
                                    )
                                    .unwrap();
                                    let (bc, _) = Pubkey::find_program_address(
                                        &[b"bonding-curve", mint.as_ref()],
                                        &program_id,
                                    );
                                    account_subscriber.track_bonding_curve(*mint, bc);
                                    get_associated_token_address(&config.pubkey, mint)
                                };
                                account_subscriber.track_ata(*mint, user_ata);

                                // 🔍 主动链上确认：轮询签名状态 → 查 ATA 余额 → 修正真实价格
                                BuyConfirmer::spawn_confirm_task(
                                    rpc_client.clone(),
                                    auto_sell_manager.clone(),
                                    bc_cache.clone(),
                                    ata_cache.clone(),
                                    sol_usd.clone(),
                                    *mint,
                                    sig,
                                    config.pubkey,
                                    buy_lamports,
                                    user_ata,
                                    estimated_tokens_raw,
                                    tg.clone(),
                                );

                                info!(
                                    "持仓已记录 (CONFIRMING) | 链上确认已启动 | 当前: {}",
                                    auto_sell_manager.position_count(),
                                );
                            }
                        }
                        Err(e) => error!("Fire-and-forget 发送错误: {}", e),
                    }
                }
                Err(e) => error!("交易构建失败: {}", e),
            }
        }
        Err(e) => error!("Pump.fun 内盘不可用: {} (跳过此交易)", e),
    }

    info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
}

// ============================================
// Token Mint 提取（纯本地，零 RPC）
// ============================================

/// 从 Pump.fun 指令 accountKeys 提取 mint (index 2) 和 token_program (index 8)
fn extract_token_info(trade: &DetectedTrade) -> Option<(Pubkey, Pubkey)> {
    if trade.instruction_accounts.len() >= 9 {
        let mint = trade.instruction_accounts[2];
        let token_program = trade.instruction_accounts[8];
        if !utils::ata::is_system_address(&mint) {
            return Some((mint, token_program));
        }
    }
    None
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,solana_copy_trader=debug".into()),
        )
        .with_target(false)
        .with_thread_ids(false)
        .with_ansi(true)
        .init();
}
