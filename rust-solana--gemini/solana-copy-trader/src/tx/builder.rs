use anyhow::Result;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::Message,
    signature::Keypair,
    signer::Signer,
    transaction::Transaction,
    hash::Hash,
};
use tracing::debug;

use crate::config::AppConfig;
use crate::processor::MirrorInstruction;

/// 交易构建器：将 MirrorInstruction 组装成可发送的交易
pub struct TxBuilder;

impl TxBuilder {
    /// 将镜像指令组装为完整交易
    pub fn build_transaction(
        mirror: &MirrorInstruction,
        config: &AppConfig,
        keypair: &Keypair,
        recent_blockhash: Hash,
    ) -> Result<Transaction> {
        let mut all_instructions: Vec<Instruction> = Vec::new();

        // 1. Compute budget 指令（优先费）
        if config.compute_units > 0 {
            all_instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
                config.compute_units,
            ));
        }
        if config.priority_fee_micro_lamport > 0 {
            all_instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                config.priority_fee_micro_lamport,
            ));
        }

        // 2. Pre-instructions（ATA 创建等）
        all_instructions.extend(mirror.pre_instructions.clone());

        // 3. Swap 指令
        all_instructions.extend(mirror.swap_instructions.clone());

        // 4. Post-instructions（ATA 关闭等）
        all_instructions.extend(mirror.post_instructions.clone());

        debug!(
            "Built transaction with {} instructions (CU={}, priority_fee={})",
            all_instructions.len(),
            config.compute_units,
            config.priority_fee_micro_lamport,
        );

        let message = Message::new(&all_instructions, Some(&keypair.pubkey()));
        let mut transaction = Transaction::new_unsigned(message);
        transaction.sign(&[keypair], recent_blockhash);

        Ok(transaction)
    }

    /// 构建 Jito bundle 交易（附带 tip 转账）
    pub fn build_jito_bundle_transaction(
        mirror: &MirrorInstruction,
        config: &AppConfig,
        keypair: &Keypair,
        recent_blockhash: Hash,
        jito_tip_account: &solana_sdk::pubkey::Pubkey,
        tip_lamports: u64,
    ) -> Result<Transaction> {
        let mut all_instructions: Vec<Instruction> = Vec::new();

        // Compute budget
        if config.compute_units > 0 {
            all_instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
                config.compute_units,
            ));
        }
        if config.priority_fee_micro_lamport > 0 {
            all_instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                config.priority_fee_micro_lamport,
            ));
        }

        // Pre-instructions
        all_instructions.extend(mirror.pre_instructions.clone());

        // Swap instructions
        all_instructions.extend(mirror.swap_instructions.clone());

        // Post-instructions
        all_instructions.extend(mirror.post_instructions.clone());

        // Jito tip（放在最后）
        if tip_lamports > 0 {
            all_instructions.push(solana_sdk::system_instruction::transfer(
                &keypair.pubkey(),
                jito_tip_account,
                tip_lamports,
            ));
        }

        let message = Message::new(&all_instructions, Some(&keypair.pubkey()));
        let mut transaction = Transaction::new_unsigned(message);
        transaction.sign(&[keypair], recent_blockhash);

        Ok(transaction)
    }
}
