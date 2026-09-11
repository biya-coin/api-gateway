# api-gateway

独立 Rust 网关，统一接收 DEX 前端请求，分流至状态 APIServer、indexer 和交易池 APIServer。
不依赖链端 workspace，不计算余额、不入交易池、不查数据库、不修改签名内容。

## 路由

| 入口 | 行为 |
| --- | --- |
| `POST /info` | 检查顶层唯一字符串 `type`，按下表转发 |
| `POST /exchange` | 原始请求体直接转交易池服务，不解析 action |
| `GET /ws`（Upgrade） | 整条 WebSocket 连接代理到 indexer |
| `GET /healthz` | `200` / `ok`，只表示网关存活，不代表后端或链就绪 |

依据 2026-09-11 联调文档及本次确认的职责边界，白名单共 22 个 type：

| 后端 | `/info type` |
| --- | --- |
| **状态 APIServer** | `meta`、`recentTrades`、`clearinghouseState`、`activeAssetData`、`openOrders`、`frontendOpenOrders`、`userFees`、`unifiedBalances`、`accountNonces`、`marketSnapshot` |
| **indexer** | `metaAndAssetCtxs`、`allMids`、`l2Book`、`webData2`、`candleSnapshot`、`historicalOrders`、`orderStatus`、`userFills`、`userFillsByTime`、`userFunding`、`fundingHistory`、`userNonFundingLedgerUpdates` |

`orderStatus` 只请求 indexer 一次，不回查状态服务。状态请求直接转状态 APIServer，不经 indexer。
聚合和订阅的数据生产仍属下游，不在网关重复实现。

## HTTP 语义

- `/info` 仅校验 JSON 对象和唯一字符串 `type`，其余参数交下游。发送原始请求体和查询字符串，不删改未知字段。
- 不自动重试、不跟随重定向、不缓存、不自动解压、不继承系统 HTTP 代理环境变量。
- 在大小限制内完整读取上游响应后返回，保留状态码、原始响应体和端到端头，包括多 `Set-Cookie`、`Retry-After`。
- 剔除逐跳头及 `Connection` 指定的头，重建 `Host` 和消息长度；HTTP trailers 暂不代理。
- 删除不可信的 `Forwarded`、`X-Real-IP`、`X-Forwarded-*`，按实际 TCP 对端生成 `X-Forwarded-For`。可信前置代理链规则后续再定。
- 下游 CORS 头不透传，由网关统一控制；保留 `Authorization` 等端到端请求头，不做业务鉴权。
- 并发许可跟随响应字节，慢读客户端不能无限积累响应；下行停滞设有写入超时。

## WebSocket 语义

- 验证 HTTP/1.1 GET 握手，先连接 indexer，成功后才返回 `101`。
- 保留握手 Key/Accept、协商子协议、压缩扩展和端到端头。
- Upgrade 后透明双向复制字节，不解码或改写订阅、JSON 心跳、RFC Ping/Pong、Close、二进制、分片和压缩负载。
- 每方向固定大小缓冲，不存储事件、不合并连接、不静默重连。
- 任一端断开就关闭另一端；正常 Close 帧透传，异常或服务停止直接关闭传输，不伪造业务结果。
- 空闲超时默认关闭，可按双向字节活动启用；安静的订阅不等于故障。
- 网关不解析帧，因此没有单帧／解压后消息大小校验，该限制交 indexer 和客户端执行。
- 不新增 indexer 未提供的 WS `post` 能力。

## 配置与启动

[config/default.toml](config/default.toml) 中后端默认留空，可以复制为本地配置，用 `--config` 指定。
地址必须是**完整接口 URL**，不自动追加路径：

```toml
[upstreams]
state_info = "http://127.0.0.1:7100/info"
indexer_info = "http://127.0.0.1:9090/info"
exchange = "http://127.0.0.1:18080/exchange"
indexer_ws = "ws://127.0.0.1:9090/ws"

[access]
allowed_origins = ["https://dex.example.com"]
```

以上仅是格式示例，非确认的部署地址。禁止把后端指向网关自身或造成循环依赖。
URL 不允许嵌入账号密码、查询参数、片段；WS 可用 WS/WSS。

```sh
cargo run -- --check-config
cargo run
# 指定已有本地配置：
cargo run -- --config config/local.toml
```

