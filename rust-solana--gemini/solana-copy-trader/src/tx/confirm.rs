use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::autosell::AutoSellManager;
use crate::grpc::{AtaBalanceCache, BondingCurveCache};
use crate::telegram::{TgEvent, TgNotifier};
use crate::utils::sol_price::SolUsdPrice;


/// 主动链上确认器
/// 买入 fire-and-forget 后立即启动，主动轮询确认状态
/// 确认后获取真实代币数量，修正 entry_price，启动自动卖出监控
pub struct BuyConfirmer;

/// GMGN 风格价格格式化: $0.0₅3525
pub fn format_price_gmgn(usd: f64) -> String {
    if usd <= 0.0 {
        return "$0".to_string();
    }
    if usd >= 0.01 {
        return format!("${:.4}", usd);
    }
    // GMGN 风格: $0.0₅3525 表示小数点后有 5 个零再跟有效数字
    let s = format!("{:.15}", usd);
    if let Some(dot_pos) = s.find('.') {
        let decimals = &s[dot_pos + 1..];
        let leading_zeros = decimals.chars().take_while(|&c| c == '0').count();
        if leading_zeros > 0 {
            let significant = &decimals[leading_zeros..];
            let sig_digits: String = significant.chars().take(4).collect();
            // $0.0₅3525 — 下标数字表示 0 的个数
            return format!("$0.0{}{}", to_subscript(leading_zeros), sig_digits);
        }
    }
    format!("${:.6}", usd)
}

/// 市值格式化: $3.12K, $1.25M, $420
pub fn format_mcap_usd(usd: f64) -> String {
    if usd >= 1_000_000.0 {
        format!("${:.2}M", usd / 1_000_000.0)
    } else if usd >= 1_000.0 {
        format!("${:.2}K", usd / 1_000.0)
    } else {
        format!("${:.0}", usd)
    }
}

/// 数字转下标 Unicode
fn to_subscript(n: usize) -> String {
    let subscripts = ['₀', '₁', '₂', '₃', '₄', '₅', '₆', '₇', '₈', '₉'];
    if n < 10 {
        subscripts[n].to_string()
    } else {
        n.to_string()
            .chars()
            .map(|c| subscripts[c.to_digit(10).unwrap_or(0) as usize])
            .collect()
    }
}

