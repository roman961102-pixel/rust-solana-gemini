use dashmap::DashMap;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::{AppConfig, DynConfig};
use crate::grpc::{AccountUpdate, BondingCurveCache};
use crate::utils::sol_price::SolUsdPrice;
use super::persistence;
use super::position::{Position, PositionState, SellReason, SellSignal};


pub struct AutoSellManager {
    positions: Arc<DashMap<Pubkey, Position>>,
    config: AppConfig,
    dyn_config: Arc<DynConfig>,
    bc_cache: BondingCurveCache,
    rpc_client: Arc<RpcClient>,
    sol_usd: SolUsdPrice,
}

impl AutoSellManager {
    pub fn new(
        config: AppConfig,
        dyn_config: Arc<DynConfig>,
        bc_cache: BondingCurveCache,
        rpc_client: Arc<RpcClient>,
        sol_usd: SolUsdPrice,
    ) -> Self {
        Self {
            positions: Arc::new(DashMap::new()),
            config,
            dyn_config,
            bc_cache,
            rpc_client,
            sol_usd,
        }
    }

    // ============================================
    // 仓位管理
    // ============================================

    /// 保存仓位到磁盘
    fn save(&self) {
        persistence::save_positions(&self.positions);
    }

    /// 立即添加仓位（Pending 状态，止损立即生效）
    pub fn add_position(&self, position: Position) {
        info!(
            "Position opened: {} | state: {} | entry: {:.6} SOL",
            &position.token_mint.to_string()[..12],
            position.state,
            position.entry_price_sol,
        );
        self.positions.insert(position.token_mint, position);
        self.save();
    }

    /// 更新仓位状态为 Submitted
    pub fn mark_submitted(&self, mint: &Pubkey, signature: String) {
        if let Some(mut pos) = self.positions.get_mut(mint) {
            pos.mark_submitted(signature);
        }
    }

    /// 更新仓位状态为 Confirming
    pub fn mark_confirming(&self, mint: &Pubkey) {
        if let Some(mut pos) = self.positions.get_mut(mint) {
            pos.mark_confirming();
        }
    }

    /// 链上确认成功 → Active，修正真实价格和代币数量
    /// bc_price_sol: bonding curve 现货价格，用作入场成本价
    pub fn confirm_success(&self, mint: &Pubkey, actual_token_amount: u64, bc_price_sol: Option<f64>) {
        if let Some(mut pos) = self.positions.get_mut(mint) {
            pos.mark_active(actual_token_amount, bc_price_sol);
        }
    }

    /// 后台修正入场价（从链上 getTransaction 获取真实花费后调用）
    pub fn update_entry_price(&self, mint: &Pubkey, real_price: f64) {
        if let Some(mut pos) = self.positions.get_mut(mint) {
            pos.entry_price_sol = real_price;
            pos.highest_price = pos.highest_price.max(real_price);
        }
    }

    /// 标记卖出中
    pub fn mark_selling(&self, mint: &Pubkey) -> bool {
        if let Some(mut pos) = self.positions.get_mut(mint) {
            pos.mark_selling()
        } else {
            false
        }
    }

    /// 卖出成功 → Closed
    pub fn mark_closed(&self, mint: &Pubkey, sell_signature: String) {
        if let Some(mut pos) = self.positions.get_mut(mint) {
            pos.mark_closed(sell_signature);
        }
        self.save();
    }

    /// 卖出失败 → 回退 Active（可重试）
    pub fn revert_to_active(&self, mint: &Pubkey) {
        if let Some(mut pos) = self.positions.get_mut(mint) {
            pos.revert_to_active();
        }
        self.save();
    }

    /// 确认失败
    pub fn confirm_failed(&self, mint: &Pubkey, reason: &str) {
        if let Some(mut pos) = self.positions.get_mut(mint) {
            pos.mark_failed(reason);
        }
        self.save();
    }

    /// 获取仓位
    pub fn get_position(&self, mint: &Pubkey) -> Option<Position> {
        self.positions.get(mint).map(|v| v.clone())
    }

    /// 移除仓位
    pub fn remove_position(&self, mint: &Pubkey) -> Option<Position> {
        self.positions.remove(mint).map(|(_, p)| p)
    }

    /// 获取当前持仓数量
    pub fn position_count(&self) -> usize {
        self.positions.len()
    }

