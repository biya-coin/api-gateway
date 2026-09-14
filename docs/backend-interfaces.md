# 三个后端服务的接口与订阅归属

本文档说明前端通过 API Gateway 访问的三个后端服务分别负责什么。

> **重要边界**：本文档描述的是网关当前的路由归属和下游已确认的接口形态。
> 网关只负责转发，不实现查询、行情聚合、订阅管理、签名校验或交易状态判断。
> “归属网关路由”不等于下游的每个业务分支都已经完成。

## 1. 服务总览

| 后端服务 | 网关入口 | 默认后端地址 | 主要职责 |
| --- | --- | --- | --- |
| 状态 APIServer | `POST /info` | `http://host.docker.internal:36020/info` | 读取当前账户、市场和链上状态 |
| indexer | `POST /info`、`GET /ws` | `http://host.docker.internal:36018/info`、`ws://host.docker.internal:36018/ws` | 历史查询、订单簿查询和实时行情／账户订阅 |
| 交易池 APIServer | `POST /exchange` | `http://host.docker.internal:36014/exchange` | 接收已经签名的交易动作并进行本地校验、入池 |

三个后端地址来自 [config/default.toml](../config/default.toml)。网关容器通过宿主机映射端口访问后端，不加入后端 Docker 网络。

## 2. 状态 APIServer

### 2.1 网关当前转发的 `/info` 类型

以下请求会被网关归属到状态 APIServer：

| `type` | 主要参数 | 用途 |
| --- | --- | --- |
| `meta` | 可选 `dex` | 市场元数据、交易品种列表和基础配置 |
| `extraAgents` | `user` | 查询用户当前有效且已命名的 API wallet／agent |
| `recentTrades` | 通常包含市场标识 | 查询最近成交 |
| `clearinghouseState` | `user`，可选 `dex` | 查询用户持仓、保证金、账户权益和未实现盈亏 |
| `activeAssetData` | `user`、`coin` | 查询用户在指定市场的杠杆、可交易数量和标记价格等状态 |
| `openOrders` | `user`，可选 `dex` | 查询用户当前订单 |
| `frontendOpenOrders` | `user`，可选 `dex` | 查询前端展示格式的用户当前订单 |
| `userFees` | 通常包含 `user` | 查询用户手续费信息 |
| `unifiedBalances` | 通常包含 `user` | 查询统一账户余额 |
| `accountNonces` | 通常包含 `user` | 查询账户 nonce |
| `marketSnapshot` | 通常包含市场标识 | 查询市场状态快照 |

网关当前只按 `type` 选择后端，其余请求字段原样转发，不在网关重新解析或组装业务响应。

`extraAgents` 由状态 APIServer 从已同步到副本头部的 `SessionRegistry` 读取。它只返回当前仍有效且已命名的 agent；默认钥匙、已撤销、未生效或已过期的 agent 不返回。请求格式为：

```json
{
  "type": "extraAgents",
  "user": "0x1111111111111111111111111111111111111111"
}
```

响应是数组。每个元素包含 `address`、`name` 和可为 `null` 的 `validUntil`（毫秒时间戳）：

```json
[
  {
    "address": "0x2222222222222222222222222222222222222222",
    "name": "trading-agent",
    "validUntil": null
  }
]
```

状态副本未就绪时由状态 APIServer 返回 `503`；用户地址缺失或格式错误时返回 `400`。网关只转发这些状态码和响应体，不在本地查询 agent，也不缓存结果。

### 2.2 状态服务的边界

- 状态服务只提供 HTTP 查询，不提供网关侧的 WebSocket 订阅入口。
- `GET /healthz` 是网关自身存活检查，不会转发到状态服务。
- 以下类型不属于当前网关开放的状态查询白名单：`stateInfo`、`block`、`bridgeSnapshot`、`bridgeDepositStatus`、`bridgeWithdrawalStatus`、`accountOverview`、`userRateLimit`、`exchangeStatus`。
- 状态服务源码还包含其他内部或未由网关开放的查询类型。它们不能直接通过网关访问，除非先更新网关路由白名单。
- 当前服务器联调时，`clearinghouseState` 已验证可返回 `200`；`meta` 曾由实际状态服务返回 `501`（`unsupported info type`）。这属于下游实现状态，网关会原样透传，不代表网关自动补齐该接口。

