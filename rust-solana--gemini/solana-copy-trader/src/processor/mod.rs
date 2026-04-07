pub mod prefetch;
pub mod pumpfun;
pub mod pumpswap;
pub mod raydium_amm;
pub mod raydium_cpmm;

use anyhow::Result;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use std::fmt;
use std::time::Instant;

use crate::config::AppConfig;

/// 交易类型枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TradeType {
    Pumpfun,
    PumpSwap,
    RaydiumAmm,
    RaydiumCpmm,
}

impl fmt::Display for TradeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TradeType::Pumpfun => write!(f, "Pump.fun"),
            TradeType::PumpSwap => write!(f, "PumpSwap"),
            TradeType::RaydiumAmm => write!(f, "Raydium AMM"),
            TradeType::RaydiumCpmm => write!(f, "Raydium CPMM"),
        }
    }
}

/// gRPC 检测到的目标钱包交易
#[derive(Debug, Clone)]
pub struct DetectedTrade {
    pub signature: String,
    pub source_wallet: Pubkey,
    pub trade_type: TradeType,
    pub is_buy: bool,
    pub program_id: Pubkey,
    pub instruction_data: Vec<u8>,
    pub instruction_accounts: Vec<Pubkey>,
    pub all_account_keys: Vec<Pubkey>,
    pub detected_at: Instant,
    /// 目标钱包的买入 SOL 数量 (lamports)，从指令数据中提取
    pub sol_amount_lamports: u64,
    /// 目标交易的原始序列化字节，用于构建 Jito Bundle
    pub raw_transaction_bytes: Vec<u8>,
    /// 是否为预执行推送（meta 为空 = 交易尚未被 leader 执行 = 可 Backrun）
    pub is_pre_execution: bool,
    /// CPI 检测时已识别的 token mint（直接调用时为 None，由 main.rs 从 instruction_accounts 提取）
    pub token_mint: Option<Pubkey>,
}

/// 处理器构建的镜像交易指令
#[derive(Debug, Clone)]
pub struct MirrorInstruction {
    /// 主 swap 指令
    pub swap_instructions: Vec<Instruction>,
    /// 需要预创建的 ATA
    pub pre_instructions: Vec<Instruction>,
    /// swap 后的清理指令（如关闭空 ATA）
    pub post_instructions: Vec<Instruction>,
    /// 代币 mint 地址（用于持仓跟踪）
    pub token_mint: Pubkey,
    /// 预期买入的 SOL 数量（lamports）
    pub sol_amount: u64,
}

/// 交易处理器 trait，每种 DEX 实现一个
#[async_trait::async_trait]
pub trait TradeProcessor: Send + Sync {
    /// 处理器支持的交易类型
    fn trade_type(&self) -> TradeType;

    /// 将检测到的交易转换为镜像指令
    async fn build_mirror_instructions(
        &self,
        trade: &DetectedTrade,
        config: &AppConfig,
    ) -> Result<MirrorInstruction>;
}

/// 处理器注册表
pub struct ProcessorRegistry {
    processors: Vec<Box<dyn TradeProcessor>>,
}

impl ProcessorRegistry {
    pub fn new() -> Self {
        Self {
            processors: Vec::new(),
        }
    }

    pub fn register(&mut self, processor: Box<dyn TradeProcessor>) {
        tracing::info!("已注册处理器: {}", processor.trade_type());
        self.processors.push(processor);
    }

    /// 根据交易类型找到对应的处理器
    pub fn get_processor(&self, trade_type: TradeType) -> Option<&dyn TradeProcessor> {
        self.processors
            .iter()
            .find(|p| p.trade_type() == trade_type)
            .map(|p| p.as_ref())
    }

    /// 注册所有默认处理器
    pub fn register_all_defaults(
        &mut self,
        rpc_client: std::sync::Arc<solana_client::rpc_client::RpcClient>,
    ) {
        self.register(Box::new(pumpfun::PumpfunProcessor::new(rpc_client.clone())));
        self.register(Box::new(pumpswap::PumpSwapProcessor::new(rpc_client.clone())));
        self.register(Box::new(raydium_amm::RaydiumAmmProcessor::new(rpc_client.clone())));
        self.register(Box::new(raydium_cpmm::RaydiumCpmmProcessor::new(rpc_client)));
    }
}