    /// 获取所有活跃仓位（排除 Closed/Failed/Selling）
    pub fn get_active_positions(&self) -> Vec<Position> {
        self.positions
            .iter()
            .filter(|p| !matches!(p.state, crate::autosell::PositionState::Closed | crate::autosell::PositionState::Failed | crate::autosell::PositionState::Selling))
            .map(|p| p.value().clone())
            .collect()
    }

    /// 获取可卖出的仓位列表
    pub fn get_sellable_positions(&self) -> Vec<Pubkey> {
        self.positions
            .iter()
            .filter(|p| p.can_sell())
            .map(|p| *p.key())
            .collect()
    }

    // ============================================
    // gRPC 驱动的价格监控（替代 Jupiter 轮询）
    // ============================================

    /// 启动 gRPC 驱动的价格监控
    /// 通过 AccountUpdate 事件实时更新仓位价格
    pub fn start_grpc_monitor(
        &self,
        mut account_update_rx: mpsc::UnboundedReceiver<AccountUpdate>,
        sell_signal_tx: mpsc::UnboundedSender<SellSignal>,
    ) -> tokio::task::JoinHandle<()> {
        let positions = self.positions.clone();
        let dyn_config = self.dyn_config.clone();
        let sol_usd = self.sol_usd.clone();
        let bc_cache = self.bc_cache.clone();

        tokio::spawn(async move {
            info!("Auto-sell monitor started (gRPC real-time pricing)");

            while let Some(update) = account_update_rx.recv().await {
                match update {
                    AccountUpdate::BondingCurve(bc_update) => {
                        let mint = bc_update.mint;

                        let signal = {
                            let mut pos = match positions.get_mut(&mint) {
                                Some(p) => p,
                                None => continue,
                            };

                            let current_price = bc_update.state.price_sol();
                            if current_price <= 0.0 {
                                continue;
                            }

                            pos.update_price(current_price);

                            Self::check_exit_conditions_dyn(&pos, &dyn_config)
                        };

                        if let Some(signal) = signal {
                            let sol_price = sol_usd.get();
                            let entry_sol = {
                                let pos = positions.get(&signal.token_mint).unwrap();
                                pos.entry_sol_amount as f64 / 1e9
                            };
                            let value_usd = entry_sol * (1.0 + signal.pnl_percent / 100.0) * sol_price;
                            info!(
                                "SELL SIGNAL: {} | reason: {} | PnL: {:.2}% | 价值: ${:.2}",
                                &signal.token_mint.to_string()[..12],
                                signal.reason,
                                signal.pnl_percent,
                                value_usd,
                            );
                            if sell_signal_tx.send(signal).is_err() {
                                error!("Sell signal channel closed");
                                return;
                            }
                        }
                    }
                    AccountUpdate::AtaBalance(ata_update) => {
                        let mint = ata_update.mint;

                        // ATA 余额变化 = 交易确认信号
                        if let Some(mut pos) = positions.get_mut(&mint) {
                            if ata_update.amount > 0
                                && (pos.state == PositionState::Submitted
                                    || pos.state == PositionState::Confirming)
                            {
                                // 买入确认成功，用 bonding curve 现货价格作为入场价
                                let bc_price = bc_cache.get(&mint).map(|s| s.price_sol());
                                pos.mark_active(ata_update.amount, bc_price);
                            } else if ata_update.amount == 0
                                && pos.state == PositionState::Selling
                            {
                                // 卖出确认成功（余额归零）
                                debug!(
                                    "ATA balance zero for {} - sell confirmed",
                                    &mint.to_string()[..12],
                                );
                            }
                        }
                    }
                }
            }

            warn!("Account update channel closed, auto-sell monitor stopped");
        })
    }

