use solana_sdk::pubkey::Pubkey;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::autosell::{AutoSellManager, SellReason, SellSignal};
use crate::config::{AppConfig, DynConfig, SELL_MODE_TP_SL, SELL_MODE_FOLLOW};
use crate::consensus::ConsensusEngine;
use crate::tx::confirm::{format_price_gmgn, format_mcap_usd};
use crate::tx::sell_executor::SellExecutor;
use crate::utils::sol_price::SolUsdPrice;

// ============================================
// 通知事件
// ============================================

pub enum TgEvent {
    ConsensusReached { mint: Pubkey, wallets: Vec<Pubkey> },
    BuySubmitted { mint: Pubkey, sol_amount: f64, latency_ms: u64 },
    BuyConfirmed { mint: Pubkey, token_name: String, spent_sol: f64, cost_price_usd: String, mcap_usd: String },
    BuyFailed { mint: Pubkey, reason: String },
    SellSuccess { mint: Pubkey, token_name: String, reason: String, pnl_percent: f64, tx_sig: String },
    SellFailed { mint: Pubkey, reason: String },
}

// ============================================
// TgNotifier
// ============================================

#[derive(Clone)]
pub struct TgNotifier {
    tx: mpsc::UnboundedSender<TgEvent>,
    enabled: bool,
}

impl TgNotifier {
    pub fn send(&self, event: TgEvent) {
        if self.enabled { let _ = self.tx.send(event); }
    }
    pub fn noop() -> Self {
        let (tx, _) = mpsc::unbounded_channel();
        Self { tx, enabled: false }
    }
    pub fn from_sender(tx: mpsc::UnboundedSender<TgEvent>) -> Self {
        Self { tx, enabled: true }
    }
}

// ============================================
// TgStats
// ============================================

pub struct TgStats {
    pub started_at: Instant,
    pub grpc_events: AtomicU64,
    pub buy_attempts: AtomicU64,
    pub buy_success: AtomicU64,
    pub buy_failed: AtomicU64,
}

impl TgStats {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            grpc_events: AtomicU64::new(0),
            buy_attempts: AtomicU64::new(0),
            buy_success: AtomicU64::new(0),
            buy_failed: AtomicU64::new(0),
        }
    }
}

// ============================================
// TgBot
// ============================================

pub struct TgBot {
    token: String,
    chat_id: String,
    http: reqwest::Client,
    auto_sell: Arc<AutoSellManager>,
    consensus: Arc<ConsensusEngine>,
    config: AppConfig,
    dyn_config: Arc<DynConfig>,
    sell_signal_tx: mpsc::UnboundedSender<SellSignal>,
    sell_executor: Arc<SellExecutor>,
    is_running: Arc<AtomicBool>,
    stats: Arc<TgStats>,
    sol_usd: SolUsdPrice,
    event_rx: Option<mpsc::UnboundedReceiver<TgEvent>>,
}

