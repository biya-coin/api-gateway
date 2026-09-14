# Docker 部署交接

部署者在目标服务器构建和运行，项目提供以下文件：

| 文件 | 作用 |
| --- | --- |
| [Dockerfile](../Dockerfile) | 容器内编译 Rust，运行镜像只保留程序、默认配置和运行依赖 |
| [.dockerignore](../.dockerignore) | 构建只上传源码及必要文件，不包含本地配置、凭证、Git 历史和 target |
| [compose.yaml](../compose.yaml) | 可选运行模板，设置端口映射、配置挂载和 Docker 网络 |
| [config/default.toml](../config/default.toml) | 配置模板，监听 `0.0.0.0:8888`；前端 Origin 保持两个本地开发地址 |

## 三种配置的关系

- **程序配置**：`listen_addr = "0.0.0.0:8888"`，表示容器内网关自己监听 `8888`。
- **Docker 映射**：例如 `36016:8888`，表示服务器 `36016` 转进容器 `8888`。宿主机端口由运维确认，不改程序监听值。
- **Origin**：浏览器打开前端页面的地址，不是上述两个网关端口。现有 `http://localhost:8080`、`http://127.0.0.1:8080` 不因容器化而改变。

HTTP、WebSocket、网关存活检查共用 `8888`，不需要 `8889`。
Compose 默认只将映射端口绑定到宿主机 `127.0.0.1`；确需内网其他机器访问时，运维设置 `GATEWAY_BIND_IP` 为对应网卡地址。

## 1. 准备配置和网络

下面命令在目标服务器的项目目录执行；本地验证不会操作服务器上的真实服务。

从 [config/default.toml](../config/default.toml) 复制一份部署配置（不要覆盖已有的本地配置）：

```sh
cp -n config/default.toml config/local.toml
```

运维修改这份本地配置里的 `upstreams`，填写从**网关容器内部**可达的实际地址。

**独立容器的 `localhost` 只指当前容器，不能访问隔壁容器。** 本项目没有改写既有 localhost 默认值，也没有假定其他容器的名称。
推荐让网关与三个后端加入同一个已有的用户自定义 Docker 网络，用服务名／网络别名及后端**容器内端口**访问。
例如某个状态服务网络别名确实为 `state-api`、容器内端口确实为 `3300`，则填 `http://state-api:3300/info`；这些名称和端口由运维核实，不是项目强制值。

网关镜像使用非 root 的 UID/GID `10001:10001`。挂载配置须对该用户可读，且父目录可遍历；Compose 不会自动创建不存在的配置文件。
Compose 仅加入运维提供的已有网络，不启动、重启或修改其他三个服务。

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

在同一个 shell 设置参数，`已存在的共享网络名` 必须换成实际名称：

```sh
export GATEWAY_CONFIG="$PWD/config/local.toml"
export GATEWAY_NETWORK="已存在的共享网络名"
export GATEWAY_HOST_PORT=36016

docker compose config --quiet
docker compose run --rm --no-deps api-gateway \
  --config /app/config/default.toml --check-config
docker compose up -d --no-build api-gateway
```

程序启动时读取挂载到 `/app/config/default.toml` 的部署配置；若只运行镜像而不挂载，则使用镜像自带的默认配置。
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

2026-09-14 已在 macOS Docker Desktop 完成以下实测：

- `linux/arm64` 原生及 `linux/amd64` 模拟运行环境中的镜像构建、非 root 配置校验。
- Compose 只读文件系统、只读配置挂载、独立 Docker 网络、动态宿主机端口映射和容器健康检查。
- 通过独立模拟后端验证状态查询、indexer 查询、交易请求的路径与原始请求体透传。
- CORS 预检和来源拒绝、未知 info 类型拒绝，以及 WS 订阅消息、JSON 心跳和关闭透传。
- 两种架构的网关均在 SIGTERM 后正常退出，退出码为 `0`。

首次构建发现配置父目录不可遍历，已在镜像中显式设置 `/app` 和 `/app/config` 为 `0755`，修复后构建通过，仍以 UID/GID `10001:10001` 运行。
测试仅访问模拟后端，没有向真实交易池提交交易。目标服务器的实际后端服务名、端口及网络仍需在部署时按实际值填写并验证。
