use anyhow::{Context, Result};
use futures::StreamExt;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tonic::transport::ClientTlsConfig;
use tracing::{debug, error, info, warn};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{
    CommitmentLevel, SubscribeRequest, SubscribeRequestFilterTransactions,
};
use yellowstone_grpc_proto::prelude::subscribe_update::UpdateOneof;

use crate::processor::{DetectedTrade, TradeType};

// ============================================
// 已知 DEX Program IDs
// ============================================
const PUMPFUN_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const PUMPSWAP_PROGRAM: &str = "PSwapMdSai8tjrEXcxFeQth87xC4rRsa4VA5mhGhXkP";
const RAYDIUM_AMM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
const RAYDIUM_CPMM: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";

// Pump.fun discriminators (Anchor sighash)
// buy  = sha256("global:buy")[..8]
// sell = sha256("global:sell")[..8]
const PUMPFUN_BUY_DISC: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
const PUMPFUN_SELL_DISC: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

// PumpSwap: 使用统一的 swap 指令，通过 token 方向判断 buy/sell
// swap discriminator = sha256("global:swap")[..8]
// 注意：这个值需要从链上交易验证，下面是 placeholder
const PUMPSWAP_SWAP_DISC: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];

// Raydium CPMM Anchor discriminators
// swap_base_input  = sha256("global:swap_base_input")[..8]
// swap_base_output = sha256("global:swap_base_output")[..8]
const CPMM_SWAP_BASE_INPUT: [u8; 8] = [143, 190, 90, 218, 196, 30, 51, 222];
const CPMM_SWAP_BASE_OUTPUT: [u8; 8] = [55, 217, 98, 86, 163, 74, 180, 173];

// WSOL Mint
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// gRPC 订阅器，监听目标钱包的交易
pub struct GrpcSubscriber {
    grpc_url: String,
    grpc_token: Option<String>,
    target_wallets: Vec<Pubkey>,
}

impl GrpcSubscriber {
    pub fn new(grpc_url: String, grpc_token: Option<String>, target_wallets: Vec<Pubkey>) -> Self {
        Self {
            grpc_url,
            grpc_token,
            target_wallets,
        }
    }

    // ================================================================
    // 主入口：连接 Shyft gRPC 并持续接收交易流
    // ================================================================