默认监听 `127.0.0.1:8080`。目前入站为 HTTP/1.1；HTTPS/WSS、HTTP/2 入口需要后续确认的前置设施或单独 TLS 实现。
出站支持 HTTP/HTTPS 和 WS/WSS，使用系统信任根，不绕过 TLS 证书验证。

### 开发初始限制

| 参数 | 默认值 |
| --- | --- |
| `http.connect_timeout_ms` | 3000 |
| `http.request_timeout_ms` | 10000，至读取完整后端响应 |
| `http.body_timeout_ms` | 5000，请求体／请求头读取 |
| `http.write_timeout_ms` | 10000，下行写入连续停滞期限 |
| `http.max_request_body_bytes` | 4 MiB |
| `http.max_response_body_bytes` | 16 MiB |
| `http.max_in_flight` | 128，包括仍持有的响应字节 |
| `http.max_connections` | 1024，总 TCP 连接，含已升级 WS |
| `websocket.handshake_timeout_ms` | 5000 |
| `websocket.max_connections` | 1024 |
| `websocket.tunnel_buffer_bytes` | 每方向 8192 字节 |
| `websocket.idle_timeout_ms` | 0，禁用空闲超时 |
| `server.shutdown_timeout_ms` | 5000，到期取消并等待 HTTP 任务退出，回收所有 WS |

上述不是生产容量承诺。HTTP 完整缓冲响应，部署前需按内存与并发预算调整。

`allowed_origins = []` 拒绝携带 Origin 的浏览器请求；无 Origin 的 SDK／服务端请求不受影响。
`["*"]` 显式允许任意来源，或填写准确域名（无尾部 `/`）。支持 OPTIONS，不启用 Cookie 跨域凭证模式。
Origin 检查不是身份鉴权；Token 验证、每 IP 限流与可信代理链暂不实现。
日志按配置过滤，不读 `RUST_LOG`，不记录请求体、签名或凭证。

## 错误

下游业务错误原样透传；网关自身错误为 `{"error":"machine_code"}`。

| 条件 | HTTP |
| --- | --- |
| 非法／未知 info 类型、非法 WS 握手 | 400 |
| Origin 不允许 | 403 |
| 请求体超时 | 408 |
| 请求体超限 | 413 |
| 后端连接／读取失败、响应超限、非法握手响应 | 502 |
| 后端未配置、请求／WS 容量饱和、服务停止中 | 503 |
| 后端请求／WS 握手超时 | 504 |

TCP 总连接超限直接关闭新连接。写入停滞或 WS 空闲到期关闭传输，不向已开始的响应追加错误体。
交易返回 502/504 不代表交易一定未入池，不自动重发、不判断业务最终状态。

## 代码与验证

- [src/routing.rs](src/routing.rs)：接口归属。
- [src/config.rs](src/config.rs)：配置校验。
- [src/http_proxy.rs](src/http_proxy.rs)：HTTP 转发和缓冲边界。
- [src/websocket.rs](src/websocket.rs)：握手和隧道。
- [src/headers.rs](src/headers.rs)：头过滤。
- [src/server.rs](src/server.rs)、[src/transport.rs](src/transport.rs)：访问、连接与停机管理。
- [tests/http_proxy.rs](tests/http_proxy.rs)、[tests/websocket.rs](tests/websocket.rs)、[tests/lifecycle.rs](tests/lifecycle.rs)：模拟后端与生命周期测试。

```sh
cargo fmt --all -- --check
cargo test --offline --locked -j 2
cargo clippy --offline --locked --all-targets -j 2 -- -D warnings
```

测试只使用临时回环端口，结束后关闭，无需真实后端或外网；未缓存的依赖需开发者先准备。
二进制项目应纳入 [Cargo.lock](Cargo.lock)。

## 后续待接入

1. 真实后端地址与部署版本、交易池实际契约及结果联调。
2. 状态 APIServer 接口补齐、indexer 调整后的数据流；路由存在不等于后端已实现。
3. 真实前端 SDK/schema、浏览器跨域、WSS/TLS 和压缩协商验收。
4. 生产证书、域名、可信代理、容量与超时配置。
5. `userRateLimit`、`exchangeStatus`、Bootstrap、维护模式、业务幂等、统一业务错误：按约定暂不实现，这些 info 类型目前返回 400。

没有数据库、缓存、重试队列、主动健康轮询、业务聚合或部署脚本。