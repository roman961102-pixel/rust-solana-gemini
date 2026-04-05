# Solana Copy Trader - 代码工作流程解析

## 目录

1. [系统概览](#系统概览)
2. [启动流程 (main.rs)](#启动流程)
3. [gRPC 订阅 (grpc/)](#grpc-订阅)
4. [共识引擎 (consensus/)](#共识引擎)
5. [交易处理器 (processor/)](#交易处理器)
6. [交易构建与发送 (tx/)](#交易构建与发送)
7. [Swap 执行 (swap/)](#swap-执行)
8. [自动卖出 (autosell/)](#自动卖出)
9. [配置管理 (config.rs)](#配置管理)
10. [完整数据流](#完整数据流)

---

## 系统概览

这是一个 Solana 链上的跟单交易机器人 (Copy Trader)，核心逻辑：

1. **监听**目标钱包的链上交易 (通过 gRPC)
2. **解析**交易，识别 DEX 买入行为
3. **共识**：当 N 个目标钱包在时间窗口内买入同一代币时触发
4. **执行**：构建并发送镜像买入交易
5. **管理**：自动止盈/止损/追踪止损

---

## 启动流程

**文件**: `src/main.rs`

### 初始化顺序

```
1. 加载配置 (AppConfig::from_env)
2. 创建 RPC 客户端 (Shyft + Helius)
3. 启动 BlockhashCache (每400ms刷新)
4. 创建 gRPC 订阅器
5. 创建共识引擎 + 启动清理任务
6. 创建 TxSender (多通道发送)
7. 创建 JupiterSwap 客户端
8. 创建 AutoSellManager + 启动监控
9. 进入主循环 (tokio::select!)
```

### 主循环

三个并发监听通道：

| 通道 | 来源 | 处理逻辑 |
|------|------|----------|
| `trade_rx` | gRPC 交易流 | 去重 → 提取代币 → 提交共识 |
| `consensus_rx` | 共识引擎 | 触发买入执行 |
| `sell_signal_rx` | 自动卖出监控 | 执行卖出 |

---

## gRPC 订阅

**文件**: `src/grpc/subscriber.rs`

### 连接

- 使用 Shyft Yellowstone gRPC 端点
- TLS 加密 + `x-token` 认证头
- 过滤条件：`account_include` = 目标钱包列表, `vote=false`, `failed=false`, `commitment=Processed`

### 交易解析 (两阶段扫描)

**阶段1**: 扫描外层指令 (message.instructions)
**阶段2**: 扫描内层指令 (meta.inner_instructions / CPI 调用)

### 账户地址解析

组合三部分地址：
- `message.account_keys` (静态账户)
- `meta.loaded_writable_addresses` (ALT 可写地址)
- `meta.loaded_readonly_addresses` (ALT 只读地址)

> 这对 Address Lookup Tables (ALT) 至关重要，否则很多交易无法正确解析。

### 支持的 DEX

| DEX | Program ID | 买入判断 |
|-----|-----------|---------|
| **Pump.fun** | `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` | 指令判别器 `[102,6,61,18,1,218,235,234]` |
| **PumpSwap** | - | `quote_in > 0 && base_in == 0` (SOL 输入) |
| **Raydium AMM** | - | user_source 是 WSOL ATA |
| **Raydium CPMM** | - | user_source 是 WSOL ATA |

### 输出结构: `DetectedTrade`

```rust
pub struct DetectedTrade {
    pub signature: String,              // 交易签名 (base58)
    pub source_wallet: Pubkey,          // 触发交易的目标钱包
    pub trade_type: TradeType,          // DEX 类型
    pub is_buy: bool,                   // 是否为买入
    pub program_id: Pubkey,             // DEX 程序地址
    pub instruction_data: Vec<u8>,      // 原始指令数据
    pub instruction_accounts: Vec<Pubkey>,
    pub all_account_keys: Vec<Pubkey>,
    pub detected_at: Instant,
}
```

---

## 共识引擎

**文件**: `src/consensus/engine.rs`

### 核心逻辑

```
BuySignal 提交
    ↓
查找/创建 TokenSignals (按 token_mint 索引)
    ↓
已触发过? → 跳过
    ↓
清理过期信号 (> timeout)
    ↓
去重: 同一钱包对同一代币只计一次
    ↓
添加新信号
    ↓
唯一钱包数 ≥ min_wallets? → 触发 ConsensusTrigger
```

### 关键参数

| 参数 | 说明 | 示例 |
|------|------|------|
| `min_wallets` | 触发所需最少钱包数 | 2 |
| `timeout` | 信号过期时间 | 60秒 |

### 清理任务

- 每 10 秒运行
- 移除已触发且信号全部过期 (2× timeout) 的条目
- 移除未触发且信号全部过期的条目
- 防止 DashMap 无限增长

---

## 交易处理器

**文件**: `src/processor/`

### Trait 定义

```rust
pub trait TradeProcessor: Send + Sync {
    fn trade_type(&self) -> TradeType;
    async fn build_mirror_instructions(
        &self,
        trade: &DetectedTrade,
        config: &AppConfig,
    ) -> Result<MirrorInstruction>;
}
```

### 输出: `MirrorInstruction`

```rust
pub struct MirrorInstruction {
    pub swap_instructions: Vec<Instruction>,   // 主 swap 指令
    pub pre_instructions: Vec<Instruction>,    // ATA 创建等前置指令
    pub post_instructions: Vec<Instruction>,   // ATA 关闭等后置指令
    pub token_mint: Pubkey,
    pub sol_amount: u64,
}
```

### Pump.fun 处理器 (主要路径)

**`buy_from_mint()` 流程**:

1. 推导 `bonding_curve` PDA: `find_program_address(&[b"bonding-curve", mint], &pumpfun_id)`
2. 推导 `associated_bonding_curve`: `get_associated_token_address(&bonding_curve, mint)`
3. 获取并解析 `BondingCurveState`:
   - `virtual_token_reserves` / `virtual_sol_reserves` (AMM 虚拟储备)
   - `real_token_reserves` / `real_sol_reserves` (实际储备)
   - `complete` (是否已迁移)
4. 报价计算 (恒定乘积 AMM):
   ```
   token_amount = (virtual_tokens × sol_amount) / (virtual_sol + sol_amount)
   ```
5. 滑点保护: `max_sol_cost = sol_amount × (1 + slippage_bps / 10000)`
6. 构建买入指令 (14 个账户)

---

## 交易构建与发送

### BlockhashCache (`tx/blockhash.rs`)

- 后台任务每 **400ms** 刷新 (匹配 Solana 出块时间)
- 读取时零延迟 (从内存读 RwLock)
- 连续错误追踪 (3+ 次失败后告警)

### TxBuilder (`tx/builder.rs`)

构建顺序：
```
1. ComputeBudget::set_compute_unit_limit
2. ComputeBudget::set_compute_unit_price
3. pre_instructions (ATA 创建等)
4. swap_instructions (核心 swap)
5. post_instructions
6. [Jito 模式] tip 转账指令
7. 签名
```

### TxSender (`tx/sender.rs`) - 多通道并发发送

| 通道 | 说明 | 备注 |
|------|------|------|
| **Shyft RPC** | 主 RPC | skip_preflight=true (Pump.fun) |
| **Helius RPC** | 备用 RPC | 可选 |
| **Jito Bundle** | MEV 保护 | HTTP POST, base64 编码 |

三个通道**并发**发送，任一成功即返回。

---

## Swap 执行

**文件**: `src/swap/jupiter.rs`

### Jupiter API (备用路径)

当 Pump.fun 直接购买失败时，回退到 Jupiter 聚合器：

```
1. GET /quote → 获取报价 (JupiterQuote)
2. POST /swap → 获取未签名交易
3. 本地签名 → VersionedTransaction
4. 通过 RPC 发送
```

### 关键参数

- `dynamicComputeUnitLimit: true`
- `dynamicSlippage: { maxBps: 500 }`
- `priorityLevel: "veryHigh"`
- `wrapAndUnwrapSol: true`

---

## 自动卖出

**文件**: `src/autosell/manager.rs`

### Position 结构

```rust
pub struct Position {
    pub token_mint: Pubkey,
    pub entry_sol_amount: u64,     // 买入花费 SOL
    pub token_amount: u64,          // 持有代币数量
    pub entry_price_sol: f64,       // 买入价格
    pub highest_price: f64,         // 历史最高价 (追踪止损用)
    pub opened_at: Instant,
    pub source_wallet: Pubkey,
    pub trade_signature: String,
}
```

### 监控循环 (每 N 秒)

对每个持仓执行以下检查：

```
1. 最大持仓时间检查
   held_seconds >= max_hold_seconds → 卖出 (MaxLifetime)

2. 获取当前价格 (Jupiter Price API v2)

3. 更新历史最高价

4. 计算盈亏百分比
   pnl% = ((current_price - entry_price) / entry_price) × 100

5. 止盈检查
   pnl% >= take_profit_percent → 卖出 (TakeProfit)

6. 止损检查
   pnl% <= -stop_loss_percent → 卖出 (StopLoss)

7. 追踪止损检查
   从最高点回撤 >= trailing_stop_percent 且 pnl > 0 → 卖出 (TrailingStop)
```

### 卖出执行

```
SellSignal 触发
    ↓
移除持仓记录
    ↓
查询用户 ATA 余额
    ↓
余额为0? → 跳过
    ↓
Jupiter sell_token (token → WSOL)
    ↓
发送交易 → 记录盈亏
```

---

## 配置管理

**文件**: `src/config.rs`

### AppConfig 关键字段

| 类别 | 字段 | 说明 |
|------|------|------|
| **网络** | `rpc_url` | 主 RPC (Shyft) |
| | `secondary_rpc_url` | 备用 RPC (Helius) |
| | `grpc_url` | gRPC 端点 |
| **钱包** | `keypair` | 交易签名密钥对 |
| | `target_wallets` | 跟踪的目标钱包列表 |
| **共识** | `consensus_min_wallets` | 最少触发钱包数 |
| | `consensus_timeout_secs` | 信号超时窗口 |
| **交易** | `buy_sol_amount` | 每次买入金额 (SOL) |
| | `slippage_bps` | 滑点容忍度 (基点) |
| | `compute_units` | 计算单元限制 |
| | `priority_fee_micro_lamport` | 优先费 |
| **Jito** | `jito_enabled` | 是否启用 Jito |
| | `jito_tip_lamports` | Jito 小费 |
| **自动卖出** | `take_profit_percent` | 止盈百分比 |
| | `stop_loss_percent` | 止损百分比 |
| | `trailing_stop_percent` | 追踪止损百分比 |
| | `max_hold_seconds` | 最大持仓时间 |

---

## 完整数据流

```
┌─────────────────────────────────────────────────────┐
│              Shyft gRPC Stream                      │
│         (监听目标钱包链上交易)                         │
└──────────────────────┬──────────────────────────────┘
                       │ VersionedTransaction
                       ▼
              ┌─────────────────┐
              │   交易解析       │
              │ • 两阶段扫描     │
              │ • ALT 地址展开   │
              │ • DEX 识别       │
              │ • 买/卖方向判断  │
              └────────┬────────┘
                       │ DetectedTrade
                       ▼
              ┌─────────────────┐
              │   签名去重       │
              │  (10秒 TTL)     │
              └────────┬────────┘
                       │ (仅买入信号)
                       ▼
              ┌─────────────────┐
              │  提取代币 Mint   │
              │ (Pump.fun 账户  │
              │  或 RPC 回退)   │
              └────────┬────────┘
                       │ BuySignal
                       ▼
              ┌─────────────────┐
              │   共识引擎       │
              │ • 按钱包去重     │
              │ • 过期信号清理   │
              │ • ≥ N 钱包触发   │
              └────────┬────────┘
                       │ ConsensusTrigger
                       ▼
    ┌──────────────────────────────────────┐
    │           买入执行                    │
    │                                      │
    │  ┌─ Pump.fun 直接购买 ──────────┐    │
    │  │ 1. 推导 bonding_curve PDA    │    │
    │  │ 2. 获取 AMM 状态             │    │
    │  │ 3. 计算报价 + 滑点           │    │
    │  │ 4. 构建指令                   │    │
    │  │ 5. 获取 blockhash (缓存)     │    │
    │  │ 6. 多通道发送                 │    │
    │  │    (Shyft + Helius + Jito)   │    │
    │  └──────────┬───────────────────┘    │
    │             │                         │
    │     成功? ──┼── 是 → 记录持仓         │
    │             │                         │
    │     失败? ──┘── Jupiter 备用路径      │
    │                  1. 获取报价           │
    │                  2. 构建交易           │
    │                  3. 签名发送           │
    └──────────────────────────────────────┘
                       │
              ┌─────────────────┐
              │  记录持仓        │
              │ (如启用自动卖出) │
              └────────┬────────┘
                       │
              ┌─────────────────┐
              │  自动卖出监控    │
              │  (每 3 秒检查)   │
              │                  │
              │ • 最大持仓时间   │
              │ • 止盈检查       │
              │ • 止损检查       │
              │ • 追踪止损检查   │
              └────────┬────────┘
                       │ SellSignal
                       ▼
              ┌─────────────────┐
              │   卖出执行       │
              │ 1. 移除持仓      │
              │ 2. 查询 ATA 余额 │
              │ 3. Jupiter 卖出  │
              │ 4. 记录盈亏      │
              └─────────────────┘
```

---

## 关键设计要点

1. **零延迟 Blockhash**: 后台 400ms 刷新缓存，交易构建不需要等待 RPC
2. **多通道发送**: Shyft + Helius + Jito 三通道并发，任一成功即可
3. **共识防重触发**: 同一代币只触发一次，同一钱包只计一次
4. **两阶段交易解析**: 同时扫描外层指令和 CPI 内层指令，覆盖聚合器路由
5. **ALT 地址展开**: 正确处理 Address Lookup Tables，确保所有账户正确解析
6. **Pump.fun 优先 + Jupiter 备用**: 直接链上交互更快，聚合器作为兜底
7. **追踪止损**: 记录历史最高价，从高点回撤超阈值时在盈利状态下自动卖出
