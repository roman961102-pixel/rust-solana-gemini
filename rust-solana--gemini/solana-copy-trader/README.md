# Solana Copy Trader (Rust)

高性能 Solana 跟单交易系统，从 Python 迁移到 Rust，核心优化延迟和可靠性。

## 架构概览

```
┌──────────────────────────────────────────────────────────────────────┐
│                        MAIN EVENT LOOP                               │
│  接收 DetectedTrade → 查找 Processor → 构建镜像指令 → 发送交易       │
└──────────┬───────────────────────────────────────────────┬───────────┘
           │                                               │
           ▼                                               ▼
┌─────────────────────┐                     ┌─────────────────────────┐
│   gRPC Subscriber   │                     │    Auto-Sell Manager    │
│  (YellowStone gRPC) │                     │  TP/SL/Trailing/Timer  │
│  ~50-100ms 延迟     │                     │  后台价格监控循环        │
└─────────────────────┘                     └─────────────────────────┘
           │                                               │
           ▼                                               ▼
┌─────────────────────┐                     ┌─────────────────────────┐
│  Processor Registry │                     │    Jupiter Fallback     │
│  ├─ Pumpfun         │                     │    (卖出 + 未知 DEX)    │
│  ├─ PumpSwap        │                     └─────────────────────────┘
│  ├─ Raydium AMM     │
│  └─ Raydium CPMM    │
└─────────────────────┘
           │
           ▼
┌─────────────────────┐     ┌──────────────────────┐
│    TX Builder       │────▶│     TX Sender        │
│  ComputeBudget +    │     │  ├─ Standard RPC     │
│  Priority Fee +     │     │  └─ Jito Bundle      │
│  Jito Tip           │     └──────────────────────┘
└─────────────────────┘
           ▲
           │
┌─────────────────────┐
│  Blockhash Cache    │
│  后台 400ms 刷新    │
│  读取零延迟         │
└─────────────────────┘
```

## 相比 Python 版本的改进

| 环节 | Python 版本 | Rust 版本 | 提升 |
|------|------------|-----------|------|
| 交易监听 | Helius WebSocket (~300-500ms) | YellowStone gRPC (~50-100ms) | 3-6x |
| Swap 执行 | Jupiter HTTP API (~200-500ms) | 直接构造 DEX 指令 (~0ms 网络) | 跳过网络往返 |
| Blockhash | 每次交易时 RPC 查询 (~100ms) | 后台预缓存 (0ms 读取) | 消除延迟 |
| 交易发送 | Python aiohttp + Jito | Rust reqwest + Jito bundle | 更低 overhead |
| 运行时 | Python asyncio (GIL) | Tokio (真并行) | CPU 密集任务无瓶颈 |

## 7 步 Pipeline（检测到执行）

```
1. gRPC 接收目标钱包交易      ← YellowStone gRPC (取代 Helius WebSocket)
2. 解析交易类型 + 指令数据     ← 本地解析，零网络
3. 路由到对应 DEX Processor    ← Pumpfun/PumpSwap/Raydium AMM/CPMM
4. 构建镜像指令                ← 本地 bonding curve 计算，ATA 管理
5. 获取 blockhash              ← 预缓存，0ms
6. 组装 + 签名交易             ← ComputeBudget + Priority Fee + Jito Tip
7. 发送交易                    ← Jito Bundle 或 Standard RPC
```

## 项目结构

```
src/
├── main.rs                    # 入口：初始化 + 主事件循环
├── config.rs                  # .env 配置加载
├── grpc/
│   ├── mod.rs
│   └── subscriber.rs          # YellowStone gRPC 订阅 + 交易解析
├── processor/
│   ├── mod.rs                 # TradeProcessor trait + Registry
│   ├── pumpfun.rs             # Pump.fun bonding curve 买卖镜像
│   ├── pumpswap.rs            # PumpSwap 池子镜像
│   ├── raydium_amm.rs         # Raydium AMM V4 镜像
│   └── raydium_cpmm.rs        # Raydium CPMM 镜像
├── swap/
│   ├── mod.rs
│   └── jupiter.rs             # Jupiter API fallback（报价 + swap）
├── tx/
│   ├── mod.rs
│   ├── blockhash.rs           # Blockhash 预缓存（400ms 后台刷新）
│   ├── builder.rs             # 交易组装（CU + priority fee + Jito tip）
│   └── sender.rs              # 发送（Standard RPC / Jito Bundle）
├── autosell/
│   ├── mod.rs
│   └── manager.rs             # 持仓管理 + 自动卖出（TP/SL/追踪/超时）
└── utils/
    ├── mod.rs
    ├── ata.rs                  # ATA 工具函数
    └── parse.rs                # 余额查询 + PnL 计算
```

