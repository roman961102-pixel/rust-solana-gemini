use solana_sdk::pubkey::Pubkey;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::autosell::{AutoSellManager, SellReason, SellSignal};
use crate::config::{AppConfig, DynConfig};
use crate::consensus::ConsensusEngine;
use crate::tx::sell_executor::SellExecutor;

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
// Mute Flags (bitfield)
// ============================================

const MUTE_SIGNALS: u8 = 1;
const MUTE_BUYS: u8 = 2;
const MUTE_SELLS: u8 = 4;

struct MuteState(AtomicU8);

impl MuteState {
    fn new() -> Self { Self(AtomicU8::new(0)) }
    fn is_muted(&self, flag: u8) -> bool { self.0.load(Ordering::Relaxed) & flag != 0 }
    fn mute(&self, flag: u8) { self.0.fetch_or(flag, Ordering::Relaxed); }
    fn mute_all(&self) { self.0.store(MUTE_SIGNALS | MUTE_BUYS | MUTE_SELLS, Ordering::Relaxed); }
    fn unmute_all(&self) { self.0.store(0, Ordering::Relaxed); }
    fn signals_muted(&self) -> bool { self.is_muted(MUTE_SIGNALS) }
    fn buys_muted(&self) -> bool { self.is_muted(MUTE_BUYS) }
    fn sells_muted(&self) -> bool { self.is_muted(MUTE_SELLS) }
    fn status_text(&self) -> String {
        let v = self.0.load(Ordering::Relaxed);
        if v == 0 { return "全部开启".into(); }
        let mut parts = vec![];
        if v & MUTE_SIGNALS != 0 { parts.push("共识"); }
        if v & MUTE_BUYS != 0 { parts.push("买入"); }
        if v & MUTE_SELLS != 0 { parts.push("卖出"); }
        format!("已静音: {}", parts.join(", "))
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
    mute: MuteState,
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
        event_rx: mpsc::UnboundedReceiver<TgEvent>,
    ) -> Self {
        let token = config.telegram_bot_token.clone().unwrap_or_default();
        let chat_id = config.telegram_chat_id.clone().unwrap_or_default();
        Self {
            token, chat_id,
            http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(60)).build().unwrap(),
            auto_sell, consensus, config, dyn_config,
            sell_signal_tx, sell_executor,
            is_running, stats,
            mute: MuteState::new(),
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
                {"command": "pos", "description": "查看持仓"},
                {"command": "sellall", "description": "一键清仓"},
                {"command": "stats", "description": "运行统计"},
                {"command": "set", "description": "查看/修改配置"},
                {"command": "wallet", "description": "查看余额"},
                {"command": "wallets", "description": "跟踪钱包列表"},
                {"command": "block", "description": "拉黑代币"},
                {"command": "blocklist", "description": "黑名单列表"},
                {"command": "mute", "description": "静音通知"},
                {"command": "unmute", "description": "恢复通知"},
                {"command": "pnl", "description": "盈亏汇总"},
                {"command": "history", "description": "交易记录"},
            ]
        });
        match self.http.post(&url).json(&commands).send().await {
            Ok(resp) => {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    if v["ok"].as_bool() == Some(true) {
                        info!("TG Bot Menu 命令已注册 (15 个)");
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
            Ok(r) => r.json::<serde_json::Value>().await.ok(),
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
                let cmd = parts.first().copied().unwrap_or("");
                match cmd {
                    "/start" => self.cmd_start().await,
                    "/stop" => self.cmd_stop().await,
                    "/status" => self.cmd_status().await,
                    // 以下命令需要 BOT 在线（is_running=true）
                    _ if !self.is_running.load(Ordering::Relaxed) && matches!(cmd,
                        "/pos" | "/sellall" | "/set" | "/stats" | "/wallet" | "/wallets" |
                        "/addwallet" | "/rmwallet" | "/block" | "/unblock" | "/blocklist" |
                        "/mute" | "/unmute" | "/pnl" | "/history"
                    ) => {
                        self.send_msg("⚠️ 监听未启动，请先发送 /start").await;
                    }
                    "/pos" => self.cmd_positions().await,
                    "/stats" => self.cmd_stats().await,
                    "/sellall" => self.cmd_sellall().await,
                    "/set" => self.cmd_set(&parts[1..]).await,
                    "/wallet" => self.cmd_wallet().await,
                    "/wallets" => self.cmd_wallets().await,
                    "/addwallet" => self.cmd_addwallet(&parts[1..]).await,
                    "/rmwallet" => self.cmd_rmwallet(&parts[1..]).await,
                    "/block" => self.cmd_block(&parts[1..]).await,
                    "/unblock" => self.cmd_unblock(&parts[1..]).await,
                    "/blocklist" => self.cmd_blocklist().await,
                    "/mute" => self.cmd_mute(&parts[1..]).await,
                    "/unmute" => { self.mute.unmute_all(); self.send_msg("🔔 所有通知已恢复").await; }
                    "/pnl" => self.cmd_pnl().await,
                    "/history" => self.cmd_history().await,
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

        let text = format!(
            "📊 <b>运行状态</b>\n\
             \n\
             状态: {}\n\
             钱包: <code>{}</code>\n\
             买入: {} SOL | 滑点: {} bps\n\
             卖出滑点: {} bps\n\
             止盈: {}% | 止损: {}% | 追踪: {}%\n\
             最大持仓: {}s\n\
             仓位: {} | 待共识: {}\n\
             通知: {}",
            if running { "🟢 运行中" } else { "🔴 已停止" },
            self.config.pubkey,
            dc.buy_sol_amount(), dc.slippage_bps(),
            dc.sell_slippage_bps(),
            dc.take_profit_percent(), dc.stop_loss_percent(), dc.trailing_stop_percent(),
            dc.max_hold_seconds(),
            positions.len(), pending,
            self.mute.status_text(),
        );
        self.send_msg(&text).await;
    }

    async fn cmd_positions(&self) {
        let positions = self.auto_sell.get_active_positions();
        if positions.is_empty() {
            self.send_msg("📭 当前无持仓").await;
            return;
        }
        for pos in &positions {
            let text = format_position_card(pos);
            let kb = position_keyboard(&pos.token_mint.to_string());
            self.send_msg_kb(&text, Some(kb)).await;
        }
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

    async fn cmd_set(&self, args: &[&str]) {
        let dc = &self.dyn_config;
        if args.is_empty() {
            let text = format!(
                "⚙️ <b>当前配置</b>\n\
                 \n\
                 buy = {} SOL\n\
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
            self.send_msg("用法: /set <key> <value>").await;
            return;
        }
        let key = args[0].to_lowercase();
        let val_str = args[1];

        let result: Result<String, String> = match key.as_str() {
            "buy" => val_str.parse::<f64>().map(|v| { dc.set_buy_sol_amount(v); format!("buy = {} SOL", v) }).map_err(|e| e.to_string()),
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
        // 显示 SOL 余额
        let rpc = solana_client::rpc_client::RpcClient::new(self.config.rpc_url.clone());
        let balance = rpc.get_balance(&self.config.pubkey).unwrap_or(0);
        self.send_msg(&format!(
            "💰 <b>钱包</b>\n\n地址: <code>{}</code>\nSOL: {:.4}",
            self.config.pubkey, balance as f64 / 1e9,
        )).await;
    }

    async fn cmd_wallets(&self) {
        let text = {
            let wallets = self.dyn_config.target_wallets.read().unwrap();
            if wallets.is_empty() {
                "📭 无跟踪钱包".to_string()
            } else {
                let mut t = "👛 <b>Smart Money 钱包</b>\n\n".to_string();
                for (i, w) in wallets.iter().enumerate() {
                    t.push_str(&format!("{}. <code>{}</code>\n", i + 1, w));
                }
                t
            }
        };
        self.send_msg(&text).await;
    }

    async fn cmd_addwallet(&self, args: &[&str]) {
        if args.is_empty() {
            self.send_msg("用法: /addwallet <地址>").await;
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
            self.send_msg("用法: /rmwallet <地址或序号>").await;
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
            self.send_msg("用法: /block <mint>").await;
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
            self.send_msg("用法: /unblock <mint>").await;
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

    async fn cmd_mute(&self, args: &[&str]) {
        if args.is_empty() {
            self.send_msg("用法: /mute signals|buys|sells|all").await;
            return;
        }
        match args[0] {
            "signals" => { self.mute.mute(MUTE_SIGNALS); self.send_msg("🔇 共识信号已静音").await; }
            "buys" => { self.mute.mute(MUTE_BUYS); self.send_msg("🔇 买入通知已静音").await; }
            "sells" => { self.mute.mute(MUTE_SELLS); self.send_msg("🔇 卖出通知已静音").await; }
            "all" => { self.mute.mute_all(); self.send_msg("🔇 全部已静音").await; }
            _ => { self.send_msg("未知类型，可选: signals buys sells all").await; }
        };
    }

    async fn cmd_pnl(&self) {
        // TODO: 需要 trades.json 持久化支持
        self.send_msg("📊 PnL 统计功能开发中\n需要交易记录持久化支持").await;
    }

    async fn cmd_history(&self) {
        // TODO: 需要 trades.json 持久化支持
        self.send_msg("📜 历史记录功能开发中\n需要交易记录持久化支持").await;
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
                let text = format_position_card(&pos);
                let kb = position_keyboard(mint_str);
                self.edit_msg(mid, &text, Some(kb)).await;
                self.answer_cb(cb_id, "已刷新").await;
            }
            None => { self.answer_cb(cb_id, "仓位已关闭").await; }
        }
    }

    // ============================================
    // 通知事件
    // ============================================

    async fn handle_event(&self, event: TgEvent) {
        match &event {
            TgEvent::ConsensusReached { .. } if self.mute.signals_muted() => return,
            TgEvent::BuySubmitted { .. } | TgEvent::BuyConfirmed { .. } | TgEvent::BuyFailed { .. }
                if self.mute.buys_muted() => return,
            TgEvent::SellSuccess { .. } | TgEvent::SellFailed { .. }
                if self.mute.sells_muted() => return,
            _ => {}
        }

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
}

// ============================================
// 辅助函数
// ============================================

fn format_position_card(pos: &crate::autosell::Position) -> String {
    let ms = pos.token_mint.to_string();
    let pnl = pos.pnl_percent();
    let held = fmt_time(pos.held_seconds());
    let spent = pos.entry_sol_amount as f64 / 1e9;
    let state = if pos.state == crate::autosell::PositionState::Active { "✅" } else { "⏳" };
    let icon = if pnl >= 0.0 { "🟢" } else { "🔴" };
    let sign = if pnl >= 0.0 { "+" } else { "" };

    format!(
        "{} <code>{}..{}</code> {}\n花费: {:.4} SOL\nPnL: {}{:.2}% | 持仓: {}",
        icon, &ms[..6], &ms[ms.len()-4..], state, spent, sign, pnl, held,
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