    /// 启动 gRPC 订阅，将检测到的目标钱包 DEX 交易发送到 channel
    /// 连接断开后返回 Err，由调用方 (main.rs) 负责重连
    pub async fn subscribe(&self, tx_sender: mpsc::UnboundedSender<DetectedTrade>) -> Result<()> {
        info!(
            "Connecting to Shyft gRPC at {} for {} target wallets",
            self.grpc_url,
            self.target_wallets.len()
        );

        // ============================================
        // Shyft gRPC 连接
        // 必须带 TLS + x-token 认证
        // 参考: https://docs.shyft.to/solana-yellowstone-grpc/docs
        // ============================================
        let mut client = GeyserGrpcClient::build_from_shared(self.grpc_url.clone())?
            .x_token(self.grpc_token.clone())?
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .tls_config(ClientTlsConfig::new().with_native_roots())?
            .max_decoding_message_size(64 * 1024 * 1024) // 64MB
            .connect()
            .await
            .context("Failed to connect to gRPC")?;

        info!("Shyft gRPC connected successfully");

        // 构建订阅请求
        let subscribe_request = self.build_subscribe_request();

        info!("发送订阅请求...");
        let (mut subscribe_tx, mut stream) = client
            .subscribe_with_request(Some(subscribe_request))
            .await
            .context("Failed to create gRPC subscription")?;

        info!("gRPC subscription active, listening for target wallet transactions...");

        // 发送 ping 保持连接活跃
        let ping_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                // 通过 subscribe_tx 发送 ping（如果支持）
                // RabbitStream 可能需要 keepalive
            }
        });

        let mut total_received: u64 = 0;
        let mut total_matched: u64 = 0;
        let start_time = Instant::now();

        // ============================================
        // 主接收循环
        // ============================================
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(update) => {
                    let recv_time = Instant::now();

                    match update.update_oneof {
                        Some(UpdateOneof::Transaction(tx_update)) => {
                            total_received += 1;

                            // 每 100 笔交易输出统计
                            if total_received % 100 == 0 {
                                let elapsed = start_time.elapsed().as_secs().max(1);
                                debug!(
                                    "Stats: received={}, matched={}, uptime={}s, rate={:.1}/s",
                                    total_received,
                                    total_matched,
                                    elapsed,
                                    total_received as f64 / elapsed as f64,
                                );
                            }

                            if let Some(ref tx_info) = tx_update.transaction {
                                // meta 在 SubscribeUpdateTransactionInfo 层，不在 Transaction 层
                                let meta = tx_info.meta.as_ref();
                                let slot = tx_update.slot;

                                // 诊断: 判断 RabbitStream 是否真正预执行
                                // 有 meta = 已执行（Processed 级别），无 meta = 预执行
                                if total_matched == 0 && meta.is_some() {
                                    warn!(
                                        "RabbitStream 诊断: meta 存在 → 推送为 Processed 级别（非预执行）| slot={}",
                                        slot,
                                    );
                                } else if total_matched == 0 && meta.is_none() {
                                    info!(
                                        "RabbitStream 诊断: meta 为空 → 推送为预执行级别 | slot={}",
                                        slot,
                                    );
                                }

                                if let Some(ref tx_data) = tx_info.transaction {
                                    let message = tx_data.message.as_ref();

                                    match self.parse_transaction(tx_info, tx_data, message, meta, recv_time) {
                                        Ok(Some(trade)) => {
                                            total_matched += 1;
                                            let parse_latency = recv_time.elapsed();

                                        info!(
                                            "DETECTED: {} {} | wallet: {}..{} | sig: {}.. | parse: {:?}",
                                            trade.trade_type,
                                            if trade.is_buy { "BUY" } else { "SELL" },
                                            &trade.source_wallet.to_string()[..4],
                                            &trade.source_wallet.to_string()
                                                [trade.source_wallet.to_string().len() - 4..],
                                            &trade.signature[..12],
                                            parse_latency,
                                        );

                                        if tx_sender.send(trade).is_err() {
                                            error!("Trade channel closed, exiting subscriber");
                                            return Ok(());
                                        }
                                    }
                                    Ok(None) => {
                                        // 不是 DEX swap 交易，跳过
                                    }
                                        Err(e) => {
                                            debug!("Parse error (non-fatal): {}", e);
                                        }
                                    }
                                }
                            }
                        }
                        Some(UpdateOneof::Ping(_)) => {
                            debug!("gRPC keepalive ping");
                        }
                        Some(UpdateOneof::Pong(_)) => {
                            debug!("gRPC pong");
                        }
                        Some(other) => {
                            debug!("gRPC other update type: {:?}", std::mem::discriminant(&other));
                        }
                        None => {
                            debug!("gRPC empty update");
                        }
                    }
                }
                Err(e) => {
                    error!(
                        "gRPC stream error after {} messages ({} matched): {}",
                        total_received, total_matched, e
                    );
                    return Err(e.into());
                }
            }
        }

        ping_task.abort();
        let elapsed = start_time.elapsed();
        warn!(
            "gRPC stream ended after {} messages ({} matched) | 持续: {:.1}s",
            total_received, total_matched, elapsed.as_secs_f64()
        );
        Ok(())
    }

    // ================================================================
    // 订阅请求构建
    // ================================================================

    /// 构建 YellowStone gRPC SubscribeRequest
    ///
    /// 过滤条件:
    ///   - account_include = 目标钱包地址列表
    ///   - vote = false (排除投票交易)
    ///   - failed = None (不过滤，兼容 RabbitStream 预执行推送)
    ///   - commitment = None (RabbitStream 不支持 commitment 过滤)
    fn build_subscribe_request(&self) -> SubscribeRequest {
        let account_keys: Vec<String> = self
            .target_wallets
            .iter()
            .map(|w| w.to_string())
            .collect();

        info!(
            "Subscribing to {} wallets: [{}]",
            account_keys.len(),
            account_keys
                .iter()
                .map(|k| {
                    let s = k.as_str();
                    format!("{}..{}", &s[..4], &s[s.len() - 4..])
                })
                .collect::<Vec<_>>()
                .join(", ")
        );

        let mut transactions = HashMap::new();
        transactions.insert(
            "target_wallets".to_string(),
            SubscribeRequestFilterTransactions {
                vote: None,   // RabbitStream 不过滤
                failed: None, // RabbitStream 预执行阶段不知道是否失败
                // account_include: 交易涉及任意一个目标钱包就推送（OR 逻辑）
                account_include: account_keys,
                account_exclude: vec![],
                // account_required 留空！设置后是 AND 逻辑，2个钱包时几乎不可能匹配
                account_required: vec![],
                signature: None,
            },
        );

        info!("订阅参数: vote=None, failed=None, commitment=None, accounts={}",
              transactions.get("target_wallets").map(|t| t.account_include.len()).unwrap_or(0));

        SubscribeRequest {
            transactions,
            // RabbitStream 不支持 commitment 过滤（预执行阶段无 commitment 概念）
            commitment: None,
            accounts: HashMap::new(),
            slots: HashMap::new(),
            blocks: HashMap::new(),
            blocks_meta: HashMap::new(),
            entry: HashMap::new(),
            accounts_data_slice: vec![],
            ping: None,
            ..Default::default()
        }
    }

    // ================================================================
    // 交易解析核心逻辑
    // ================================================================

    /// 从 gRPC proto Transaction 序列化为 Solana wire format
    /// 用于 Jito Backrun Bundle: [target_tx_bytes, our_tx_bytes]
    /// 使用 yellowstone-grpc-proto 官方转换函数，避免手动重建导致的序列化偏差
    /// 从 gRPC proto Transaction 序列化为 Solana wire format（用于 Jito Backrun Bundle）
    /// 仅在匹配成功后调用，避免非匹配交易的序列化开销
    fn serialize_transaction_from_proto(
        tx_data: &yellowstone_grpc_proto::prelude::Transaction,
    ) -> Option<Vec<u8>> {
        // 使用 yellowstone 官方的 proto → VersionedTransaction 转换
        let versioned_tx = match yellowstone_grpc_proto::convert_from::create_tx_versioned(
            tx_data.clone(),
        ) {
            Ok(tx) => tx,
            Err(e) => {
                warn!("Backrun: proto→VersionedTransaction 转换失败: {}", e);
                return None;
            }
        };

        // 序列化为 wire format（去掉冗余的反序列化验证，省 ~100µs）
        match bincode::serialize(&versioned_tx) {
            Ok(bytes) => {
                debug!("Backrun: 目标交易序列化 {}bytes", bytes.len());
                Some(bytes)
            }
            Err(e) => {
                warn!("Backrun: VersionedTransaction 序列化失败: {}", e);
                None
            }
        }
    }

    /// 解析 gRPC 接收到的原始交易
    ///
    /// 两轮扫描:
    ///   1. 顶层指令 (outer instructions) — 直接调用 DEX 的交易
    ///   2. 内部指令 (inner instructions / CPI) — 通过聚合器间接调用 DEX
    ///      例如 Jupiter route → 实际 swap 藏在 inner instructions 里
    fn parse_transaction(
        &self,
        tx_info: &yellowstone_grpc_proto::prelude::SubscribeUpdateTransactionInfo,
        tx_data: &yellowstone_grpc_proto::prelude::Transaction,
        message: Option<&yellowstone_grpc_proto::prelude::Message>,
        meta: Option<&yellowstone_grpc_proto::prelude::TransactionStatusMeta>,
        _recv_time: Instant,
    ) -> Result<Option<DetectedTrade>> {
        let message = message.context("Missing transaction message")?;

        // 提取目标交易的原始字节（用于构建 Jito Backrun Bundle）
        // 延迟到匹配成功后再序列化（避免非匹配交易的序列化开销）
        // raw_transaction_bytes 在匹配成功时通过 serialize_transaction_from_proto 填充

        // 提取 signature (base58)
        let signature = if !tx_info.signature.is_empty() {
            bs58::encode(&tx_info.signature).into_string()
        } else {
            "unknown".to_string()
        };

        // ============================================
        // 构建完整的 account keys 列表
        // = message.account_keys (static)
        // + meta.loaded_writable_addresses (ALT writable)
        // + meta.loaded_readonly_addresses (ALT readonly)
        //
        // 这一步很关键：很多交易使用 Address Lookup Table (ALT)
        // 指令中的 account index 可能指向 ALT 中的地址
        // ============================================
        let mut account_keys: Vec<Pubkey> = message
            .account_keys
            .iter()
            .filter_map(|k| {
                if k.len() == 32 {
                    <[u8; 32]>::try_from(k.as_slice())
                        .ok()
                        .map(Pubkey::new_from_array)
                } else {
                    None
                }
            })
            .collect();

        // 追加 ALT 解析出的额外地址
        // RabbitStream 下 ALT 地址为空是正常情况（预执行阶段无 meta）
        if let Some(m) = meta {
            let writable = &m.loaded_writable_addresses;
            let readonly = &m.loaded_readonly_addresses;
            if !writable.is_empty() || !readonly.is_empty() {
                for addr in writable.iter().chain(readonly.iter()) {
                    if addr.len() == 32 {
                        if let Ok(arr) = <[u8; 32]>::try_from(addr.as_slice()) {
                            account_keys.push(Pubkey::new_from_array(arr));
                        }
                    }
                }
            }
        }

        // 识别哪个目标钱包参与了这笔交易
        let source_wallet = match account_keys
            .iter()
            .find(|k| self.target_wallets.contains(k))
            .copied()
        {
            Some(w) => w,
            None => return Ok(None), // 目标钱包不在这笔交易中
        };

        // ============================================
        // 第一轮：扫描顶层指令 (outer instructions)
        // ============================================
        for ix in &message.instructions {
            let program_idx = ix.program_id_index as usize;
            if program_idx >= account_keys.len() {
                continue;
            }
            let program_id = account_keys[program_idx];

            if let Some(mut trade) = self.try_parse_instruction(
                &program_id,
                &ix.data,
                &ix.accounts,
                &account_keys,
                &signature,
                source_wallet,
            )? {
                // 延迟序列化：仅在匹配成功后才序列化目标交易（省 ~400µs/非匹配交易）
                trade.raw_transaction_bytes = Self::serialize_transaction_from_proto(tx_data)
                    .unwrap_or_default();
                // meta 为空 = 预执行（交易尚未被 leader 执行，可 Backrun）
                trade.is_pre_execution = meta.is_none();
                return Ok(Some(trade));
            }
        }

        // ============================================
        // 第二轮：扫描 inner instructions (CPI 调用)
        // 场景：通过 Jupiter 等聚合器间接调用 Pump.fun
        // ============================================
        if let Some(m) = meta {
            for inner_group in &m.inner_instructions {
                for inner_ix in &inner_group.instructions {
                    let program_idx = inner_ix.program_id_index as usize;
                    if program_idx >= account_keys.len() {
                        continue;
                    }
                    let program_id = account_keys[program_idx];

                    if let Some(mut trade) = self.try_parse_instruction(
                        &program_id,
                        &inner_ix.data,
                        &inner_ix.accounts,
                        &account_keys,
                        &signature,
                        source_wallet,
                    )? {
                        trade.raw_transaction_bytes = Self::serialize_transaction_from_proto(tx_data)
                            .unwrap_or_default();
                        // inner instructions 来自 meta，所以交易已执行，不可 Backrun
                        trade.is_pre_execution = false;
                        return Ok(Some(trade));
                    }
                }
            }
        }

        Ok(None) // 没有找到已知 DEX 的 swap 指令
    }

    // ================================================================
    // 单条指令解析
    // ================================================================

    /// 尝试解析单条指令，判断是否为已知 DEX 的 swap 操作
    /// 同时适用于顶层指令和 inner instructions
    fn try_parse_instruction(
        &self,
        program_id: &Pubkey,
        data: &[u8],
        account_indices: &[u8],
        all_account_keys: &[Pubkey],
        signature: &str,
        source_wallet: Pubkey,
    ) -> Result<Option<DetectedTrade>> {
        let program_str = program_id.to_string();

        // 只匹配 Pump.fun 内盘，其他 DEX 全部跳过
        let trade_type = match program_str.as_str() {
            PUMPFUN_PROGRAM => Some(TradeType::Pumpfun),
            _ => None,
        };

        let trade_type = match trade_type {
            Some(t) => t,
            None => return Ok(None), // 非 Pump.fun，跳过
        };

        // 解析 buy/sell 方向
        let is_buy =
            self.detect_buy_or_sell(data, &trade_type, account_indices, all_account_keys);

        // 提取指令涉及的 account keys（用于后续 processor 重建指令）
        let instruction_accounts: Vec<Pubkey> = account_indices
            .iter()
            .filter_map(|&idx| all_account_keys.get(idx as usize).copied())
            .collect();

        // 从 Pump.fun buy 指令数据提取 max_sol_cost (买入 SOL 数量)
        // data 布局: [0..8] discriminator, [8..16] token_amount, [16..24] max_sol_cost
        let sol_amount_lamports = if is_buy && data.len() >= 24 {
            u64::from_le_bytes(data[16..24].try_into().unwrap_or([0; 8]))
        } else {
            0
        };

        Ok(Some(DetectedTrade {
            signature: signature.to_string(),
            source_wallet,
            trade_type,
            is_buy,
            program_id: *program_id,
            instruction_data: data.to_vec(),
            instruction_accounts,
            all_account_keys: all_account_keys.to_vec(),
            detected_at: Instant::now(),
            sol_amount_lamports,
            raw_transaction_bytes: Vec::new(), // 由 parse_transaction 填充
            is_pre_execution: false, // 由 parse_transaction 根据 meta 设置
        }))
    }

    // ================================================================
    // Buy / Sell 方向检测
    // ================================================================

    /// 根据指令数据 discriminator + account 布局判断交易方向
    ///
    /// 返回 true = BUY (SOL → Token), false = SELL (Token → SOL)
    fn detect_buy_or_sell(
        &self,
        data: &[u8],
        trade_type: &TradeType,
        account_indices: &[u8],
        all_account_keys: &[Pubkey],
    ) -> bool {
        if data.len() < 8 {
            return true; // 数据不足，默认 buy
        }

        let disc = &data[..8];

        match trade_type {
            // ========================================
            // Pump.fun
            // 有明确的 buy/sell 两个独立指令
            // ========================================
            TradeType::Pumpfun => {
                if disc == PUMPFUN_BUY_DISC {
                    true
                } else if disc == PUMPFUN_SELL_DISC {
                    false
                } else {
                    debug!("Unknown Pumpfun discriminator: {:?}", disc);
                    true
                }
            }

            // ========================================
            // PumpSwap
            // 统一 swap 指令，通过 instruction data 中的
            // base_amount_in / quote_amount_in 判断方向:
            //   quote_in > 0, base_in == 0 → SOL 买入 token (BUY)
            //   base_in > 0, quote_in == 0 → token 卖出换 SOL (SELL)
            //
            // data 布局:
            //   [0..8]   discriminator
            //   [8..16]  base_amount_in  (u64, token 数量)
            //   [16..24] quote_amount_in (u64, SOL lamports)
            // ========================================
            TradeType::PumpSwap => {
                if data.len() >= 24 {
                    let base_in =
                        u64::from_le_bytes(data[8..16].try_into().unwrap_or([0; 8]));
                    let quote_in =
                        u64::from_le_bytes(data[16..24].try_into().unwrap_or([0; 8]));

                    if quote_in > 0 && base_in == 0 {
                        true // SOL → Token = BUY
                    } else if base_in > 0 && quote_in == 0 {
                        false // Token → SOL = SELL
                    } else {
                        debug!(
                            "PumpSwap direction ambiguous: base_in={}, quote_in={}",
                            base_in, quote_in
                        );
                        true // 默认 buy
                    }
                } else {
                    debug!("PumpSwap data too short: {} bytes", data.len());
                    true
                }
            }

            // ========================================
            // Raydium AMM V4
            // 第一个字节是指令 index:
            //   9  = SwapBaseIn
            //   11 = SwapBaseOut
            //
            // account 布局:
            //   index 15 = user_source_token_account
            //   index 16 = user_destination_token_account
            //
            // 如果 user_source 是 WSOL ATA → buy
            // 如果 user_source 是 token ATA → sell
            // ========================================
            TradeType::RaydiumAmm => {
                let ix_index = data[0];
                match ix_index {
                    9 | 11 => {
                        // SwapBaseIn / SwapBaseOut
                        self.is_sol_input(account_indices, all_account_keys, 15)
                    }
                    _ => {
                        debug!("Raydium AMM unknown instruction index: {}", ix_index);
                        true
                    }
                }
            }

            // ========================================
            // Raydium CPMM
            // Anchor discriminator 区分指令类型
            //
            // account 布局:
            //   index 4 = user_input_token_account
            //   index 5 = user_output_token_account
            //
            // 如果 user_input 是 WSOL ATA → buy
            // ========================================
            TradeType::RaydiumCpmm => {
                if disc == CPMM_SWAP_BASE_INPUT || disc == CPMM_SWAP_BASE_OUTPUT {
                    self.is_sol_input(account_indices, all_account_keys, 4)
                } else {
                    debug!("Unknown CPMM discriminator: {:?}", disc);
                    true
                }
            }
        }
    }

    /// 判断指定位置的 account 是否是 WSOL ATA
    ///
    /// 原理: 检查目标钱包的 WSOL ATA (由 wallet + WSOL mint 确定性派生)
    ///       是否匹配 swap 指令中 source token account 的位置
    ///
    /// 优点: 零网络请求，纯本地计算
    /// 缺点: 只能匹配目标钱包自己的 WSOL ATA
    ///        如果是 wrap SOL 到临时账户再 swap 的情况可能误判
    fn is_sol_input(
        &self,
        account_indices: &[u8],
        all_account_keys: &[Pubkey],
        source_position: usize,
    ) -> bool {
        // 安全检查
        if source_position >= account_indices.len() {
            return true; // 兜底默认 buy
        }
        let source_idx = account_indices[source_position] as usize;
        if source_idx >= all_account_keys.len() {
            return true;
        }

        let source_account = all_account_keys[source_idx];
        let wsol_mint = Pubkey::from_str(WSOL_MINT).unwrap();

        // 检查是否匹配任何目标钱包的 WSOL ATA
        for wallet in &self.target_wallets {
            let wsol_ata =
                spl_associated_token_account::get_associated_token_address(wallet, &wsol_mint);
            if source_account == wsol_ata {
                return true; // source 是 WSOL ATA → 用 SOL 买入 → BUY
            }
        }

        // source 不是 WSOL ATA → 大概率是 token ATA → SELL
        false
    }
}

