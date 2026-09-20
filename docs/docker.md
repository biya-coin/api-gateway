# Docker 部署交接

部署者在目标服务器构建和运行，项目提供以下文件：

| 文件 | 作用 |
| --- | --- |
| [Dockerfile](../Dockerfile) | 容器内编译 Rust，运行镜像只保留程序、默认配置和运行依赖 |
| [.dockerignore](../.dockerignore) | 构建只上传源码及必要文件，不包含本地配置、凭证、Git 历史和 target |
| [compose.yaml](../compose.yaml) | 默认配置挂载、端口映射、内置 bridge 网络及宿主机名称解析 |
| [config/default.toml](../config/default.toml) | 配置模板，监听 `0.0.0.0:8888`；允许两个本地开发 Origin、`https://dev.dex.biya.io` 及文档站 `https://dev.dex-api.biya.io` |

## 三种配置的关系

- **程序配置**：`listen_addr = "0.0.0.0:8888"`，表示容器内网关自己监听 `8888`。
- **Docker 映射**：例如 `36016:8888`，表示服务器 `36016` 转进容器 `8888`。宿主机端口由运维确认，不改程序监听值。
- **Origin**：浏览器打开前端／文档页面的地址，不是上述两个网关端口。默认允许 `http://localhost:8080`、`http://127.0.0.1:8080`、`https://dev.dex.biya.io` 和文档站 `https://dev.dex-api.biya.io`（供 `/docs` 页内 Test Request），协议及端口须精确匹配，不带尾部 `/`。原公网 IP 来源已移除；新增 HTTPS Origin 不改变网关监听协议。

HTTP、WebSocket、网关存活检查共用 `8888`，不需要 `8889`。
Compose 默认将映射端口绑定到宿主机 `0.0.0.0`，监听所有 IPv4 网卡。公网访问需服务器防火墙及云安全组放行 TCP `36016`，客户端使用服务器公网 IP 或域名访问，不使用 `0.0.0.0` 作为目标地址。需要限制监听范围时，可通过 `GATEWAY_BIND_IP` 指定网卡地址。

## 1. 当前服务器配置

下面命令在目标服务器的项目目录执行；本地验证不会操作服务器上的真实服务。

已根据服务器 `docker ps` 输出配置：

| 项目 | 当前值 |
| --- | --- |
| 交易池容器 | `bybchain-exchange-apiserver-dev-bridge2` |
| 交易 HTTP 映射 | `0.0.0.0:36014 -> 8888/tcp` |
| 另一端口映射 | `127.0.0.1:18281 -> 8889/tcp`，网关不使用 |
| 交易容器网络 | Docker 内置 `bridge` |
| 网关交易后端 URL | `http://host.docker.internal:36014/exchange` |
| indexer 容器／网络 | `biya-indexer` / `biya-indexer_default` |
| indexer HTTP/WS 映射 | 宿主机 `9090` 和 `36018` 均映射至容器 `8888/tcp`，网关统一使用 `36018` |
| indexer 另一端口映射 | `127.0.0.1:36019 -> 8889/tcp`，网关不使用 |
| 网关 indexer 后端 URL | `http://host.docker.internal:36018/info` 和 `ws://host.docker.internal:36018/ws` |
| 状态服务容器／网络 | `bybchain-api-server-api-server-1` / `bybchain-api-server_default` |
| 状态 HTTP/WS 映射 | `0.0.0.0:36020 -> 8888/tcp` |
| 网关状态后端 URL | `http://host.docker.internal:36020/info` 和 `ws://host.docker.internal:36020/ws` |

**直接使用项目默认配置即可，不需要创建本地配置或填写网络名。** Compose 默认挂载 [config/default.toml](../config/default.toml)，并使用 `network_mode: bridge`。
内置 bridge 不提供容器名自动解析，因此不能直接把交易容器名当成主机名。Compose 通过 `extra_hosts` 将 `host.docker.internal` 映射到 Docker 的 `host-gateway`，从网关容器经宿主机 `36014` 访问交易容器 `8888`。
indexer 和状态 APIServer 同样经宿主机映射端口 `36018`、`36020` 访问，不要求网关加入后端网络。
该方案要求 Docker Engine 20.10 或更高版本，不修改或重启后端容器，也不创建共享网络。

三个后端的默认地址均已配置。WS 的 `assetCtxs`、`clearinghouseState` 改为通过新增的 `upstreams.state_ws` 访问状态服务；其余订阅仍走 indexer。旧挂载配置需补上此键，不能只替换镜像。状态 `/ws` 由后端团队提供，上线后需验证订阅确认、推送和取消订阅；网关健康检查不证明其可用。
WS 现在按消息分流，不协商压缩或子协议。每条前端连接最多建立两条上游连接，部署前请核对连接数和消息缓冲的内存预算。
**容器里的 `localhost` 只指该容器自己**，不是宿主机。Linux 宿主机直接运行程序时，可另用 `http://127.0.0.1:36014/exchange`。

