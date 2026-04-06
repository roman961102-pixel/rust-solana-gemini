use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// 单条买入信号
#[derive(Debug, Clone)]
pub struct BuySignal {
    pub token_mint: Pubkey,
    pub wallet: Pubkey,
    pub detected_at: Instant,
    pub signature: String,
}

/// 共识触发事件（发送给主循环执行跟单）
#[derive(Debug, Clone)]
pub struct ConsensusTrigger {
    pub token_mint: Pubkey,
    /// 触发共识的所有钱包
    pub wallets: Vec<Pubkey>,
    /// 最早的一笔信号签名（用于日志）
    pub first_signature: String,
    pub triggered_at: Instant,
}

/// 某个 token 的信号聚合
#[derive(Debug, Clone)]
struct TokenSignals {
    signals: Vec<BuySignal>,
    /// 是否已经触发过（防止重复触发）
    triggered: bool,
}

/// 共识引擎
/// 记录每个 token 的买入信号，当同一 token 被 N 个不同钱包买入时触发
pub struct ConsensusEngine {
    /// token_mint → 信号列表
    signals: Arc<DashMap<Pubkey, TokenSignals>>,
    /// 触发所需的最少钱包数
    min_wallets: usize,
    /// 信号有效期（秒）
    timeout: Duration,
}

impl ConsensusEngine {
    pub fn new(min_wallets: usize, timeout_secs: u64) -> Self {
        info!(
            "共识引擎初始化: 最少 {} 个钱包, 窗口 {}s",
            min_wallets, timeout_secs
        );
        Self {
            signals: Arc::new(DashMap::new()),
            min_wallets,
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    /// 提交一条买入信号，返回是否触发共识
    pub fn submit_signal(
        &self,
        signal: BuySignal,
        trigger_tx: &mpsc::UnboundedSender<ConsensusTrigger>,
    ) {
        let mint = signal.token_mint;
        let wallet = signal.wallet;
        let now = Instant::now();

        let mut entry = self.signals.entry(mint).or_insert_with(|| TokenSignals {
            signals: Vec::new(),
            triggered: false,
        });

        // 已经触发过的 token 不重复触发
        if entry.triggered {
            debug!("跳过: {} 已触发共识", mint);
            return;
        }

        // 清理过期信号
        entry
            .signals
            .retain(|s| now.duration_since(s.detected_at) < self.timeout);

        // 去重：同一钱包不重复计数
        let already_has = entry.signals.iter().any(|s| s.wallet == wallet);
        if already_has {
            debug!("跳过: 钱包 {}.. 已记录 {}", &wallet.to_string()[..8], mint);
            return;
        }

        // 添加新信号
        info!(
            "信号记录: {} 买入 {} | 当前 {}/{} 个钱包",
            &wallet.to_string()[..8],
            &mint.to_string()[..12],
            entry.signals.len() + 1,
            self.min_wallets,
        );
        entry.signals.push(signal);

        // 检查是否达到共识
        let unique_wallets: Vec<Pubkey> =
            entry.signals.iter().map(|s| s.wallet).collect();

        if unique_wallets.len() >= self.min_wallets {
            entry.triggered = true;

            let first_sig = entry
                .signals
                .first()
                .map(|s| s.signature.clone())
                .unwrap_or_default();

            let trigger = ConsensusTrigger {
                token_mint: mint,
                wallets: unique_wallets.clone(),
                first_signature: first_sig,
                triggered_at: Instant::now(),
            };

            info!(
                "━━ 共识达成! ━━ {} | {} 个钱包: [{}]",
                &mint.to_string()[..12],
                unique_wallets.len(),
                unique_wallets
                    .iter()
                    .map(|w| format!("{}..", &w.to_string()[..6]))
                    .collect::<Vec<_>>()
                    .join(", "),
            );

            if trigger_tx.send(trigger).is_err() {
                warn!("共识通道已关闭");
            }
        }
    }

    /// 撤回信号：某钱包卖出了该 token，从共识中移除其买入信号
    /// 仅在共识未触发时生效（已触发的不可撤回，买入已执行）
    /// 返回是否成功撤回
    pub fn revoke_signal(&self, mint: &Pubkey, wallet: &Pubkey) -> bool {
        if let Some(mut entry) = self.signals.get_mut(mint) {
            // 已触发的共识不撤回（买入已执行）
            if entry.triggered {
                debug!(
                    "撤回跳过: {} 已触发共识，无法撤回 {}.. 的信号",
                    &mint.to_string()[..12],
                    &wallet.to_string()[..8],
                );
                return false;
            }

            let before = entry.signals.len();
            entry.signals.retain(|s| s.wallet != *wallet);
            let after = entry.signals.len();

            if before != after {
                info!(
                    "共识撤回: {}.. 卖出 {} | 信号 {} → {} / {} 个钱包",
                    &wallet.to_string()[..8],
                    &mint.to_string()[..12],
                    before,
                    after,
                    self.min_wallets,
                );
                return true;
            }
        }
        false
    }

    /// 启动过期信号清理任务（每 10 秒清理一次）
    pub fn start_cleanup_task(&self) -> tokio::task::JoinHandle<()> {
        let signals = self.signals.clone();
        let timeout = self.timeout;

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;

                let now = Instant::now();
                let mut removed = 0u32;

                signals.retain(|_mint, token_sigs| {
                    // 清理已触发 + 全部过期的条目
                    if token_sigs.triggered {
                        let all_expired = token_sigs
                            .signals
                            .iter()
                            .all(|s| now.duration_since(s.detected_at) > timeout * 2);
                        if all_expired {
                            removed += 1;
                            return false; // 移除
                        }
                    }

                    // 清理未触发但全部过期的条目
                    token_sigs
                        .signals
                        .retain(|s| now.duration_since(s.detected_at) < timeout);
                    if token_sigs.signals.is_empty() && !token_sigs.triggered {
                        removed += 1;
                        return false;
                    }

                    true
                });

                if removed > 0 {
                    debug!("清理了 {} 个过期信号组", removed);
                }
            }
        })
    }

    /// 当前待决信号数量
    pub fn pending_count(&self) -> usize {
        self.signals.iter().filter(|e| !e.triggered).count()
    }
}