## 快速开始

### 1. 安装 Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### 2. 配置

```bash
cp .env.example .env
# 编辑 .env，填入你的私钥、RPC、gRPC endpoint 等
```

### 3. 编译运行

```bash
# Debug 模式（开发调试）
cargo run

# Release 模式（生产环境，性能优化）
cargo run --release
```

## 配置说明

### gRPC Endpoint 选择

| 提供商 | URL | 特点 |
|--------|-----|------|
| Triton (付费) | `https://grpc.triton.one` | 最稳定，需要 x-token |
| Helius (付费) | `https://atlas-mainnet.helius-rpc.com` | 你已有 Helius key |
| 自建 Validator | `http://localhost:10000` | 最低延迟，成本最高 |

### Jito Bundle

启用 Jito bundle 可以实现 0-block 跟单（和目标钱包同一个 block 执行），但需要付 tip。建议 tip 设置为 10000-50000 lamports (0.00001-0.00005 SOL)。

### 自动卖出策略

- **止盈** (`TAKE_PROFIT_PERCENT`): 达到目标利润百分比自动卖出
- **止损** (`STOP_LOSS_PERCENT`): 亏损超过阈值自动止损
- **追踪止损** (`TRAILING_STOP_PERCENT`): 从最高点回撤超过阈值卖出（锁定利润）
- **超时卖出** (`MAX_HOLD_SECONDS`): 持仓超过时间限制强制卖出

## 需要你完成的 TODO

项目骨架已搭建完成，以下部分需要根据你的实际环境补充：

### 高优先级

1. **PumpSwap discriminator 验证**
   - `src/processor/pumpswap.rs` 中的 BUY/SELL discriminator 需要验证
   - 从 PumpSwap IDL 或链上交易中提取正确值

2. **价格获取实现**
   - `src/autosell/manager.rs` → `fetch_token_price()`
   - 推荐方案：直接查询 bonding curve / pool reserves 计算价格

3. **gRPC SubscribeRequest 字段兼容**
   - `src/grpc/subscriber.rs` 中 SubscribeRequest 的字段名可能随 yellowstone-grpc 版本不同
   - 按实际版本调整（v2 vs v5 vs v6 字段差异较大）

4. **inner instructions 解析**
   - 当前只解析顶层指令，部分交易通过 CPI 调用 DEX
   - 需要遍历 `meta.inner_instructions` 来捕获

### 中优先级

5. **CPMM swap discriminator 验证**
   - `src/processor/raydium_cpmm.rs` 中的 `SWAP_BASE_INPUT_DISC`

6. **Jupiter 卖出的 VersionedTransaction 发送**
   - `main.rs` 中卖出信号处理部分需要用正确的 versioned tx 发送方法

7. **min_amount_out 计算**
   - PumpSwap 和 Raydium 处理器中 `min_out` 目前硬编码为 0
   - 需要从链上 pool state 计算 + slippage

### 低优先级

8. **Telegram 通知集成**
9. **持仓数据持久化**（当前仅内存）
10. **多钱包支持**

## 延迟优化路线图

```
当前 Python 版本总延迟: ~700-1200ms
  WebSocket 接收:    300-500ms
  Jupiter API:       200-500ms
  Blockhash 查询:    50-100ms
  签名+发送:         50-100ms

目标 Rust 版本总延迟: ~100-300ms
  gRPC 接收:         50-100ms   ← YellowStone gRPC
  直接 DEX 指令:     0ms        ← 本地构造，无网络
  Blockhash:         0ms        ← 预缓存
  签名+发送:         50-200ms   ← Jito bundle
```

## License

MIT