## 3. Indexer

Indexer 同时负责 HTTP 查询和 WebSocket 实时订阅。

### 3.1 HTTP `/info` 查询

以下请求会被网关归属到 indexer：

| `type` | 主要参数 | 用途 |
| --- | --- | --- |
| `metaAndAssetCtxs` | 可选 `dex` | 返回市场元数据及市场上下文 |
| `allMids` | 通常无额外参数 | 查询全部市场的中间价 |
| `l2Book` | `coin`，可选 `nSigFigs`、`nLevels`、`mantissa` | 查询 L2 聚合订单簿 |
| `webData2` | `user` | 返回前端页面加载所需的账户和市场聚合数据 |
| `candleSnapshot` | `coin`、`interval`、时间范围 | 查询 K 线快照 |
| `historicalOrders` | `user` | 查询用户近期历史订单 |
| `orderStatus` | `user`、`oid` | 查询用户自己的订单状态；`oid` 可以是数字订单 ID 或 cloid |
| `userFills` | `user` | 查询用户近期成交 |
| `userFillsByTime` | `user`、`startTime`，可选 `endTime` | 按时间范围查询用户成交 |
| `userFunding` | `user`、`startTime`，可选 `endTime` | 按时间范围查询用户资金费 |
| `fundingHistory` | `coin`、`startTime`，可选 `endTime` | 按时间范围查询市场资金费历史 |
| `userNonFundingLedgerUpdates` | `user`、`startTime`，可选 `endTime` | 按时间范围查询用户非资金类账本变更 |

`orderStatus` 只访问 indexer，不回退到状态 APIServer。Indexer 自己负责订单状态索引、历史文件读取、订单簿快照和数据组装。

### 3.2 WebSocket `/ws` 订阅

前端通过 `GET /ws` 发起 HTTP/1.1 Upgrade。网关成功连接 indexer 后，只做双向字节隧道，不解析或改写 WebSocket 帧。

客户端发送的基本格式是：

```json
{
  "method": "subscribe",
  "subscription": {
    "type": "订阅类型"
  }
}
```

取消订阅时将 `method` 改为 `unsubscribe`；连接级 JSON 心跳使用 `{"method":"ping"}`。

#### 市场和订单簿订阅

| 订阅 `type` | 必要参数 | 推送内容 |
| --- | --- | --- |
| `trades` | `coin` | 指定市场的成交 |
| `l2Book` | `coin`；可选 `nSigFigs`、`nLevels`、`mantissa` | L2 订单簿快照及变化 |
| `l4Book` | `coin` | 指定市场的 L4 订单簿 |
| `bbo` | `coin` | 最优买卖报价 |
| `bookDiffs` | `coin` | 订单簿差异 |
| `activeAssetCtx` | `coin` | 永续市场上下文，例如标记价格、资金费率相关上下文 |
| `activeSpotAssetCtx` | `coin` | Spot 市场上下文 |
| `allMids` | 无 | 全部市场中间价 |
| `assetCtxs` | 可选 `dex` | 全部永续市场上下文 |
| `candle` | `coin`、`interval` | K 线实时更新 |

#### 用户和账户订阅

| 订阅 `type` | 必要参数 | 推送内容 |
| --- | --- | --- |
| `orderUpdates` | `user` | 用户订单状态更新 |
| `userFills` | `user`；可选 `aggregateByTime` | 用户成交推送 |
| `userEvents` | `user` | 用户事件推送 |
| `userFundings` | `user` | 用户资金费推送 |
| `userNonFundingLedgerUpdates` | `user` | 用户非资金类账本更新 |
| `userHistoricalOrders` | `user` | 用户历史订单状态推送 |
| `clearinghouseState` | `user`；可选 `dex` | 用户持仓和账户状态变化 |
| `openOrders` | `user`；可选 `dex` | 用户当前订单变化 |
| `activeAssetData` | `user`、`coin` | 用户在指定市场的交易状态变化 |

Indexer 当前源码共定义上述 **19 种** WebSocket 订阅类型。网关不新增订阅类型，也不在网关缓存或合并订阅数据。

### 3.3 Indexer 订阅行为边界

