# 聚合接口文档方案（网关统一呈现，三后端供数）

## 1. 目标

* 用户只记网关一个文档地址，就能看到 `exchange / state(indexer 以外的状态服务) / indexer` 三个后端服务的详细接口。
* 网关本身只是转发，没有业务接口，不为网关立章节；页内只保留一段路由说明（哪个 `type` 走哪个服务）。
* 每次网关发版都重新拉取三后端的最新文档拼装，不是静态写死。拉不到或版本对不上就让构建失败，不静默用旧文档顶。

非目标：本次只定机制，不写各接口的 schema，不改任何服务的 `Router`，不装 `utoipa` 等依赖。

## 2. 总体形态

```text
三后端（数据源，各自维护）：
  GET /openapi.json   本服务 HTTP 接口（OpenAPI 3.1 片段）
  GET /version        {"service":"exchange-apiserver","rev":"<git-sha>"}
  ws-subscriptions.json（与 openapi 同源，见 §6）

网关发版时（构建步）：
  拉三份 openapi.json + 版本 → 合并成一份 → embed 进网关镜像

网关运行时（用户唯一入口）：
  GET /docs           自研门户单页（左导航按服务分组，离线无 CDN），TAG = Exchange / State / Indexer
  GET /openapi.json   合并后的总 spec（机器读）
```

页面顶部必须打出版本矩阵（格式见 §5），否则“看到的文档”无法证明是“线上跑的版本”。

## 3. 数据源契约（三后端要提供的）

每个后端在其仓库维护自己的 spec 片段，并暴露两个只读地址：

| 地址 | 内容 | 要求 |
|---|---|---|
| `GET /openapi.json` | OpenAPI 3.1，本服务 HTTP 部分 | `info.version` = 服务 git rev；`tags` 只用自己名（`Exchange` / `State` / `Indexer`）；schema 名加服务前缀，避免合并冲突（如 `Exchange__SignedAction`） |
| `GET /version` | `{"service":"...","rev":"..."}` | 与 `openapi.json` 同一次构建产物 |
| `ws-subscriptions.json` | 本服务 WS 订阅表（类型、参数、推送帧示例） | 与 `openapi.json` 同目录维护，格式由网关合并脚本约定（先从交易服务试点定稿） |

注意：三服务对外都是 `POST /info` + `GET /ws`，直接拼 path 会撞车。合并时不按 URL 拼，按下面的规则处理（§4）。

## 4. 合并规则（网关构建脚本做）

脚本位置（预留，本文只定行为，不实现）：`api-gateway/scripts/merge-openapi.py`，输入输出：

```text
in:  exchange.json state.json indexer.json + 三者的 /version
out: api-gateway/docs/.generated/openapi.json + version-matrix.json
```

规则：

1. `POST /exchange`：整体取 exchange 片段。
2. `POST /info`：三个片段的 `requestBody` 按 `type` 字段做 `oneOf + discriminator` 合并；`responses` 同理。网关不再为每个 type 手写 schema，只维护“哪个 type 归哪个服务”的路由表（以 `src/routing.rs` 的 `STATE_TYPES / INDEXER_TYPES` 为准，文档页原样展示该表）。
3. `components.schemas`：全部按 `Exchange__ / State__ / Indexer__` 重命名后合并，重名即构建失败。
4. `GET /ws`：不进 OpenAPI `paths`（OpenAPI 表达不了我们的单连接多路复用订阅），统一放到门户各服务分组的“WebSocket 订阅”一节，数据源是三份 `ws-subscriptions.json`（§6）。
5. 总 spec 的 `info.version` = 网关 rev；`x-backend-revs` 扩展字段记录三后端 rev（页首版本矩阵就读它）。

## 5. 版本矩阵格式

合并产物 `version-matrix.json`（门户总览渲染，`GET /openapi.json` 里以 `x-backend-revs` 同步携带）：

```json
{
  "gateway_rev": "7e01d09",
  "fetched_at": "2026-09-20T00:00:00Z",
  "backends": [
    {"service": "exchange-apiserver", "rev": "<sha>", "source": "git:dev@<sha>"},
    {"service": "bybchain-api-server", "rev": "<sha>", "source": "git:dev@<sha>"},
    {"service": "biya-indexer", "rev": "<sha>", "source": "git:main@<sha>"}
  ]
}
```

`source` 二选一并如实标注：`git:<branch>@<sha>`（从仓库拉）或 `live:http://...:/openapi.json`（从运行实例拉，默认只用于联调）。正式发版必须用 git 源，`live` 源打出的页面要标“联调快照，非发版口径”。

## 6. WS 订阅同步

OpenAPI 不覆盖 WS，所以 WS 走伴生机制，不单独立项：

* 每个后端在自己仓库维护 `ws-subscriptions.json`（与 `openapi.json` 同一次评审合入）。
* 网关合并时拼成统一页的一节，按 `exchange / state / indexer` 分组；`userHistoricalOrders` 等自加订阅必须标“非官方扩展”。
* 网关不校验订阅语义，只展示“订阅 type → 后端”归属（以 `src/routing.rs::websocket_backend` 为准）。

## 7. 发版同步流程

```text
1. 各后端合入接口变更时，同 PR 更新自己的 openapi.json + ws-subscriptions.json（否则合并不通过）。
2. 网关发版构建：merge 脚本按 manifest（预留：api-gateway/docs/api-sources.json，
   记录三后端的仓库地址 + 分支 + rev）拉取三份 spec + version。
3. 任一拉取失败、rev 与 manifest 不一致、schema 重名未加前缀 → 构建失败。
4. 构建产物（合并 spec + version-matrix）embed 进网关镜像，随网关发布。
5. 发布后验证：打开 /docs 核对页首四 rev；curl /openapi.json 校验 x-backend-revs；
   抽查一个新增 type 的参数表与示例。
```

网关 `Router` 改动（实施阶段才做）：在 `src/server.rs::build_router` 旁加 `GET /docs`（`scripts/render-portal.py` 生成的自研门户）与 `GET /openapi.json`（合并产物 `include_str!`），与 `/healthz` 一样走本机路由，不进后端代理。不引入文档 UI 依赖。

## 8. 验收标准

* 打开网关 `/docs`：一页内可见 Exchange / State / Indexer 三组，HTTP 参数表 + 请求/响应示例可读，WS 订阅独立成节。
* 页首版本矩阵四 rev 可见，且与各服务 `GET /version` 一致。
* `GET /openapi.json` 无 schema 冲突；`GET /docs` 离线可开，导航条目与内容区块一一对应。
* 任一后端 spec 拉取失败时，网关构建失败而非发布旧文档。
* 前端确认：只用网关这一个地址即可联调，不再需要翻三个仓库找文档。

## 9. 风险与顺序

* 最大前置依赖：三后端现在都没有 `openapi.json`（三服务均为 `axum` 手写 `Json<Value>` 分发，`utoipa` 零接线），网关聚合框架先行，内容按“交易服务试点 → 状态服务 → indexer”顺序补。
* `/info` 多态的 `oneOf` 是主要体力活，试点时先把写法定稿，后两个服务照抄。
* 本文件是方案，不产生代码与依赖变更；实施（各后端 spec 片段、合并脚本、网关 `/docs` 路由）需另行逐项授权。