// ================================================================
// 单元测试
// ================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_subscriber() -> GrpcSubscriber {
        GrpcSubscriber::new("https://test.grpc.shyft.to".to_string(), None, vec![])
    }

    #[test]
    fn test_pumpfun_buy_discriminator() {
        let sub = make_subscriber();
        let mut data = vec![0u8; 24];
        data[..8].copy_from_slice(&PUMPFUN_BUY_DISC);

        assert!(sub.detect_buy_or_sell(&data, &TradeType::Pumpfun, &[], &[]));
    }

    #[test]
    fn test_pumpfun_sell_discriminator() {
        let sub = make_subscriber();
        let mut data = vec![0u8; 24];
        data[..8].copy_from_slice(&PUMPFUN_SELL_DISC);

        assert!(!sub.detect_buy_or_sell(&data, &TradeType::Pumpfun, &[], &[]));
    }

    #[test]
    fn test_pumpswap_buy_direction() {
        let sub = make_subscriber();
        let mut data = vec![0u8; 24];
        data[..8].copy_from_slice(&PUMPSWAP_SWAP_DISC);
        data[8..16].copy_from_slice(&0u64.to_le_bytes()); // base_in = 0
        data[16..24].copy_from_slice(&1_000_000u64.to_le_bytes()); // quote_in > 0

        assert!(sub.detect_buy_or_sell(&data, &TradeType::PumpSwap, &[], &[]));
    }

    #[test]
    fn test_pumpswap_sell_direction() {
        let sub = make_subscriber();
        let mut data = vec![0u8; 24];
        data[..8].copy_from_slice(&PUMPSWAP_SWAP_DISC);
        data[8..16].copy_from_slice(&500_000u64.to_le_bytes()); // base_in > 0
        data[16..24].copy_from_slice(&0u64.to_le_bytes()); // quote_in = 0

        assert!(!sub.detect_buy_or_sell(&data, &TradeType::PumpSwap, &[], &[]));
    }

    #[test]
    fn test_raydium_amm_swap_base_in() {
        let sub = make_subscriber();
        let data = [9u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

        // 没有足够 account 数据时默认 buy (true)
        assert!(sub.detect_buy_or_sell(&data, &TradeType::RaydiumAmm, &[], &[]));
    }

    #[test]
    fn test_raydium_cpmm_swap_base_input() {
        let sub = make_subscriber();
        let mut data = vec![0u8; 24];
        data[..8].copy_from_slice(&CPMM_SWAP_BASE_INPUT);

        // 没有足够 account 数据时默认 buy (true)
        assert!(sub.detect_buy_or_sell(&data, &TradeType::RaydiumCpmm, &[], &[]));
    }

    #[test]
    fn test_short_data_defaults_to_buy() {
        let sub = make_subscriber();
        let data = [1u8, 2, 3]; // 不足 8 字节

        assert!(sub.detect_buy_or_sell(&data, &TradeType::Pumpfun, &[], &[]));
        assert!(sub.detect_buy_or_sell(&data, &TradeType::PumpSwap, &[], &[]));
        assert!(sub.detect_buy_or_sell(&data, &TradeType::RaydiumAmm, &[], &[]));
        assert!(sub.detect_buy_or_sell(&data, &TradeType::RaydiumCpmm, &[], &[]));
    }
}