impl BuyConfirmer {
    /// 启动后台确认任务（目标 400-700ms 完成）
    ///
    /// 快速路径: gRPC AtaBalanceCache 已有实时推送，直接读取（0ms）
    /// 中速路径: RPC 轮询 ATA 余额，80ms 间隔（最多 ~640ms）
    /// 慢速路径: getTransaction 提取 post_token_balances（回退）
    ///
    /// 签名确认和余额查询并行，不串行等待
    pub fn spawn_confirm_task(
        rpc_client: Arc<RpcClient>,
        auto_sell: Arc<AutoSellManager>,
        bc_cache: BondingCurveCache,
        ata_cache: AtaBalanceCache,
        sol_usd: SolUsdPrice,
        mint: Pubkey,
        signature: Signature,
        user_pubkey: Pubkey,
        entry_sol_amount: u64,
        user_ata: Pubkey,
        estimated_tokens_raw: u64,
        tg: TgNotifier,
    ) {
        tokio::spawn(async move {
            let start = Instant::now();
            let mint_short = &mint.to_string()[..12];
            let buy_sol = entry_sol_amount as f64 / 1e9;

            info!(
                "链上确认启动: {} | sig: {}",
                mint_short,
                &signature.to_string()[..16],
            );

            // ============================================
            // 统一轮询: 签名确认 + ATA 余额并行检测
            // 每 80ms 同时检查签名状态和 ATA 余额
            // 目标 400-700ms 内完成
            // ============================================
            let mut confirmed = false;
            let mut token_balance: u64 = 0;
            let max_wait = Duration::from_secs(10);
            let poll_interval = Duration::from_millis(80);

            while start.elapsed() < max_wait {
                // --- 快速路径: 先查 gRPC 缓存（无网络开销） ---
                if token_balance == 0 {
                    if let Some(cached_balance) = ata_cache.get(&mint) {
                        if cached_balance > 0 {
                            token_balance = cached_balance;
                            info!(
                                "链上确认: {} gRPC 缓存命中 ATA 余额: {} | {:.0}ms",
                                mint_short, cached_balance, start.elapsed().as_millis(),
                            );
                        }
                    }
                }

                // --- 签名状态检查 ---
                if !confirmed {
                    let rpc = rpc_client.clone();
                    let sig = signature;
                    let status = tokio::task::spawn_blocking(move || {
                        rpc.get_signature_statuses(&[sig])
                    })
                    .await;

                    match status {
                        Ok(Ok(response)) => {
                            if let Some(Some(status)) = response.value.first() {
                                if status.err.is_some() {
                                    warn!(
                                        "链上确认: {} 交易失败 | err: {:?} | {:.0}ms",
                                        mint_short, status.err, start.elapsed().as_millis(),
                                    );
                                    auto_sell.confirm_failed(&mint, "tx failed on-chain");
                                    tg.send(TgEvent::BuyFailed {
                                        mint,
                                        reason: format!("链上交易失败: {:?}", status.err),
                                    });
                                    return;
                                }
                                confirmed = true;
                                debug!(
                                    "链上确认: {} 签名已确认 | {:.0}ms",
                                    mint_short, start.elapsed().as_millis(),
                                );
                            }
                        }
                        Ok(Err(e)) => {
                            debug!("链上确认 RPC 错误: {} ({})", e, mint_short);
                        }
                        Err(e) => {
                            debug!("链上确认任务错误: {} ({})", e, mint_short);
                        }
                    }
                }

                // --- 再查一次 gRPC 缓存（签名确认期间可能已推送） ---
                if token_balance == 0 {
                    if let Some(cached_balance) = ata_cache.get(&mint) {
                        if cached_balance > 0 {
                            token_balance = cached_balance;
                            info!(
                                "链上确认: {} gRPC 缓存命中 ATA 余额: {} | {:.0}ms",
                                mint_short, cached_balance, start.elapsed().as_millis(),
                            );
                        }
                    }
                }

                // --- 签名已确认后: RPC 查 ATA 余额 ---
                if confirmed && token_balance == 0 {
                    let rpc = rpc_client.clone();
                    let ata = user_ata;
                    let ata_result = tokio::task::spawn_blocking(move || {
                        rpc.get_token_account_balance(&ata)
                    })
                    .await;

                    match &ata_result {
                        Ok(Ok(balance)) => {
                            let parsed = balance.amount.parse::<u64>().unwrap_or(0);
                            if parsed > 0 {
                                token_balance = parsed;
                                info!(
                                    "链上确认: {} RPC ATA 余额: {} | {:.0}ms",
                                    mint_short, parsed, start.elapsed().as_millis(),
                                );
                            }
                        }
                        Ok(Err(e)) => {
                            debug!(
                                "链上确认: {} ATA RPC 查询失败: {} | {:.0}ms",
                                mint_short, e, start.elapsed().as_millis(),
                            );
                        }
                        Err(e) => {
                            debug!("链上确认: {} ATA 任务错误: {}", mint_short, e);
                        }
                    }
                }

                // --- 两个条件都满足，退出循环 ---
                if confirmed && token_balance > 0 {
                    break;
                }

                tokio::time::sleep(poll_interval).await;
            }

            // ============================================
            // 回退: 签名已确认但 ATA 余额仍为 0
            // 从 getTransaction post_token_balances 提取
            // ============================================
            if confirmed && token_balance == 0 {
                warn!(
                    "链上确认: {} 签名已确认但 ATA 余额为 0 | {:.0}ms | 尝试 getTransaction",
                    mint_short, start.elapsed().as_millis(),
                );

                let rpc2 = rpc_client.clone();
                let sig2 = signature;
                let tx_detail = tokio::task::spawn_blocking(move || {
                    rpc2.get_transaction(
                        &sig2,
                        solana_transaction_status::UiTransactionEncoding::JsonParsed,
                    )
                }).await;

                match tx_detail {
                    Ok(Ok(tx)) => {
                        if let Some(meta) = &tx.transaction.meta {
                            use solana_transaction_status::option_serializer::OptionSerializer;
                            if let OptionSerializer::Some(ref token_balances) = meta.post_token_balances {
                                let user_str = user_pubkey.to_string();
                                for tb in token_balances.iter() {
                                    let owner_match = match &tb.owner {
                                        OptionSerializer::Some(owner) => owner == &user_str,
                                        _ => false,
                                    };
                                    if owner_match {
                                        if let Ok(amount) = tb.ui_token_amount.amount.parse::<u64>() {
                                            if amount > 0 {
                                                token_balance = amount;
                                                info!(
                                                    "链上确认: {} 从交易详情提取余额: {} | {:.0}ms",
                                                    mint_short, token_balance, start.elapsed().as_millis(),
                                                );
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => warn!("链上确认: {} getTransaction 失败: {}", mint_short, e),
                    Err(e) => warn!("链上确认: {} getTransaction 任务错误: {}", mint_short, e),
                }
            }

            // ============================================
            // 阶段3: 处理结果
            // ============================================
            if token_balance > 0 {
                let display_tokens = token_balance as f64 / 1e6;

                // 当前市场价（从 bonding curve）
                let bc_price = bc_cache.get(&mint).map(|s| s.price_sol()).filter(|p| *p > 0.0);
                let current_market_price = bc_price.unwrap_or(0.0);

                // 入场成本价：先用本地数据立即估算，不等 RPC
                // BC 现货价最精确（零 RPC），回退到 buy_sol / tokens
                let entry_price = if let Some(p) = bc_price {
                    p
                } else if display_tokens > 0.0 {
                    buy_sol / display_tokens
                } else {
                    0.0
                };

                // 滑点: (预估数量 - 实际数量) / 预估数量 × 100
                let estimated_display = estimated_tokens_raw as f64 / 1e6;
                let slippage_pct = if estimated_display > 0.0 {
                    (estimated_display - display_tokens) / estimated_display * 100.0
                } else {
                    0.0
                };

                // PnL（刚确认时 entry ≈ current，PnL ≈ 0）
                let pnl = if entry_price > 0.0 && current_market_price > 0.0 {
                    ((current_market_price - entry_price) / entry_price) * 100.0
                } else {
                    0.0
                };

                let sol_price_usd = sol_usd.get();
                let value_sol = buy_sol * (1.0 + pnl / 100.0);
                let value_usd = value_sol * sol_price_usd;
                let cost_usd = format_price_gmgn(entry_price * sol_price_usd);

                // 市值: price × 10 亿总供应量
                let mcap_sol = current_market_price * crate::processor::pumpfun::PUMP_TOTAL_SUPPLY;
                let mcap_usd = mcap_sol * sol_price_usd;

                // 调用 mark_active，传入真实入场价
                let entry_price_for_position = if entry_price > 0.0 { Some(entry_price) } else { bc_price };
                auto_sell.confirm_success(&mint, token_balance, entry_price_for_position);

                let mcap_str = format_mcap_usd(mcap_usd);

                info!(
                    "✅ 持仓确认: {} | {:.0} tokens | 成本: {} | 市值: {} | PnL: {:.2}% | 滑点: {:.1}% | 价值: {:.4} SOL (${:.2}) | {:.0}ms",
                    mint_short,
                    display_tokens,
                    cost_usd,
                    mcap_str,
                    pnl,
                    slippage_pct,
                    value_sol,
                    value_usd,
                    start.elapsed().as_millis(),
                );

                // TG 推送买入确认
                tg.send(TgEvent::BuyConfirmed {
                    mint,
                    token_name: format!("{}..{}", &mint.to_string()[..6], &mint.to_string()[mint.to_string().len() - 4..]),
                    spent_sol: buy_sol,
                    cost_price_usd: cost_usd.clone(),
                    mcap_usd: mcap_str,
                });

                // 后台异步修正入场价（不阻塞确认流程）
                // 从链上 getTransaction 提取真实 SOL 花费，修正 entry_price
                let rpc_bg = rpc_client.clone();
                let auto_sell_bg = auto_sell.clone();
                let display_tokens_bg = display_tokens;
                tokio::spawn(async move {
                    if let Some(actual_sol) = Self::get_actual_sol_spent(
                        &rpc_bg, signature, &user_pubkey,
                    ).await {
                        let real_price = if display_tokens_bg > 0.0 {
                            actual_sol / display_tokens_bg
                        } else {
                            0.0
                        };
                        if real_price > 0.0 {
                            auto_sell_bg.update_entry_price(&mint, real_price);
                            debug!(
                                "入场价修正: {} | {:.10} SOL/token",
                                &mint.to_string()[..12], real_price,
                            );
                        }
                    }
                });
            } else if confirmed {
                warn!(
                    "链上确认: {} 交易已确认但余额为 0 → 删除仓位 | {:.0}ms",
                    mint_short, start.elapsed().as_millis(),
                );
                auto_sell.confirm_failed(&mint, "confirmed but zero balance");
                tg.send(TgEvent::BuyFailed {
                    mint,
                    reason: "交易已确认但余额为 0".to_string(),
                });
            } else {
                warn!(
                    "链上确认: {} 超时且余额为 0 → 删除仓位 | {:.0}ms",
                    mint_short, start.elapsed().as_millis(),
                );
                auto_sell.confirm_failed(&mint, "timeout with zero balance");
                tg.send(TgEvent::BuyFailed {
                    mint,
                    reason: "确认超时且余额为 0".to_string(),
                });
            }
        });
    }

    /// 从链上交易的 pre/post SOL 余额差计算实际花费的 SOL
    async fn get_actual_sol_spent(
        rpc_client: &Arc<RpcClient>,
        signature: Signature,
        user_pubkey: &Pubkey,
    ) -> Option<f64> {
        let rpc = rpc_client.clone();
        let sig = signature;
        let user = *user_pubkey;

        let result = tokio::task::spawn_blocking(move || {
            rpc.get_transaction(
                &sig,
                solana_transaction_status::UiTransactionEncoding::Json,
            )
        })
        .await;

        match result {
            Ok(Ok(tx)) => {
                let meta = tx.transaction.meta.as_ref()?;
                // 找到 user 在 account_keys 中的 index
                let account_keys: Vec<String> = match &tx.transaction.transaction {
                    solana_transaction_status::EncodedTransaction::Json(ui_tx) => {
                        match &ui_tx.message {
                            solana_transaction_status::UiMessage::Raw(msg) => {
                                msg.account_keys.clone()
                            }
                            solana_transaction_status::UiMessage::Parsed(msg) => {
                                msg.account_keys.iter().map(|k| k.pubkey.clone()).collect()
                            }
                        }
                    }
                    _ => return None,
                };

                let user_str = user.to_string();
                let user_idx = account_keys.iter().position(|k| k == &user_str)?;

                let pre_balance = meta.pre_balances.get(user_idx)?;
                let post_balance = meta.post_balances.get(user_idx)?;

                // 计算实际花在代币上的 SOL（和 GMGN 一致）
                // total_spent = pre - post
                // token_cost = total_spent - fee - ATA_rent(如有) - jito_tip
                if pre_balance > post_balance {
                    let total_spent = pre_balance - post_balance;
                    let fee = meta.fee;

                    // 检测 ATA 租金: 如果 pre_token_balances 中没有用户的 token 账户
                    // 但 post 中有，说明 ATA 是新创建的
                    const ATA_RENT: u64 = 2039280;
                    let mut deductions = fee;

                    use solana_transaction_status::option_serializer::OptionSerializer;
                    let has_new_ata = if let (
                        OptionSerializer::Some(ref pre_tb),
                        OptionSerializer::Some(ref post_tb),
                    ) = (&meta.pre_token_balances, &meta.post_token_balances) {
                        let user_str = user.to_string();
                        let pre_has_user = pre_tb.iter().any(|tb| {
                            matches!(&tb.owner, OptionSerializer::Some(o) if o == &user_str)
                        });
                        let post_has_user = post_tb.iter().any(|tb| {
                            matches!(&tb.owner, OptionSerializer::Some(o) if o == &user_str)
                        });
                        !pre_has_user && post_has_user
                    } else {
                        false
                    };

                    if has_new_ata {
                        deductions += ATA_RENT;
                    }

                    let token_cost = total_spent.saturating_sub(deductions);
                    let sol = token_cost as f64 / 1e9;
                    debug!(
                        "实际 SOL 花费: {:.6} SOL (total={}, fee={}, ata_rent={}, token_cost={})",
                        sol, total_spent, fee, if has_new_ata { ATA_RENT } else { 0 }, token_cost,
                    );
                    Some(sol)
                } else {
                    None
                }
            }
            Ok(Err(e)) => {
                debug!("getTransaction 失败: {}", e);
                None
            }
            Err(e) => {
                debug!("getTransaction 任务错误: {}", e);
                None
            }
        }
    }
}