- `l2Book` 和 `l4Book` 在订阅成功时可能先返回一次快照，然后继续推送变化。
- `userFills`、`userEvents`、资金费和账本类订阅需要合法的 42 字符 `0x` 用户地址。
- `l2Book` 的 `nSigFigs`、`nLevels` 和 `mantissa` 有 indexer 自身的校验及上限。
- `candle` 的 `interval` 必须是 indexer 支持的时间周期。
- WebSocket 关闭、重连、订阅去重和订阅数量限制由 indexer 与客户端负责，网关不自动重连。

## 4. 交易池 APIServer

### 4.1 网关入口

交易池只通过 `POST /exchange` 接收请求。网关不会按 `action.type` 设置白名单，也不会解析、重排、重新签名或重试请求体。

通用请求外层包含：

- `action: object`
- `nonce: u64`
- `signature: { r, s, v }`
- 可选 `vaultAddress`
- 可选 `expiresAfter`

交易池负责签名标准化、参数校验、交易哈希计算、签名恢复、验签和进入本地交易池。

### 4.2 当前已确认的交易动作

以下动作在交易池当前签名处理路径中有明确分支。它们不是网关白名单，最终是否接受仍由交易池参数校验、签名和业务状态决定。

#### Hyperliquid L1-agent 签名动作

| `action.type` | 用途 | 主要字段 |
| --- | --- | --- |
| `order` | 创建限价订单 | `orders`、`grouping`；订单含 `a`、`b`、`p`、`s`、`r`、`t.limit.tif`，可选 `c` |
| `cancel` | 按订单 ID 撤单 | `cancels[]`，每项含 `a`、`o` |
| `cancelByCloid` | 按 client order id 撤单 | `cancels[]`，每项含 `asset`、`cloid` |
| `cancelAll` | 撤销账户全部订单 | 仅 `type` |
| `updateLeverage` | 更新市场杠杆及逐仓／全仓模式 | `asset`、`isCross`、`leverage` |
| `batchModify` | 批量撤单并提交替换订单 | `modifies[]`，含 `oid`、`order` |

#### User-signed EIP-712 动作

| `action.type` | 用途 | 主要字段 |
| --- | --- | --- |
| `usdSend` | 向其他地址转移 USDC | `hyperliquidChain`、`signatureChainId`、`destination`、`amount`、`time` |
| `withdraw3` | Hyperliquid 风格的 USDC 提现 | `hyperliquidChain`、`signatureChainId`、`destination`、`amount`、`time` |
| `approveAgent` | 主账户授权 API wallet／agent | `hyperliquidChain`、`signatureChainId`、`agentAddress`、可选 `agentName`、`nonce` |

交易池源码还包含其他动作枚举或链端扩展动作。它们是否能在当前部署配置中成功执行，应以交易池当前代码和联调结果为准，不由网关文档扩大承诺范围。

### 4.3 交易结果含义

- `code: 0` 或 `accepted`：交易通过交易池本地校验并进入本地交易池。
- `1001`、`1002`、`1003` 等业务错误：由交易池产生，网关原样透传。
- HTTP `200` 或 `accepted` 不代表交易已经执行、区块已经最终确认。
- 网关不会查询交易最终状态，也不会因为网络错误自动重发交易。

## 5. 不属于三个后端转发职责的入口

| 入口或类型 | 处理方式 |
| --- | --- |
| `GET /healthz` | 网关本地返回 `200 ok`，不调用后端 |
| `POST /info` 的未知或未开放 `type` | 网关返回 `400`，不调用后端 |
| `POST /info {"type":"health"}` | 网关返回 `400`；不能用它检查三个后端 |
| HTTPS/WSS 入口 | 当前网关不终止 TLS；由外部反向代理或其他前置设施负责 |

## 6. 请求流向速查

```text
前端 POST /info
  ├─ 状态类型       -> 状态 APIServer /info
  └─ indexer 类型   -> indexer /info

前端 GET /ws Upgrade
  └─ 整条字节隧道   -> indexer /ws

前端 POST /exchange
  └─ 原始请求体     -> 交易池 APIServer /exchange
```

网关的关键原则是：**负责分流，不负责业务完成**。状态计算由状态 APIServer 完成，历史和订阅由 indexer 完成，验签和交易入池由交易池 APIServer 完成。