    /// 启动定时兜底检查
    /// 1. 买入确认超时兜底：CONFIRMING 超过 5 秒 → RPC 查余额 → 有币确认/无币删除
    /// 2. 价格监控兜底：用 bc_cache 更新价格 + 检查退出条件
    /// 3. 清理已关闭/失败仓位
    pub fn start_fallback_monitor(
        &self,
        sell_signal_tx: mpsc::UnboundedSender<SellSignal>,
    ) -> tokio::task::JoinHandle<()> {
        let positions = self.positions.clone();
        let config = self.config.clone();
        let dyn_config = self.dyn_config.clone();
        let bc_cache = self.bc_cache.clone();
        let rpc = self.rpc_client.clone();
        let user_pubkey = config.pubkey;
        let interval = Duration::from_secs(config.price_check_interval_secs);

        tokio::spawn(async move {
            info!("Fallback monitor started (interval: {:?})", interval);

            loop {
                tokio::time::sleep(interval).await;

                if positions.is_empty() {
                    continue;
                }

                let mints: Vec<Pubkey> = positions.iter().map(|r| *r.key()).collect();

                for mint in mints {
                    // ===== 买入确认超时兜底 =====
                    let needs_confirm_check = {
                        let pos = match positions.get(&mint) {
                            Some(p) => p,
                            None => continue,
                        };
                        (pos.state == PositionState::Submitted
                            || pos.state == PositionState::Confirming)
                            && pos.held_seconds() >= config.confirm_timeout_secs
                    };

                    if needs_confirm_check {
                        let user_ata = get_associated_token_address(&user_pubkey, &mint);
                        let rpc_clone = rpc.clone();
                        let ata = user_ata;
                        let balance = tokio::task::spawn_blocking(move || {
                            rpc_clone
                                .get_token_account_balance(&ata)
                                .map(|b| b.amount.parse::<u64>().unwrap_or(0))
                                .unwrap_or(0)
                        })
                        .await
                        .unwrap_or(0);

                        if let Some(mut pos) = positions.get_mut(&mint) {
                            if balance > 0 {
                                // 有代币 → 买入实际成功，确认并修正价格
                                let bc_price = bc_cache.get(&mint).map(|s| s.price_sol());
                                info!(
                                    "确认兜底: {} 有 {} tokens → 确认 ACTIVE",
                                    &mint.to_string()[..12],
                                    balance,
                                );
                                pos.mark_active(balance, bc_price);
                            } else {
                                // 无代币 → 买入实际失败，删除仓位
                                warn!(
                                    "确认兜底: {} 余额为 0 → 删除仓位 (买入实际失败)",
                                    &mint.to_string()[..12],
                                );
                                pos.mark_failed("buy not confirmed: zero balance");
                            }
                        }
                        continue;
                    }

                    // ===== 正常价格监控 + 退出条件检查 =====
                    let signal = {
                        let mut pos = match positions.get_mut(&mint) {
                            Some(p) => p,
                            None => continue,
                        };

                        // 超时退出（读取动态配置）
                        let max_hold = dyn_config.max_hold_seconds();
                        if max_hold > 0
                            && pos.held_seconds() >= max_hold
                            && pos.can_sell()
                        {
                            Some(SellSignal {
                                token_mint: mint,
                                reason: SellReason::MaxLifetime,
                                current_price: pos.current_price,
                                pnl_percent: pos.pnl_percent(),
                            })
                        } else if let Some(bc_state) = bc_cache.get(&mint) {
                            let price = bc_state.price_sol();
                            if price > 0.0 {
                                pos.update_price(price);
                            }
                            Self::check_exit_conditions_dyn(&pos, &dyn_config)
                        } else if pos.can_sell() {
                            // BC 缓存未命中 → RPC 兜底获取 bonding curve 价格
                            let program_id = solana_sdk::pubkey::Pubkey::from_str(
                                "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",
                            ).unwrap();
                            let (bc_addr, _) = solana_sdk::pubkey::Pubkey::find_program_address(
                                &[b"bonding-curve", mint.as_ref()], &program_id,
                            );
                            let rpc_clone = rpc.clone();
                            let bc_data = tokio::task::spawn_blocking(move || {
                                rpc_clone.get_account_data(&bc_addr)
                            }).await;
                            if let Ok(Ok(data)) = bc_data {
                                if let Ok(state) = crate::processor::pumpfun::BondingCurveState::from_account_data(&data) {
                                    let price = state.price_sol();
                                    if price > 0.0 {
                                        bc_cache.update(&mint, state);
                                        pos.update_price(price);
                                        debug!(
                                            "兜底价格更新(RPC): {} | price: {:.10} SOL",
                                            &mint.to_string()[..12], price,
                                        );
                                    }
                                    Self::check_exit_conditions_dyn(&pos, &dyn_config)
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };

                    if let Some(signal) = signal {
                        info!(
                            "SELL SIGNAL: {} | reason: {} | PnL: {:.2}%",
                            &signal.token_mint.to_string()[..12],
                            signal.reason,
                            signal.pnl_percent,
                        );
                        if sell_signal_tx.send(signal).is_err() {
                            error!("Sell signal channel closed");
                            return;
                        }
                    }
                }

                // 清理已关闭/失败的仓位
                positions.retain(|_, pos| {
                    if pos.state == PositionState::Closed || pos.state == PositionState::Failed {
                        pos.created_at.elapsed().as_secs() < 60
                    } else {
                        true
                    }
                });
            }
        })
    }

    // ============================================
    // 退出条件检查
    // ============================================

    /// 更新仓位的代币信息（确认后填充）
    pub fn update_token_info(&self, mint: &Pubkey, token_name: String, entry_mcap_sol: f64) {
        if let Some(mut pos) = self.positions.get_mut(mint) {
            pos.token_name = token_name;
            pos.entry_mcap_sol = entry_mcap_sol;
        }
    }

    /// 使用 DynConfig（运行时可修改参数）检查退出条件
    /// 跟卖模式(sell_mode=1)时跳过 TP/SL/Trailing，仅保留 MaxLifetime 安全兜底
    fn check_exit_conditions_dyn(pos: &Position, dc: &DynConfig) -> Option<SellSignal> {
        let pnl = pos.pnl_percent();
        let mint = pos.token_mint;

        // MaxLifetime 始终生效（安全兜底）
        let max_hold = dc.max_hold_seconds();
        if max_hold > 0 && pos.held_seconds() >= max_hold && pos.can_sell() {
            return Some(SellSignal { token_mint: mint, reason: SellReason::MaxLifetime, current_price: pos.current_price, pnl_percent: pnl });
        }

        // 跟卖模式: 不检查 TP/SL/Trailing（由聪明钱卖出信号触发）
        if dc.is_follow_sell_mode() {
            return None;
        }

        if pos.can_check_stop_loss() && pnl <= -dc.stop_loss_percent() {
            return Some(SellSignal { token_mint: mint, reason: SellReason::StopLoss, current_price: pos.current_price, pnl_percent: pnl });
        }
        if pos.can_check_take_profit() && pnl >= dc.take_profit_percent() {
            return Some(SellSignal { token_mint: mint, reason: SellReason::TakeProfit, current_price: pos.current_price, pnl_percent: pnl });
        }
        if pos.can_check_take_profit() && dc.trailing_stop_percent() > 0.0 && pos.highest_price > 0.0 && pnl > 0.0 {
            if pos.drawdown_percent() >= dc.trailing_stop_percent() {
                return Some(SellSignal { token_mint: mint, reason: SellReason::TrailingStop, current_price: pos.current_price, pnl_percent: pnl });
            }
        }
        None
    }

    fn check_exit_conditions(pos: &Position, config: &AppConfig) -> Option<SellSignal> {
        let pnl = pos.pnl_percent();
        let mint = pos.token_mint;

        // 1. 超时退出
        if config.max_hold_seconds > 0
            && pos.held_seconds() >= config.max_hold_seconds
            && pos.can_sell()
        {
            return Some(SellSignal {
                token_mint: mint,
                reason: SellReason::MaxLifetime,
                current_price: pos.current_price,
                pnl_percent: pnl,
            });
        }

        // 2. 止损（Confirming + Active 都检查）
        if pos.can_check_stop_loss() && pnl <= -config.stop_loss_percent {
            return Some(SellSignal {
                token_mint: mint,
                reason: SellReason::StopLoss,
                current_price: pos.current_price,
                pnl_percent: pnl,
            });
        }

        // 3. 止盈（仅 Active）
        if pos.can_check_take_profit() && pnl >= config.take_profit_percent {
            return Some(SellSignal {
                token_mint: mint,
                reason: SellReason::TakeProfit,
                current_price: pos.current_price,
                pnl_percent: pnl,
            });
        }

        // 4. 追踪止损（仅 Active 且盈利时）
        if pos.can_check_take_profit()
            && config.trailing_stop_percent > 0.0
            && pos.highest_price > 0.0
            && pnl > 0.0
        {
            let drawdown = pos.drawdown_percent();
            if drawdown >= config.trailing_stop_percent {
                return Some(SellSignal {
                    token_mint: mint,
                    reason: SellReason::TrailingStop,
                    current_price: pos.current_price,
                    pnl_percent: pnl,
                });
            }
        }

        None
    }
}
