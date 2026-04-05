use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::autosell::{AutoSellManager, SellSignal};
use crate::config::{AppConfig, DynConfig};
use crate::grpc::{AccountSubscriber, AtaBalanceCache};
use crate::processor::prefetch::PrefetchCache;
use crate::telegram::{TgEvent, TgNotifier};
use crate::tx::jupiter::JupiterSeller;
use crate::tx::sender::TxSender;

const MAX_SELL_RETRIES: u32 = 3;

pub struct SellExecutor {
    config: AppConfig,
    dyn_config: Arc<DynConfig>,
    rpc_client: Arc<RpcClient>,
    tx_sender: Arc<TxSender>,
    auto_sell: Arc<AutoSellManager>,
    ata_cache: AtaBalanceCache,
    prefetch_cache: Arc<PrefetchCache>,
    account_subscriber: Arc<AccountSubscriber>,
    jupiter: JupiterSeller,
    tg: TgNotifier,
}

impl SellExecutor {
    pub fn new(
        config: AppConfig,
        dyn_config: Arc<DynConfig>,
        rpc_client: Arc<RpcClient>,
        _pumpfun: Arc<crate::processor::pumpfun::PumpfunProcessor>,
        tx_sender: Arc<TxSender>,
        _blockhash_cache: crate::tx::blockhash::BlockhashCache,
        auto_sell: Arc<AutoSellManager>,
        ata_cache: AtaBalanceCache,
        prefetch_cache: Arc<PrefetchCache>,
        account_subscriber: Arc<AccountSubscriber>,
        tg: TgNotifier,
    ) -> Self {
        Self {
            config,
            dyn_config,
            rpc_client,
            tx_sender,
            auto_sell,
            ata_cache,
            prefetch_cache,
            account_subscriber,
            jupiter: JupiterSeller::new(),
            tg,
        }
    }