网关镜像使用非 root 的 UID/GID `10001:10001`。挂载配置须对该用户可读，且父目录可遍历；Compose 不会自动创建不存在的配置文件。

## 2. 构建镜像

服务器需要 Docker Engine 和 Compose 插件，不要求宿主机安装 Rust。

```sh
docker build --pull -t api-gateway:local .
```

编译使用 `cargo build --release --locked`，依赖版本来自 [Cargo.lock](../Cargo.lock)。
默认构建镜像为 `rust:1.97.1-bookworm`，与当前已验证的本地 Rust 版本一致；运行镜像为 `debian:bookworm-slim`。
构建需要访问镜像仓库、Cargo 仓库和 Debian 软件源。不在运行镜像中携带 Rust/Cargo，也不复制 macOS 的本地二进制。
使用其他架构服务器时应在目标架构上构建，或由运维使用 Docker Buildx 指定目标平台。

公司镜像源可以替换基础镜像，例如：

```sh
docker build \
  --build-arg RUST_IMAGE=公司镜像仓库/rust:1.97.1-bookworm \
  --build-arg RUNTIME_IMAGE=公司镜像仓库/debian:bookworm-slim \
  -t api-gateway:local .
```

替换镜像应保持兼容的 Rust 版本及 Debian/glibc 基线；生产环境建议运维固定基础镜像 digest。

## 3. 校验并启动

构建完镜像后，在项目目录直接执行：

```sh
docker compose config --quiet
docker compose run --rm --no-deps api-gateway \
  --config /app/config/default.toml --check-config
docker compose up -d --no-build api-gateway
```

程序读取挂载到 `/app/config/default.toml` 的项目默认配置，宿主机映射默认为 `0.0.0.0:36016 -> 8888`。
确需覆盖时，可设置 `GATEWAY_CONFIG`（配置文件路径）、`GATEWAY_HOST_PORT`、`GATEWAY_BIND_IP`、`GATEWAY_IMAGE`；不再需要 `GATEWAY_NETWORK`。
若此前导出过这些环境变量或写入过 `.env`，请先核对 `docker compose config` 输出，确认没有覆盖本次默认值。
只使用 `docker run` 而不用 Compose 时，必须同样添加 `--add-host host.docker.internal:host-gateway`，否则 Linux 容器不一定能解析宿主机名称。
更换配置不需要重建镜像，但配置不会热更新。修改挂载文件后执行 `docker compose restart api-gateway`；修改映射端口或挂载位置后重新执行 `docker compose up -d --no-build api-gateway`。
如修改 `server.shutdown_timeout_ms`，应确保 Compose 的 `stop_grace_period` 留出更长的退出时间。

## 4. 检查和更新

```sh
docker compose ps
docker compose logs --tail=100 api-gateway
curl --fail http://127.0.0.1:36016/healthz
```

`ok` 或 Docker `healthy` 只表示网关可接收请求，不主动探测任何后端。
健康检查固定访问容器内 `8888`；若改用其他容器内端口，须同步修改映射和健康检查。
日志写标准输出，由 Docker 轮转；进程异常退出时 Compose 按 `unless-stopped` 重启。仅被标记 `unhealthy` 不会触发 Docker 自动重启。

代码更新后重新构建镜像，再执行 `docker compose up -d --no-build api-gateway`。其他服务不受影响。

## 验证边界

2026-09-14 宿主机端口方案变更前，已在 macOS Docker Desktop 完成以下镜像与代理实测：

- `linux/arm64` 原生及 `linux/amd64` 模拟运行环境中的镜像构建、非 root 配置校验。
- Compose 只读文件系统、只读配置挂载、独立 Docker 网络、动态宿主机端口映射和容器健康检查。
- 通过独立模拟后端验证状态查询、indexer 查询、交易请求的路径与原始请求体透传。
- CORS 预检和来源拒绝、未知 info 类型拒绝，以及 WS 订阅消息、JSON 心跳和关闭透传。
- 两种架构的网关均在 SIGTERM 后正常退出，退出码为 `0`。

首次构建发现配置父目录不可遍历，已在镜像中显式设置 `/app` 和 `/app/config` 为 `0755`，修复后构建通过，仍以 UID/GID `10001:10001` 运行。
测试仅访问模拟后端，没有向真实交易池提交交易。上述网络测试使用独立用户自定义网络，不等同于本次内置 bridge + host-gateway + 宿主机 36014 路径的实测。
当前默认路径已按用户提供的服务器端口信息配置；本次只做 Compose 静态校验和本地回归，宿主机端口连通性仍需目标服务器验证。