impl TgBot {
    /// 从预创建的 event_rx 构建（支持两阶段初始化）
    pub fn from_parts(
        config: AppConfig,
        dyn_config: Arc<DynConfig>,
        auto_sell: Arc<AutoSellManager>,
        consensus: Arc<ConsensusEngine>,
        sell_signal_tx: mpsc::UnboundedSender<SellSignal>,
        sell_executor: Arc<SellExecutor>,
        is_running: Arc<AtomicBool>,
        stats: Arc<TgStats>,
        sol_usd: SolUsdPrice,
        event_rx: mpsc::UnboundedReceiver<TgEvent>,
    ) -> Self {
        let token = config.telegram_bot_token.clone().unwrap_or_default();
        let chat_id = config.telegram_chat_id.clone().unwrap_or_default();
        Self {
            token, chat_id,
            http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(60)).build().unwrap(),
            auto_sell, consensus, config, dyn_config,
            sell_signal_tx, sell_executor,
            is_running, stats, sol_usd,
            event_rx: Some(event_rx),
        }
    }

    pub async fn run(mut self) {
        info!("Telegram Bot V2 已启动 | chat_id: {}", self.chat_id);

        // 注册 Bot Menu 按钮
        self.set_bot_commands().await;

        // 启动时发送提示
        self.send_msg("🤖 <b>跟单机器人已上线</b>\n\n⏸ 监听未启动，发送 /start 开始跟单").await;

        let mut offset: i64 = 0;
        let mut event_rx = self.event_rx.take().expect("event_rx already taken");

        loop {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    self.handle_event(event).await;
                }
                result = self.get_updates(offset) => {
                    match result {
                        Ok(updates) => {
                            for update in updates {
                                let uid = update["update_id"].as_i64().unwrap_or(0);
                                if uid >= offset { offset = uid + 1; }
                                self.handle_update(&update).await;
                            }
                        }
                        Err(e) => {
                            debug!("TG getUpdates: {}", e);
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        }
                    }
                }
            }
        }
    }

    // ============================================
    // Telegram API
    // ============================================

    /// 注册 Bot Menu 命令按钮（在输入框左侧显示 / 菜单）
    async fn set_bot_commands(&self) {
        let url = format!("https://api.telegram.org/bot{}/setMyCommands", self.token);
        let commands = serde_json::json!({
            "commands": [
                {"command": "start", "description": "启动监听"},
                {"command": "stop", "description": "停止监听"},
                {"command": "status", "description": "运行状态"},
                {"command": "pos", "description": "持仓列表"},
                {"command": "sellall", "description": "一键清仓"},
                {"command": "sellmode", "description": "切换卖出模式"},
                {"command": "stats", "description": "运行统计"},
                {"command": "set", "description": "查看/修改配置"},
                {"command": "wallet", "description": "查看余额"},
                {"command": "wallets", "description": "跟踪钱包列表"},
                {"command": "block", "description": "拉黑代币"},
                {"command": "blocklist", "description": "黑名单列表"},
                {"command": "pnl", "description": "盈亏汇总"},
            ]
        });
        match self.http.post(&url).json(&commands).send().await {
            Ok(resp) => {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    if v["ok"].as_bool() == Some(true) {
                        info!("TG Bot Menu 命令已注册 (13 个)");
                    } else {
                        warn!("TG setMyCommands 失败: {}", v);
                    }
                }
            }
            Err(e) => warn!("TG setMyCommands 请求失败: {}", e),
        }
    }

    async fn get_updates(&self, offset: i64) -> anyhow::Result<Vec<serde_json::Value>> {
        let url = format!("https://api.telegram.org/bot{}/getUpdates", self.token);
        let resp: serde_json::Value = self.http.get(&url)
            .query(&[("offset", offset.to_string()), ("timeout", "30".into()), ("allowed_updates", r#"["message","callback_query"]"#.into())])
            .send().await?.json().await?;
        if resp["ok"].as_bool() != Some(true) { anyhow::bail!("TG: {}", resp); }
        Ok(resp["result"].as_array().cloned().unwrap_or_default())
    }

    async fn send_msg(&self, text: &str) -> Option<serde_json::Value> {
        self.send_msg_kb(text, None).await
    }

    async fn send_msg_kb(&self, text: &str, kb: Option<serde_json::Value>) -> Option<serde_json::Value> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let mut body = serde_json::json!({
            "chat_id": self.chat_id, "text": text,
            "parse_mode": "HTML", "disable_web_page_preview": true,
        });
        if let Some(k) = kb { body["reply_markup"] = k; }
        match self.http.post(&url).json(&body).send().await {
            Ok(r) => {
                let resp = r.json::<serde_json::Value>().await.ok()?;
                if resp["ok"].as_bool() != Some(true) {
                    warn!("TG sendMessage 失败: {}", resp);
                    // 如果带键盘发送失败，尝试纯文本重发
                    if body.get("reply_markup").is_some() {
                        warn!("尝试无键盘重发...");
                        let fallback = serde_json::json!({
                            "chat_id": self.chat_id, "text": text,
                            "parse_mode": "HTML", "disable_web_page_preview": true,
                        });
                        if let Ok(r2) = self.http.post(&url).json(&fallback).send().await {
                            return r2.json::<serde_json::Value>().await.ok();
                        }
                    }
                    return None;
                }
                Some(resp)
            }
            Err(e) => { warn!("TG send: {}", e); None }
        }
    }

    async fn edit_msg(&self, mid: i64, text: &str, kb: Option<serde_json::Value>) {
        let url = format!("https://api.telegram.org/bot{}/editMessageText", self.token);
        let mut body = serde_json::json!({
            "chat_id": self.chat_id, "message_id": mid, "text": text,
            "parse_mode": "HTML", "disable_web_page_preview": true,
        });
        if let Some(k) = kb { body["reply_markup"] = k; }
        let _ = self.http.post(&url).json(&body).send().await;
    }

    async fn answer_cb(&self, cb_id: &str, text: &str) {
        let url = format!("https://api.telegram.org/bot{}/answerCallbackQuery", self.token);
        let _ = self.http.post(&url).json(&serde_json::json!({"callback_query_id": cb_id, "text": text})).send().await;
    }

    // ============================================
    // 消息分发
    // ============================================

    async fn handle_update(&self, update: &serde_json::Value) {
        if let Some(msg) = update.get("message") {
            let cid = msg["chat"]["id"].as_i64().unwrap_or(0).to_string();
            if cid != self.chat_id { return; }
            if let Some(text) = msg["text"].as_str() {
                let text = text.trim();
                let parts: Vec<&str> = text.split_whitespace().collect();
                // 处理 /cmd@botname 格式（Telegram 群组中自动附加）
                let raw_cmd = parts.first().copied().unwrap_or("");
                let cmd = raw_cmd.split('@').next().unwrap_or(raw_cmd);
                match cmd {
                    "/start" => self.cmd_start().await,
                    "/stop" => self.cmd_stop().await,
                    "/status" => self.cmd_status().await,
                    "/pos" => self.cmd_positions().await,
                    "/stats" => self.cmd_stats().await,
                    "/sellall" => self.cmd_sellall().await,
                    "/sellmode" => self.cmd_sellmode().await,
                    "/set" => self.cmd_set(&parts[1..]).await,
                    "/wallets" => self.cmd_wallets().await,
                    "/wallet" => self.cmd_wallet().await,
                    "/addwallet" => self.cmd_addwallet(&parts[1..]).await,
                    "/rmwallet" => self.cmd_rmwallet(&parts[1..]).await,
                    "/block" => self.cmd_block(&parts[1..]).await,
                    "/unblock" => self.cmd_unblock(&parts[1..]).await,
                    "/blocklist" => self.cmd_blocklist().await,
                    "/pnl" => self.cmd_pnl().await,
                    _ => {
                        // 检测粘贴的 mint 地址 → 弹出买入按钮
                        if text.len() >= 32 && text.len() <= 44 {
                            if !self.is_running.load(Ordering::Relaxed) {
                                self.send_msg("⚠️ 监听未启动，请先发送 /start").await;
                            } else if let Ok(mint) = text.parse::<Pubkey>() {
                                self.show_buy_buttons(&mint).await;
                            }
                        }
                    }
                }
            }
        }

        if let Some(cb) = update.get("callback_query") {
            let cid = cb["message"]["chat"]["id"].as_i64().unwrap_or(0).to_string();
            if cid != self.chat_id { return; }
            let cb_id = cb["id"].as_str().unwrap_or("");
            let data = cb["data"].as_str().unwrap_or("");
            let mid = cb["message"]["message_id"].as_i64().unwrap_or(0);

            if let Some(rest) = data.strip_prefix("sell:") {
                // sell:PERCENT:MINT
                let parts: Vec<&str> = rest.splitn(2, ':').collect();
                if parts.len() == 2 {
                    if let (Ok(pct), Ok(mint)) = (parts[0].parse::<u32>(), parts[1].parse::<Pubkey>()) {
                        self.answer_cb(cb_id, &format!("卖出 {}%...", pct)).await;
                        let exec = self.sell_executor.clone();
                        tokio::spawn(async move { exec.handle_partial_sell(&mint, pct).await; });
                    }
                }
            } else if let Some(mint_str) = data.strip_prefix("refresh:") {
                self.cb_refresh(cb_id, mid, mint_str).await;
            } else if data == "refresh_pos_list" {
                self.cb_refresh_pos_list(cb_id, mid).await;
            } else if let Some(rest) = data.strip_prefix("sellmode:") {
                let new_mode = if rest == "follow" { SELL_MODE_FOLLOW } else { SELL_MODE_TP_SL };
                self.dyn_config.set_sell_mode(new_mode);
                let mode_text = if new_mode == SELL_MODE_FOLLOW { "跟卖模式" } else { "止盈止损模式" };
                self.answer_cb(cb_id, &format!("已切换: {}", mode_text)).await;
                self.cmd_sellmode_edit(mid).await;
            } else if let Some(wallet_str) = data.strip_prefix("rmw:") {
                if let Ok(pk) = wallet_str.parse::<Pubkey>() {
                    let removed = {
                        let mut wallets = self.dyn_config.target_wallets.write().unwrap();
                        if let Some(idx) = wallets.iter().position(|w| w == &pk) {
                            wallets.remove(idx);
                            true
                        } else {
                            false
                        }
                    }; // RwLockWriteGuard dropped here
                    if removed {
                        self.answer_cb(cb_id, "已删除").await;
                        let updated = self.dyn_config.target_wallets.read().unwrap().clone();
                        if updated.is_empty() {
                            self.edit_msg(mid, "📭 无跟踪钱包\n\n发送 /addwallet 地址 添加", None).await;
                        } else {
                            let mut text = format!("👛 <b>Smart Money 钱包</b> ({}个)\n\n", updated.len());
                            for (i, w) in updated.iter().enumerate() {
                                let ws = w.to_string();
                                text.push_str(&format!("{}. <code>{}..{}</code>\n", i + 1, &ws[..6], &ws[ws.len()-4..]));
                            }
                            text.push_str("\n发送 /addwallet 地址 添加\n⚠️ 需重启 gRPC 生效");
                            let kb = wallets_keyboard(&updated);
                            self.edit_msg(mid, &text, Some(kb)).await;
                        }
                    } else {
                        self.answer_cb(cb_id, "钱包不在列表中").await;
                    }
                }
            } else if data.starts_with("noop:") {
                self.answer_cb(cb_id, "").await;
            } else if let Some(rest) = data.strip_prefix("buy:") {
                // buy:LAMPORTS:MINT
                let parts: Vec<&str> = rest.splitn(2, ':').collect();
                if parts.len() == 2 {
                    self.answer_cb(cb_id, "⚠️ 手动买入暂未实现").await;
                }
            }
        }
    }

    // ============================================
    // 命令处理
    // ============================================

    async fn cmd_start(&self) {
        self.is_running.store(true, Ordering::Relaxed);
        self.send_msg("✅ 监听脚本已启动").await;
    }

    async fn cmd_stop(&self) {
        self.is_running.store(false, Ordering::Relaxed);
        self.send_msg("🛑 监听脚本已停止\n⚠️ 现有仓位的自动卖出仍然有效").await;
    }

    async fn cmd_status(&self) {
        let dc = &self.dyn_config;
        let running = self.is_running.load(Ordering::Relaxed);
        let positions = self.auto_sell.get_active_positions();
        let pending = self.consensus.pending_count();
        let sell_mode_text = if dc.is_follow_sell_mode() { "跟卖模式" } else { "止盈止损模式" };

        let text = format!(
            "📊 <b>运行状态</b>\n\
             \n\
             状态: {}\n\
             钱包: <code>{}</code>\n\
             买入: {} SOL | 最小跟单: {} SOL\n\
             滑点: {} bps | 卖出滑点: {} bps\n\
             卖出模式: {}\n\
             止盈: {}% | 止损: {}% | 追踪: {}%\n\
             最大持仓: {}s\n\
             仓位: {} | 待共识: {}",
            if running { "🟢 运行中" } else { "🔴 已停止" },
            self.config.pubkey,
            dc.buy_sol_amount(), dc.min_target_buy_sol(),
            dc.slippage_bps(), dc.sell_slippage_bps(),
            sell_mode_text,
            dc.take_profit_percent(), dc.stop_loss_percent(), dc.trailing_stop_percent(),
            dc.max_hold_seconds(),
            positions.len(), pending,
        );
        self.send_msg(&text).await;
    }

    async fn cmd_positions(&self) {
        let positions = self.auto_sell.get_active_positions();
        if positions.is_empty() {
            self.send_msg("📭 当前无持仓").await;
            return;
        }
        let text = self.format_position_list(&positions);
        let kb = position_list_keyboard(&positions);
        self.send_msg_kb(&text, Some(kb)).await;
    }

    async fn cmd_stats(&self) {
        let up = self.stats.started_at.elapsed().as_secs();
        let text = format!(
            "📈 <b>运行统计</b>\n\
             \n\
             运行时间: {}\n\
             gRPC 事件: {}\n\
             买入: {} 次 | 成功: {} | 失败: {}\n\
             当前持仓: {} 个",
            fmt_time(up),
            self.stats.grpc_events.load(Ordering::Relaxed),
            self.stats.buy_attempts.load(Ordering::Relaxed),
            self.stats.buy_success.load(Ordering::Relaxed),
            self.stats.buy_failed.load(Ordering::Relaxed),
            self.auto_sell.position_count(),
        );
        self.send_msg(&text).await;
    }

    async fn cmd_sellall(&self) {
        let positions = self.auto_sell.get_active_positions();
        if positions.is_empty() {
            self.send_msg("📭 无持仓可清").await;
            return;
        }
        let count = positions.len();
        for pos in positions {
            let signal = SellSignal {
                token_mint: pos.token_mint,
                reason: SellReason::Manual,
                current_price: pos.current_price,
                pnl_percent: pos.pnl_percent(),
            };
            let _ = self.sell_signal_tx.send(signal);
        }
        self.send_msg(&format!("🔴 已发送 {} 个清仓信号", count)).await;
    }

    async fn cmd_sellmode(&self) {
        let current = self.dyn_config.sell_mode();
        let (cur_text, other_text, other_data) = if current == SELL_MODE_FOLLOW {
            ("跟卖模式 (聪明钱卖出时跟卖)", "切换到止盈止损模式", "sellmode:tpsl")
        } else {
            ("止盈止损模式 (TP/SL/追踪止损)", "切换到跟卖模式", "sellmode:follow")
        };
        let text = format!(
            "🔄 <b>卖出模式</b>\n\n当前: <b>{}</b>\n\n两种模式互斥，只能启用一种:",
            cur_text,
        );
        let kb = serde_json::json!({
            "inline_keyboard": [[
                {"text": other_text, "callback_data": other_data},
            ]]
        });
        self.send_msg_kb(&text, Some(kb)).await;
    }

    async fn cmd_sellmode_edit(&self, mid: i64) {
        let current = self.dyn_config.sell_mode();
        let (cur_text, other_text, other_data) = if current == SELL_MODE_FOLLOW {
            ("跟卖模式 (聪明钱卖出时跟卖)", "切换到止盈止损模式", "sellmode:tpsl")
        } else {
            ("止盈止损模式 (TP/SL/追踪止损)", "切换到跟卖模式", "sellmode:follow")
        };
        let text = format!(
            "🔄 <b>卖出模式</b>\n\n当前: <b>{}</b>\n\n两种模式互斥，只能启用一种:",
            cur_text,
        );
        let kb = serde_json::json!({
            "inline_keyboard": [[
                {"text": other_text, "callback_data": other_data},
            ]]
        });
        self.edit_msg(mid, &text, Some(kb)).await;
    }

    async fn cmd_set(&self, args: &[&str]) {
        let dc = &self.dyn_config;
        if args.is_empty() {
            let text = format!(
                "⚙️ <b>当前配置</b>\n\
                 \n\
                 buy = {} SOL\n\
                 min_buy = {} SOL\n\
                 tp = {}%\n\
                 sl = {}%\n\
                 trailing = {}%\n\
                 slippage = {} bps\n\
                 sell_slippage = {} bps\n\
                 consensus = {}\n\
                 hold = {} 分钟\n\
                 tip_buy = {} lamports\n\
                 tip_sell = {} lamports\n\
                 \n\
                 用法: <code>/set buy 0.01</code>",
                dc.buy_sol_amount(),
                dc.min_target_buy_sol(),
                dc.take_profit_percent(), dc.stop_loss_percent(), dc.trailing_stop_percent(),
                dc.slippage_bps(), dc.sell_slippage_bps(),
                dc.consensus_min_wallets(),
                dc.max_hold_seconds() / 60,
                dc.jito_buy_tip_lamports(), dc.jito_sell_tip_lamports(),
            );
            self.send_msg(&text).await;
            return;
        }
        if args.len() < 2 {
            self.send_msg("用法: /set key value\n例: /set sl 20").await;
            return;
        }
        let key = args[0].to_lowercase();
        // 支持 "/set sl = 20%" 和 "/set sl 20" 两种格式
        let raw_val = if args.len() >= 3 && args[1] == "=" {
            if args.len() >= 3 { args[2] } else { args[1] }
        } else {
            args[1]
        };
        // 去掉尾部的 % 符号
        let val_str = raw_val.trim_end_matches('%');

        let result: Result<String, String> = match key.as_str() {
            "buy" => val_str.parse::<f64>().map(|v| { dc.set_buy_sol_amount(v); format!("buy = {} SOL", v) }).map_err(|e| e.to_string()),
            "min_buy" => val_str.parse::<f64>().map(|v| { dc.set_min_target_buy_sol(v); format!("min_buy = {} SOL", v) }).map_err(|e| e.to_string()),
            "tp" => val_str.parse::<f64>().map(|v| { dc.set_take_profit_percent(v); format!("tp = {}%", v) }).map_err(|e| e.to_string()),
            "sl" => val_str.parse::<f64>().map(|v| { dc.set_stop_loss_percent(v.abs()); format!("sl = {}%", v.abs()) }).map_err(|e| e.to_string()),
            "trailing" => val_str.parse::<f64>().map(|v| { dc.set_trailing_stop_percent(v); format!("trailing = {}%", v) }).map_err(|e| e.to_string()),
            "slippage" => val_str.parse::<u64>().map(|v| { dc.set_slippage_bps(v); format!("slippage = {} bps", v) }).map_err(|e| e.to_string()),
            "sell_slippage" => val_str.parse::<u64>().map(|v| { dc.set_sell_slippage_bps(v); format!("sell_slippage = {} bps", v) }).map_err(|e| e.to_string()),
            "consensus" => val_str.parse::<usize>().map(|v| { dc.set_consensus_min_wallets(v); format!("consensus = {}", v) }).map_err(|e| e.to_string()),
            "hold" => val_str.parse::<u64>().map(|v| { dc.set_max_hold_seconds(v * 60); format!("hold = {} 分钟 ({}s)", v, v * 60) }).map_err(|e| e.to_string()),
            "tip_buy" => val_str.parse::<u64>().map(|v| { dc.set_jito_buy_tip_lamports(v); format!("tip_buy = {} lamports", v) }).map_err(|e| e.to_string()),
            "tip_sell" => val_str.parse::<u64>().map(|v| { dc.set_jito_sell_tip_lamports(v); format!("tip_sell = {} lamports", v) }).map_err(|e| e.to_string()),
            _ => Err(format!("未知参数: {}", key)),
        };

        match result {
            Ok(msg) => self.send_msg(&format!("✅ 已修改: {}", msg)).await,
            Err(e) => self.send_msg(&format!("❌ {}", e)).await,
        };
    }

    async fn cmd_wallet(&self) {
        let rpc_url = self.config.rpc_url.clone();
        let pubkey = self.config.pubkey;
        let balance = tokio::task::spawn_blocking(move || {
            let rpc = solana_client::rpc_client::RpcClient::new(rpc_url);
            rpc.get_balance(&pubkey).unwrap_or(0)
        }).await.unwrap_or(0);
        self.send_msg(&format!(
            "💰 <b>钱包</b>\n\n地址: <code>{}</code>\nSOL: {:.4}",
            self.config.pubkey, balance as f64 / 1e9,
        )).await;
    }

    async fn cmd_wallets(&self) {
        let wallets = self.dyn_config.target_wallets.read().unwrap().clone();
        if wallets.is_empty() {
            self.send_msg("📭 无跟踪钱包\n\n发送 /addwallet 地址 添加").await;
            return;
        }
        let mut text = format!("👛 <b>Smart Money 钱包</b> ({}个)\n\n", wallets.len());
        for (i, w) in wallets.iter().enumerate() {
            let ws = w.to_string();
            text.push_str(&format!("{}. <code>{}..{}</code>\n", i + 1, &ws[..6], &ws[ws.len()-4..]));
        }
        text.push_str("\n发送 /addwallet 地址 添加\n点击下方按钮删除钱包");
        let kb = wallets_keyboard(&wallets);
        self.send_msg_kb(&text, Some(kb)).await;
    }

    async fn cmd_addwallet(&self, args: &[&str]) {
        if args.is_empty() {
            self.send_msg("用法: /addwallet 地址").await;
            return;
        }
        let msg = match args[0].parse::<Pubkey>() {
            Ok(pk) => {
                let mut wallets = self.dyn_config.target_wallets.write().unwrap();
                if wallets.contains(&pk) {
                    "⚠️ 钱包已存在".to_string()
                } else {
                    wallets.push(pk);
                    "✅ 已添加钱包\n⚠️ 需要 /stop → /start 重新订阅 gRPC".to_string()
                }
            }
            Err(_) => "❌ 无效地址".to_string(),
        };
        self.send_msg(&msg).await;
    }

    async fn cmd_rmwallet(&self, args: &[&str]) {
        if args.is_empty() {
            self.send_msg("用法: /rmwallet 地址或序号").await;
            return;
        }
        let msg = {
            let mut wallets = self.dyn_config.target_wallets.write().unwrap();
            if let Ok(idx) = args[0].parse::<usize>() {
                if idx >= 1 && idx <= wallets.len() {
                    let removed = wallets.remove(idx - 1);
                    format!("✅ 已删除: <code>{}</code>", removed)
                } else {
                    "❌ 序号超出范围".to_string()
                }
            } else if let Ok(pk) = args[0].parse::<Pubkey>() {
                if let Some(pos) = wallets.iter().position(|w| w == &pk) {
                    wallets.remove(pos);
                    "✅ 已删除钱包".to_string()
                } else {
                    "⚠️ 钱包不在列表中".to_string()
                }
            } else {
                "❌ 无效输入".to_string()
            }
        };
        self.send_msg(&msg).await;
    }

    async fn cmd_block(&self, args: &[&str]) {
        if args.is_empty() {
            self.send_msg("用法: /block mint地址").await;
            return;
        }
        match args[0].parse::<Pubkey>() {
            Ok(pk) => {
                self.dyn_config.blocklist.insert(pk);
                self.send_msg(&format!("🚫 已拉黑: <code>{}..{}</code>", &args[0][..6], &args[0][args[0].len()-4..])).await;
            }
            Err(_) => { self.send_msg("❌ 无效地址").await; }
        }
    }

    async fn cmd_unblock(&self, args: &[&str]) {
        if args.is_empty() {
            self.send_msg("用法: /unblock mint地址").await;
            return;
        }
        match args[0].parse::<Pubkey>() {
            Ok(pk) => {
                self.dyn_config.blocklist.remove(&pk);
                self.send_msg("✅ 已取消拉黑").await;
            }
            Err(_) => { self.send_msg("❌ 无效地址").await; }
        }
    }

    async fn cmd_blocklist(&self) {
        let list: Vec<String> = self.dyn_config.blocklist.iter().map(|p| p.to_string()).collect();
        if list.is_empty() {
            self.send_msg("📭 黑名单为空").await;
        } else {
            let mut text = "🚫 <b>黑名单</b>\n\n".to_string();
            for m in &list {
                text.push_str(&format!("<code>{}</code>\n", m));
            }
            self.send_msg(&text).await;
        }
    }

    async fn cmd_pnl(&self) {
        let positions = self.auto_sell.get_active_positions();
        if positions.is_empty() {
            self.send_msg("📭 当前无持仓，无盈亏数据").await;
            return;
        }
        let sol_price = self.sol_usd.get();
        let mut total_cost_sol = 0.0_f64;
        let mut total_value_sol = 0.0_f64;
        let mut count = 0;
        for pos in &positions {
            if pos.entry_price_sol > 0.0 && pos.current_price > 0.0 {
                let cost = pos.entry_sol_amount as f64 / 1e9;
                let value = cost * (1.0 + pos.pnl_percent() / 100.0);
                total_cost_sol += cost;
                total_value_sol += value;
                count += 1;
            }
        }
        let total_pnl_sol = total_value_sol - total_cost_sol;
        let total_pnl_pct = if total_cost_sol > 0.0 { (total_pnl_sol / total_cost_sol) * 100.0 } else { 0.0 };
        let sign = if total_pnl_sol >= 0.0 { "+" } else { "" };
        let icon = if total_pnl_sol >= 0.0 { "🟢" } else { "🔴" };
        self.send_msg(&format!(
            "📊 <b>盈亏汇总</b>\n\n\
             持仓数: {}\n\
             总投入: {:.4} SOL (${:.2})\n\
             总价值: {:.4} SOL (${:.2})\n\
             {} 总PnL: {}{:.4} SOL ({}{:.2}%)",
            count,
            total_cost_sol, total_cost_sol * sol_price,
            total_value_sol, total_value_sol * sol_price,
            icon, sign, total_pnl_sol, sign, total_pnl_pct,
        )).await;
    }

    async fn show_buy_buttons(&self, mint: &Pubkey) {
        let mint_str = mint.to_string();
        let short = format!("{}..{}", &mint_str[..6], &mint_str[mint_str.len()-4..]);
        let text = format!(
            "🪙 代币: <code>{}</code>\n\n选择买入金额:",
            mint_str,
        );
        let kb = serde_json::json!({
            "inline_keyboard": [
                [
                    {"text": "0.003 SOL", "callback_data": format!("buy:3000000:{}", mint_str)},
                    {"text": "0.005 SOL", "callback_data": format!("buy:5000000:{}", mint_str)},
                ],
                [
                    {"text": "0.01 SOL", "callback_data": format!("buy:10000000:{}", mint_str)},
                    {"text": "0.05 SOL", "callback_data": format!("buy:50000000:{}", mint_str)},
                ],
            ]
        });
        self.send_msg_kb(&text, Some(kb)).await;
    }

    // ============================================
    // 按钮回调
    // ============================================

    async fn cb_refresh(&self, cb_id: &str, mid: i64, mint_str: &str) {
        let mint = match mint_str.parse::<Pubkey>() {
            Ok(m) => m,
            Err(_) => { self.answer_cb(cb_id, "无效地址").await; return; }
        };
        match self.auto_sell.get_position(&mint) {
            Some(pos) => {
                let text = format_position_card(&pos, self.sol_usd.get());
                let kb = position_keyboard(mint_str);
                self.edit_msg(mid, &text, Some(kb)).await;
                self.answer_cb(cb_id, "已刷新").await;
            }
            None => { self.answer_cb(cb_id, "仓位已关闭").await; }
        }
    }

    async fn cb_refresh_pos_list(&self, cb_id: &str, mid: i64) {
        let positions = self.auto_sell.get_active_positions();
        if positions.is_empty() {
            self.edit_msg(mid, "📭 当前无持仓", None).await;
            self.answer_cb(cb_id, "已刷新").await;
            return;
        }
        let text = self.format_position_list(&positions);
        let kb = position_list_keyboard(&positions);
        self.edit_msg(mid, &text, Some(kb)).await;
        self.answer_cb(cb_id, "已刷新").await;
    }

    // ============================================
    // 通知事件
    // ============================================

    async fn handle_event(&self, event: TgEvent) {
        match event {
            TgEvent::ConsensusReached { mint, wallets } => {
                let ms = mint.to_string();
                let wl: Vec<String> = wallets.iter().map(|w| { let s = w.to_string(); format!("{}..{}", &s[..4], &s[s.len()-4..]) }).collect();
                self.send_msg(&format!(
                    "🔥 <b>共识达成!</b>\n\n代币: <code>{}</code>\n钱包: {}\n<a href=\"https://gmgn.ai/sol/token/{}\">GMGN</a> | <a href=\"https://solscan.io/token/{}\">Solscan</a>",
                    ms, wl.join(", "), ms, ms,
                )).await;
            }
            TgEvent::BuySubmitted { mint, sol_amount, latency_ms } => {
                let ms = mint.to_string();
                self.send_msg(&format!(
                    "🛒 <b>买入已提交</b>\n\n代币: <code>{}..{}</code>\n金额: {:.4} SOL | 延迟: {}ms",
                    &ms[..6], &ms[ms.len()-4..], sol_amount, latency_ms,
                )).await;
            }
            TgEvent::BuyConfirmed { mint, token_name, spent_sol, cost_price_usd, mcap_usd } => {
                let ms = mint.to_string();
                // 买入确认带分档卖出按钮
                let text = format!(
                    "✅ <b>买入确认: {}</b>\n\n花费: {:.4} SOL\n成本价: {}\n市值: {}\n<a href=\"https://gmgn.ai/sol/token/{}\">GMGN</a>",
                    token_name, spent_sol, cost_price_usd, mcap_usd, ms,
                );
                let kb = position_keyboard(&ms);
                self.send_msg_kb(&text, Some(kb)).await;
                self.stats.buy_success.fetch_add(1, Ordering::Relaxed);
            }
            TgEvent::BuyFailed { mint, reason } => {
                let ms = mint.to_string();
                self.send_msg(&format!(
                    "❌ <b>买入失败</b>\n\n代币: <code>{}..{}</code>\n原因: {}",
                    &ms[..6], &ms[ms.len()-4..], reason,
                )).await;
                self.stats.buy_failed.fetch_add(1, Ordering::Relaxed);
            }
            TgEvent::SellSuccess { mint: _, token_name, reason, pnl_percent, tx_sig } => {
                let sign = if pnl_percent >= 0.0 { "+" } else { "" };
                self.send_msg(&format!(
                    "💰 <b>卖出成功</b>\n\n代币: {}\n原因: {}\nPnL: {}{:.2}%\n<a href=\"https://solscan.io/tx/{}\">TX</a>",
                    token_name, reason, sign, pnl_percent, tx_sig,
                )).await;
            }
            TgEvent::SellFailed { mint, reason } => {
                let ms = mint.to_string();
                self.send_msg(&format!(
                    "⚠️ <b>卖出失败</b>\n\n代币: <code>{}..{}</code>\n原因: {}",
                    &ms[..6], &ms[ms.len()-4..], reason,
                )).await;
            }
        }
    }

    /// 格式化持仓列表（详细版）
    fn format_position_list(&self, positions: &[crate::autosell::Position]) -> String {
        let sol_price = self.sol_usd.get();
        let mut text = format!("📊 <b>持仓列表</b> ({}个)\n", positions.len());
        for (i, pos) in positions.iter().enumerate() {
            let ms = pos.token_mint.to_string();
            let pnl = pos.pnl_percent();
            let icon = if pnl >= 0.0 { "🟢" } else { "🔴" };
            let sign = if pnl >= 0.0 { "+" } else { "" };
            let held = fmt_time(pos.held_seconds());
            let name = if pos.token_name.is_empty() {
                format!("{}..{}", &ms[..6], &ms[ms.len()-4..])
            } else {
                pos.token_name.clone()
            };

            // 成本价
            let cost_usd = format_price_gmgn(pos.entry_price_sol * sol_price);
            // 入场市值
            let entry_mcap = if pos.entry_mcap_sol > 0.0 {
                format_mcap_usd(pos.entry_mcap_sol * sol_price)
            } else {
                "N/A".to_string()
            };
            // 当前市值（从当前价格推算: current_price * 10亿总供应）
            let current_mcap_sol = pos.current_price * 1_000_000_000.0;
            let current_mcap = format_mcap_usd(current_mcap_sol * sol_price);

            text.push_str(&format!(
                "\n{}. {} <b>{}</b>\n   成本价: {} | 入场市值: {}\n   当前市值: {} | PnL: {}{:.2}% | {}\n",
                i + 1, icon, name, cost_usd, entry_mcap,
                current_mcap, sign, pnl, held,
            ));
        }
        text
    }
}

// ============================================
// 辅助函数
// ============================================

fn format_position_card(pos: &crate::autosell::Position, sol_price: f64) -> String {
    let ms = pos.token_mint.to_string();
    let pnl = pos.pnl_percent();
    let held = fmt_time(pos.held_seconds());
    let spent = pos.entry_sol_amount as f64 / 1e9;
    let state = if pos.state == crate::autosell::PositionState::Active { "✅" } else { "⏳" };
    let icon = if pnl >= 0.0 { "🟢" } else { "🔴" };
    let sign = if pnl >= 0.0 { "+" } else { "" };
    let name = if pos.token_name.is_empty() {
        format!("{}..{}", &ms[..6], &ms[ms.len()-4..])
    } else {
        pos.token_name.clone()
    };
    let cost_usd = format_price_gmgn(pos.entry_price_sol * sol_price);
    let entry_mcap = if pos.entry_mcap_sol > 0.0 {
        format_mcap_usd(pos.entry_mcap_sol * sol_price)
    } else {
        "N/A".to_string()
    };
    let current_mcap_sol = pos.current_price * 1_000_000_000.0;
    let current_mcap = format_mcap_usd(current_mcap_sol * sol_price);

    format!(
        "{} <b>{}</b> {}\n花费: {:.4} SOL | 成本价: {}\n入场市值: {} | 当前市值: {}\nPnL: {}{:.2}% | 持仓: {}",
        icon, name, state, spent, cost_usd, entry_mcap, current_mcap, sign, pnl, held,
    )
}

fn position_keyboard(mint_str: &str) -> serde_json::Value {
    serde_json::json!({
        "inline_keyboard": [
            [
                {"text": "25%", "callback_data": format!("sell:25:{}", mint_str)},
                {"text": "50%", "callback_data": format!("sell:50:{}", mint_str)},
                {"text": "75%", "callback_data": format!("sell:75:{}", mint_str)},
                {"text": "100%", "callback_data": format!("sell:100:{}", mint_str)},
            ],
            [
                {"text": "🔄 刷新", "callback_data": format!("refresh:{}", mint_str)},
                {"text": "📊 GMGN", "url": format!("https://gmgn.ai/sol/token/{}", mint_str)},
            ],
        ]
    })
}

/// 持仓列表键盘：每个代币一行名称 + 分档卖出按钮 + 底部刷新
fn position_list_keyboard(positions: &[crate::autosell::Position]) -> serde_json::Value {
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for pos in positions {
        let ms = pos.token_mint.to_string();
        let name = if pos.token_name.is_empty() {
            format!("{}..{}", &ms[..6], &ms[ms.len()-4..])
        } else {
            if pos.token_name.len() > 10 {
                format!("{}...", &pos.token_name[..10])
            } else {
                pos.token_name.clone()
            }
        };
        // 代币名称行
        rows.push(serde_json::json!([
            {"text": format!("📌 {}", name), "callback_data": format!("noop:{}", ms)},
            {"text": "📊", "url": format!("https://gmgn.ai/sol/token/{}", ms)},
        ]));
        // 分档卖出按钮行
        rows.push(serde_json::json!([
            {"text": "20%", "callback_data": format!("sell:20:{}", ms)},
            {"text": "50%", "callback_data": format!("sell:50:{}", ms)},
            {"text": "75%", "callback_data": format!("sell:75:{}", ms)},
            {"text": "全部", "callback_data": format!("sell:100:{}", ms)},
        ]));
    }
    rows.push(serde_json::json!([
        {"text": "🔄 刷新列表", "callback_data": "refresh_pos_list"},
    ]));
    serde_json::json!({"inline_keyboard": rows})
}

/// 钱包列表键盘：每个钱包一个删除按钮
fn wallets_keyboard(wallets: &[Pubkey]) -> serde_json::Value {
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for (i, w) in wallets.iter().enumerate() {
        let ws = w.to_string();
        let short = format!("{}..{}", &ws[..6], &ws[ws.len()-4..]);
        rows.push(serde_json::json!([
            {"text": format!("❌ 删除 #{} {}", i + 1, short), "callback_data": format!("rmw:{}", ws)},
        ]));
    }
    serde_json::json!({"inline_keyboard": rows})
}

fn fmt_time(secs: u64) -> String {
    if secs >= 3600 { format!("{}h{}m", secs / 3600, (secs % 3600) / 60) }
    else if secs >= 60 { format!("{}m{}s", secs / 60, secs % 60) }
    else { format!("{}s", secs) }
}

/// 独立函数：进程关闭时发送 TG 下线通知（不依赖 TgBot 实例）
pub async fn send_shutdown_notification(bot_token: &str, chat_id: &str) {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": "🔴 <b>跟单机器人已下线</b>\n\n请在 VPS 重新启动脚本",
        "parse_mode": "HTML",
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    if let Ok(client) = client {
        let _ = client.post(&url).json(&body).send().await;
    }
}