    /// 处理卖出信号（通过 Jupiter API）
    pub async fn handle_sell_signal(&self, signal: SellSignal) {
        let sell_start = std::time::Instant::now();
        let mint = signal.token_mint;

        info!(
            "收到卖出信号: {}.. | 原因: {} | 盈亏: {:.2}%",
            &mint.to_string()[..12],
            signal.reason,
            signal.pnl_percent,
        );

        // 检查是否已超过最大卖出尝试次数
        const MAX_TOTAL_SELL_ATTEMPTS: u32 = 3;
        if let Some(pos) = self.auto_sell.get_position(&mint) {
            if pos.sell_attempts >= MAX_TOTAL_SELL_ATTEMPTS {
                error!(
                    "卖出已尝试 {} 次全部失败，放弃: {}",
                    pos.sell_attempts, &mint.to_string()[..12],
                );
                self.auto_sell.confirm_failed(&mint, "max sell attempts exceeded");
                self.account_subscriber.untrack_mint(&mint);
                self.prefetch_cache.remove(&mint);
                return;
            }
        }

        if !self.auto_sell.mark_selling(&mint) {
            warn!("仓位不在可卖出状态，跳过");
            return;
        }

        // 获取 ATA 和余额
        let user_ata = self.prefetch_cache.get(&mint)
            .map(|pf| pf.user_ata)
            .unwrap_or_else(|| get_associated_token_address(&self.config.pubkey, &mint));

        let token_balance = self.ata_cache.get(&mint).unwrap_or_else(|| {
            self.get_token_balance_rpc(&user_ata)
        });

        if token_balance == 0 {
            warn!("跳过卖出: 余额为 0");
            self.auto_sell.confirm_failed(&mint, "zero balance");
            return;
        }

        let mut success = false;
        let mut last_sig = String::new();

        for attempt in 1..=MAX_SELL_RETRIES {
            info!(
                "Jupiter 卖出尝试 #{}/{}: {}.. (数量: {})",
                attempt, MAX_SELL_RETRIES,
                &mint.to_string()[..12], token_balance,
            );

            match self.try_jupiter_sell(&mint, token_balance).await {
                Ok(sig) => {
                    info!(
                        "Jupiter 卖出已提交: https://solscan.io/tx/{} | 构建: {}ms",
                        sig, sell_start.elapsed().as_millis(),
                    );

                    // 等待确认
                    let confirmed = self.wait_sell_confirm(&sig, 10).await;
                    if confirmed {
                        info!(
                            "Jupiter 卖出确认成功: {} | 全流程: {}ms",
                            &sig[..16], sell_start.elapsed().as_millis(),
                        );
                        last_sig = sig;
                        success = true;
                        break;
                    } else {
                        warn!("Jupiter 卖出未确认或失败: {} (立即重试)", &sig[..16]);
                    }
                }
                Err(e) => {
                    warn!("Jupiter 卖出尝试 #{} 失败: {}", attempt, e);
                    if attempt < MAX_SELL_RETRIES {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }

        if success {
            self.try_close_ata(&mint, &user_ata).await;
            self.auto_sell.mark_closed(&mint, last_sig.clone());
            self.ata_cache.remove(&mint);
            self.account_subscriber.untrack_mint(&mint);
            self.prefetch_cache.remove(&mint);

            // TG 推送卖出成功
            let mint_str = mint.to_string();
            self.tg.send(TgEvent::SellSuccess {
                mint,
                token_name: format!("{}..{}", &mint_str[..6], &mint_str[mint_str.len() - 4..]),
                reason: signal.reason.to_string(),
                pnl_percent: signal.pnl_percent,
                tx_sig: last_sig,
            });
        } else {
            let remaining = self.get_token_balance_rpc(&user_ata);
            if remaining > 0 {
                warn!(
                    "卖出失败但仍有 {} tokens，回退 Active: {}",
                    remaining, &mint.to_string()[..12],
                );
                self.auto_sell.revert_to_active(&mint);
            } else {
                warn!(
                    "卖出失败且余额为 0，放弃: {}",
                    &mint.to_string()[..12],
                );
                self.auto_sell.confirm_failed(&mint, "sell failed, zero balance");
                self.account_subscriber.untrack_mint(&mint);
                self.prefetch_cache.remove(&mint);
            }

            // TG 推送卖出失败
            self.tg.send(TgEvent::SellFailed {
                mint,
                reason: "Jupiter 卖出重试耗尽".to_string(),
            });
        }
    }

    /// 通过 Jupiter API 构建卖出交易并发送
    async fn try_jupiter_sell(&self, mint: &Pubkey, token_amount: u64) -> Result<String> {
        // Jupiter 构建签名后的交易（priority fee 由 Jupiter 自动估算）
        let signed_tx_bytes = self.jupiter.build_sell_transaction(
            mint,
            token_amount,
            self.dyn_config.sell_slippage_bps(),
            &self.config.keypair,
        ).await?;

        // 发送到多通道（RPC + Jito）
        let tx_base64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &signed_tx_bytes,
        );

        // 通过 RPC sendTransaction 发送（base64 编码）
        let rpc_url = self.config.rpc_url.clone();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        let send_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [
                tx_base64,
                {
                    "encoding": "base64",
                    "skipPreflight": true,
                    "maxRetries": 0
                }
            ]
        });

        let resp: serde_json::Value = http
            .post(&rpc_url)
            .json(&send_request)
            .send()
            .await?
            .json()
            .await?;

        if let Some(error) = resp.get("error") {
            anyhow::bail!("Jupiter 卖出 RPC 发送失败: {}", error);
        }

        let sig = resp["result"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Jupiter 卖出无签名返回"))?;

        Ok(sig.to_string())
    }

    async fn try_close_ata(&self, mint: &Pubkey, ata: &Pubkey) {
        // 从 gRPC 缓存检查余额
        let balance = self.ata_cache.get(mint).unwrap_or_else(|| {
            self.get_token_balance_rpc(ata)
        });
        if balance > 0 {
            debug!("ATA still has balance {}, skip close", balance);
            return;
        }

        // 使用正确的 token program（兼容 Token-2022）
        let token_program = self.prefetch_cache.get(mint)
            .map(|pf| pf.token_program)
            .unwrap_or_else(spl_token::id);

        let close_ix = spl_token::instruction::close_account(
            &token_program,
            ata,
            &self.config.pubkey,
            &self.config.pubkey,
            &[],
        );

        let close_ix = match close_ix {
            Ok(ix) => ix,
            Err(e) => {
                debug!("Failed to build close ATA instruction: {}", e);
                return;
            }
        };

        // 获取最新 blockhash
        let rpc_bh = self.rpc_client.clone();
        let bh_result = tokio::task::spawn_blocking(move || {
            rpc_bh.get_latest_blockhash()
        }).await;
        let blockhash = match bh_result {
            Ok(Ok(bh)) => bh,
            _ => { debug!("获取 blockhash 失败，跳过 close ATA"); return; }
        };
        let msg = solana_sdk::message::Message::new(&[close_ix], Some(&self.config.pubkey));
        let mut tx = solana_sdk::transaction::Transaction::new_unsigned(msg);
        tx.sign(&[&*self.config.keypair], blockhash);

        let rpc = self.rpc_client.clone();
        match tokio::task::spawn_blocking(move || {
            rpc.send_transaction_with_config(
                &tx,
                solana_client::rpc_config::RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..Default::default()
                },
            )
        })
        .await
        {
            Ok(Ok(sig)) => {
                info!(
                    "ATA closed: https://solscan.io/tx/{} (~0.002 SOL)",
                    sig,
                );
            }
            Ok(Err(e)) => debug!("Close ATA failed: {}", e),
            Err(e) => debug!("Close ATA task error: {}", e),
        }
    }

