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

依据联调文档及最新职责确认，`/info` 白名单共 23 个 type，仅分流到状态 APIServer 和 indexer：

| 后端 | `/info type` |
| --- | --- |
| **状态 APIServer** | `meta`、`extraAgents`、`recentTrades`、`clearinghouseState`、`activeAssetData`、`openOrders`、`frontendOpenOrders`、`userFees`、`unifiedBalances`、`accountNonces`、`marketSnapshot` |
| **indexer** | `metaAndAssetCtxs`、`allMids`、`l2Book`、`webData2`、`candleSnapshot`、`historicalOrders`、`orderStatus`、`userFills`、`userFillsByTime`、`userFunding`、`fundingHistory`、`userNonFundingLedgerUpdates` |

`orderStatus` 只请求 indexer 一次，不回查状态服务。状态请求直接转状态 APIServer，不经 indexer。
聚合和订阅的数据生产仍属下游，不在网关重复实现。

### 交易池接口对照

按 2026-09-14 最新确认，交易池只处理 `/exchange`，网关不提供交易池 `/info` 转发：

- `POST /info {"type":"health"}` 不支持，返回 `400`，不调用任何下游。
- `stateInfo`、`block`、`bridgeSnapshot`、`bridgeDepositStatus`、`bridgeWithdrawalStatus`、`accountOverview` 这六种查询仍暂不开放，网关返回 `400`。
- `clearinghouseState`、`unifiedBalances`、`accountNonces` 直接转状态 APIServer，不增加交易池中转。
- `GET /healthz` 继续用于网关自身存活检查，不调用后端，不代表链或交易池就绪。
- S1 文档中的 `approveAgent`、`order`、`cancel`、`cancelByCloid`、`updateLeverage` 在交易池代码中已有签名处理分支。其 README 中“只支持 order”的描述已滞后，不作为网关过滤 action 的依据。
- `/exchange` 的 `code:0 / accepted` 仅表示通过交易池本地校验并入池，不是上链成功。`1001/1002/1003` 等业务码及 HTTP 状态原样返回，不转译、不补 oid、不重试。

以上是代码对照，不代表真实交易闭环已验收；网关不会代替交易池验签、规范化签名或修复其业务语义。

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

[config/default.toml](config/default.toml) 已按当前服务器的交易容器配置：`bybchain-exchange-apiserver-dev-bridge2` 使用默认 `bridge` 网络，宿主机 `36014` 映射至容器 `8888`。
网关通过 `http://host.docker.internal:36014/exchange` 转发，Compose 使用 `host-gateway` 将该名称解析到宿主机；不依赖默认 bridge 不提供的容器名 DNS，也不写死容器 IP。
`127.0.0.1:18281 -> 8889` 不用于网关交易转发，不配置交易池 `/info` 地址。
indexer 容器 `biya-indexer` 使用 `biya-indexer_default` 网络，宿主机 `9090` 和 `36018` 均映射至容器 `8888`。网关统一经宿主机 `36018` 访问其 `/info` 和 `/ws`，无需加入 indexer 网络；`36019 -> 8889` 不用于转发。
状态容器 `bybchain-api-server-api-server-1` 使用 `bybchain-api-server_default` 网络，宿主机 `36020` 映射至容器 `8888`。网关经宿主机 `36020` 访问其 `/info`，无需加入状态服务网络；原有类型归属不变。
Compose 默认直接挂载 [config/default.toml](config/default.toml)，无需先设置环境变量。也可以用 `GATEWAY_CONFIG` 指定自己的部署文件。
地址必须是**完整接口 URL**，不自动追加路径：

```toml
[upstreams]
exchange = "http://host.docker.internal:36014/exchange"
indexer_info = "http://host.docker.internal:36018/info"
indexer_ws = "ws://host.docker.internal:36018/ws"
state_info = "http://host.docker.internal:36020/info"

[access]
allowed_origins = ["http://localhost:8080", "http://127.0.0.1:8080"]
```

目前启用已部署的交易后端、indexer 和状态 APIServer，当前前端开发 Origin 保持上述两个本机地址。后端无鉴权或 IP 白名单不等于放开网关自身的浏览器 Origin 策略。
自定义配置省略 `exchange` 时，`/exchange` 返回 `503`，不会改用其他后端。
禁止把后端指向网关自身或造成循环依赖。
URL 不允许嵌入账号密码、查询参数、片段；WS 可用 WS/WSS。
`host.docker.internal` 是此 Docker 部署的地址；Linux 宿主机直接运行二进制时，可在自己的配置中使用 `http://127.0.0.1:36014/exchange`。不要在容器内用 `localhost` 指代宿主机。

```sh
cargo run -- --check-config
cargo run
# 指定已有本地配置：
cargo run -- --config config/local.toml
```

默认监听 `0.0.0.0:8888`，即网关在容器内的所有 IPv4 接口上接收请求；本地运行时也会监听全部 IPv4 接口，如只供本机使用可在本地配置中改成 `127.0.0.1:8888`。
目前入站为 HTTP/1.1；HTTPS/WSS、HTTP/2 入口需要后续确认的前置设施或单独 TLS 实现。
出站支持 HTTP/HTTPS 和 WS/WSS，使用系统信任根，不绕过 TLS 证书验证。

### 配置与运维端口映射

- 程序启动时读取 `--config` 指定的文件；未指定时读取工作目录下的 [config/default.toml](config/default.toml)。配置修改后需重启才生效。
- 网关容器端口为 `8888`。宿主机映射端口由运维另行设置，例如 `36016:8888`；`36016` 仅为示例，不写入 `listen_addr`。
- 运维可以挂载部署配置并通过 `--config` 指定，实际加载的配置才决定监听及后端地址；容器映射的目标端口必须与监听端口一致。
- `allowed_origins` 保留前端提供的 `http://localhost:8080` 和 `http://127.0.0.1:8080`，它们是浏览器页面来源，与网关监听／映射端口无关。

### Docker 部署

已提供 [Dockerfile](Dockerfile)、[.dockerignore](.dockerignore) 和可选的 [compose.yaml](compose.yaml)。
镜像采用多阶段编译、非 root 运行，HTTP/WS 共用容器端口 `8888`，不需要第二个端口。
Compose 默认挂载项目配置、使用内置 `bridge` 网络并配置宿主机解析；网关宿主机映射为 `0.0.0.0:36016 -> 8888`，监听所有 IPv4 网卡，可通过环境变量覆盖映射或配置路径；前端 Origin 保持不变。公网访问还需服务器防火墙及云安全组放行 TCP `36016`。
具体流程及命令见 [Docker 部署交接](docs/docker.md)。容器构建／启动的实测情况应与本地 Rust 测试分开确认。

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

默认允许 `http://localhost:8080` 和 `http://127.0.0.1:8080` 两个前端开发 Origin；无 Origin 的 SDK／服务端请求不受影响。
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

1. 目标服务器的交易端口转发连通性；配置来自实际容器信息，尚未在目标机器实测或提交真实交易。
2. 目标服务器的状态 APIServer、indexer HTTP/WS 连通性；默认地址已配置，尚未在目标机器实测。
3. 真实前端 SDK/schema、浏览器跨域、WSS/TLS 和压缩协商验收。
4. 生产证书、域名、可信代理、容量与超时配置。
5. `userRateLimit`、`exchangeStatus`、Bootstrap、维护模式、业务幂等、统一业务错误：按约定暂不实现，这些 info 类型目前返回 400。

没有数据库、缓存、重试队列、主动下游健康轮询或业务聚合；Docker 健康检查只访问网关自身。