    /// 等待卖出交易链上确认
    async fn wait_sell_confirm(&self, sig_str: &str, max_secs: u64) -> bool {
        use solana_sdk::signature::Signature;
        let sig = match sig_str.parse::<Signature>() {
            Ok(s) => s,
            Err(_) => return false,
        };
        let max_wait = Duration::from_secs(max_secs);
        let start = std::time::Instant::now();
        while start.elapsed() < max_wait {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let rpc = self.rpc_client.clone();
            let s = sig;
            let result = tokio::task::spawn_blocking(move || {
                rpc.get_signature_statuses(&[s])
            }).await;
            match result {
                Ok(Ok(resp)) => {
                    if let Some(Some(status)) = resp.value.first() {
                        if status.err.is_some() {
                            warn!("卖出链上失败: {:?}", status.err);
                            return false;
                        }
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// 分档卖出（25% / 50% / 75% / 100%）
    pub async fn handle_partial_sell(&self, mint: &Pubkey, percent: u32) {
        let user_ata = self.prefetch_cache.get(mint)
            .map(|pf| pf.user_ata)
            .unwrap_or_else(|| get_associated_token_address(&self.config.pubkey, mint));

        let total_balance = self.ata_cache.get(mint).unwrap_or_else(|| {
            self.get_token_balance_rpc(&user_ata)
        });

        if total_balance == 0 {
            warn!("分档卖出: 余额为 0");
            self.tg.send(TgEvent::SellFailed { mint: *mint, reason: "余额为 0".into() });
            return;
        }

        let sell_amount = if percent >= 100 {
            total_balance
        } else {
            (total_balance as u128 * percent as u128 / 100) as u64
        };

        if sell_amount == 0 {
            self.tg.send(TgEvent::SellFailed { mint: *mint, reason: "卖出数量为 0".into() });
            return;
        }

        info!("分档卖出: {}% of {} = {} tokens", percent, &mint.to_string()[..12], sell_amount);

        let mut success = false;
        let mut last_sig = String::new();

        for attempt in 1..=MAX_SELL_RETRIES {
            match self.try_jupiter_sell(mint, sell_amount).await {
                Ok(sig) => {
                    let confirmed = self.wait_sell_confirm(&sig, 10).await;
                    if confirmed {
                        last_sig = sig;
                        success = true;
                        break;
                    }
                }
                Err(e) => {
                    warn!("分档卖出尝试 #{} 失败: {}", attempt, e);
                    if attempt < MAX_SELL_RETRIES {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }

        let mint_str = mint.to_string();
        let short = format!("{}..{}", &mint_str[..6], &mint_str[mint_str.len() - 4..]);

        if success {
            // 100% 卖出时关闭仓位和 ATA
            if percent >= 100 {
                self.try_close_ata(mint, &user_ata).await;
                self.auto_sell.mark_closed(mint, last_sig.clone());
                self.ata_cache.remove(mint);
                self.account_subscriber.untrack_mint(mint);
                self.prefetch_cache.remove(mint);
            }
            self.tg.send(TgEvent::SellSuccess {
                mint: *mint,
                token_name: short,
                reason: format!("手动 {}%", percent),
                pnl_percent: self.auto_sell.get_position(mint).map(|p| p.pnl_percent()).unwrap_or(0.0),
                tx_sig: last_sig,
            });
        } else {
            self.tg.send(TgEvent::SellFailed {
                mint: *mint,
                reason: format!("{}% 卖出重试耗尽", percent),
            });
        }
    }

    /// RPC 回退：仅在 gRPC 缓存未命中时使用
    fn get_token_balance_rpc(&self, ata: &Pubkey) -> u64 {
        self.rpc_client
            .get_token_account_balance(ata)
            .map(|b| b.amount.parse::<u64>().unwrap_or(0))
            .unwrap_or(0)
    }
}
