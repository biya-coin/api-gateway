#!/usr/bin/env python3
"""Render the self-built API portal (single offline HTML file).

Usage (run from api-gateway/ after merge-openapi.py):
    python3 scripts/render-portal.py [--in-dir docs/.generated]

Reads docs/.generated/openapi.json (+ version-matrix.json) and writes
docs/.generated/portal.html served by the gateway at GET /docs.

Layout (per product decision, no generic OpenAPI renderer):
  left   nav tree: per service -> POST items (one per type) + WS items
  main   per-item detail: params, response, notes, prefilled editable
         test case with send, plus generated cURL / TypeScript tabs
  WS     prefilled subscribe message with connect + live log

No CDN, no external assets, no new runtime dependencies.
"""

import argparse
import html
import json
from pathlib import Path

GATEWAY_ROOT = Path(__file__).resolve().parent.parent

SERVICE_ORDER = ["Exchange", "State", "Indexer"]
SERVICE_CN = {"Exchange": "交易池", "State": "状态服务", "Indexer": "Indexer"}
WS_TRANSPORT = "GET /ws（单连接多路复用）：订阅 {\"method\":\"subscribe\",\"subscription\":{\"type\": ... }}，心跳 {\"method\":\"ping\"} 回 {\"channel\":\"pong\"}"

# (service, type) -> response rendering. "schema" names a components schema
# (arrays expressed as [name]); "sample" is a literal illustration;
# "note" is shown when no honest shape can be modeled.
RESPONSE_MAP = {
    ("Exchange", "__exchange__"): ("schema", "Exchange__ExchangeResult"),
    ("Exchange", "health"): ("sample", {"status": "ok"}),
    ("Exchange", "stateInfo"): ("note", "透传 connected node 的 /info 结果，其中 blockHeight 是当前区块高度。"),
    ("Exchange", "block"): ("note", "透传 connected node 的区块内容（需带 height）。"),
    ("State", "meta"): ("schema", "State__MetaResponse"),
    ("State", "metaAndAssetCtxs"): ("note", "[meta, 行情数组] 二元数组，行情与 universe 一一对应、顺序相同。"),
    ("State", "clearinghouseState"): ("schema", "State__ClearinghouseStateResponse"),
    ("State", "openOrders"): ("schema", ["State__OpenOrderResponse"]),
    ("State", "frontendOpenOrders"): ("schema", ["State__NodeDataOrder"]),
    ("State", "extraAgents"): ("schema", ["State__ExtraAgentResponse"]),
    ("State", "activeAssetData"): ("schema", "State__ActiveAssetDataResponse"),
    ("State", "userFees"): ("schema", "State__UserFeesResponse"),
    ("State", "exchangeStatus"): ("schema", "State__ExchangeStatusResponse"),
    ("State", "orderStatus"): ("schema", "State__OrderStatusResponse"),
    ("Indexer", "metaAndAssetCtxs"): ("note", "[meta, 行情数组]；meta 取自上游节点，行情取自本地 ctxs。"),
    ("Indexer", "webData2"): ("note", "页面加载聚合对象（账户 + 挂单 + 元数据 + 行情）。"),
    ("Indexer", "allMids"): ("sample", {"mids": {"BTCUSDC": "76600.5"}}),
    ("Indexer", "candleSnapshot"): ("note", "K 线数组（SDK candle wire {t,T,s,i,o,c,h,l,v,n}）。"),
    ("Indexer", "orderStatus"): ("schema", "Indexer__OrderStatusResponse"),
    ("Indexer", "l2Book"): ("note", "本地订单簿引擎的盘口快照。"),
    ("Indexer", "fundingHistory"): ("note", "资金费历史数组。"),
    ("Indexer", "userFillsByTime"): ("note", "Fill 数组（Hyperliquid SDK Fill 原文透传）。"),
    ("Indexer", "userFills"): ("note", "Fill 数组（Hyperliquid SDK Fill 原文透传）。"),
    ("Indexer", "recentTrades"): ("note", "公开成交数组（与 WS trades 频道配对方式一致）。"),
    ("Indexer", "userFunding"): ("note", "资金费数组。"),
    ("Indexer", "userNonFundingLedgerUpdates"): ("note", "非资金类账本变更数组。"),
    ("Indexer", "historicalOrders"): ("schema", ["Indexer__HistoricalOrderItem"]),
}

# Status semantics per POST surface (verified against handler code).
STATUS_TABLES = {
    ("Exchange", "__exchange__"): [
        ("200", "本地处理结果：code 0=已入池；1001 参数/哈希失败；1002 重复交易；1003 签名失败。"),
        ("400", "malformed JSON，Axum 在进业务逻辑前拒绝。"),
    ],
    ("State", "__info__"): [
        ("200", "查询结果。"),
        ("400", "未知 type 或参数错误。"),
        ("501", "type 可解析但尚未实现，或 dex 不是默认 dex。"),
        ("503", "副本尚未同步就绪。"),
    ],
    ("Indexer", "__info__"): [
        ("200", "查询结果；历史存储未启用时历史类返回 []。"),
        ("400", "请求参数错误，或时间范围超出留存窗口。"),
        ("404", "非 indexer info type。"),
    ],
}

FIELD_OVERRIDES = {
    "user": "0x1111111111111111111111111111111111111111",
    "address": "0x1111111111111111111111111111111111111111",
    "destination": "0x1111111111111111111111111111111111111111",
    "agentAddress": "0x2222222222222222222222222222222222222222",
    "coin": "BTCUSDC",
    "interval": "1h",
    "height": 1,
    "oid": 2,
    "grouping": "na",
}


def esc(text):
    return html.escape(str(text), quote=True)


def gateway_routing():
    """(state_types, indexer_types) from the gateway router (single source of truth).
    A type in neither list gets HTTP 400 at the gateway."""
    import re

    code = (GATEWAY_ROOT / "src" / "routing.rs").read_text()

    def string_list(name):
        match = re.search(name + r": &\[&str\] = &\[(.*?)\];", code, re.S)
        return set(re.findall(r'"([^"]+)"', match.group(1))) if match else set()

    return string_list("STATE_TYPES"), string_list("INDEXER_TYPES")


def info_route_of(state_types, indexer_types, type_value):
    if type_value in state_types:
        return "State"
    if type_value in indexer_types:
        return "Indexer"
    return None


def ws_route_of(type_value):
    return "State" if type_value in ("assetCtxs", "clearinghouseState") else "Indexer"


def ws_prefill(sub):
    """Prefilled subscribe message: required-ish params only."""
    subscription = {"type": sub["type"]}
    for param in sub.get("params", []):
        name = param.rstrip("?")
        if "user" in name:
            subscription[name] = FIELD_OVERRIDES["user"]
        elif name == "coin":
            subscription[name] = "BTCUSDC"
        elif name == "interval":
            subscription[name] = "1h"
    return {"method": "subscribe", "subscription": subscription}


def resolve(schemas, schema):
    while isinstance(schema, dict) and "$ref" in schema:
        schema = schemas[schema["$ref"].split("/")[-1]]
    return schema


def sample_of(schemas, schema, field_name="", depth=0):
    """Best-effort sample value for a schema (request prefill + illustration)."""
    if depth > 6:
        return "…"
    schema = resolve(schemas, schema)
    if not isinstance(schema, dict):
        return None
    if "const" in schema:
        return schema["const"]
    if "example" in schema and not isinstance(schema.get("example"), dict):
        return schema["example"]
    if field_name in FIELD_OVERRIDES:
        return FIELD_OVERRIDES[field_name]
    kind = schema.get("type")
    if isinstance(kind, list):
        kind = [k for k in kind if k != "null"][0] if any(k != "null" for k in kind) else "string"
    if kind == "object":
        props = schema.get("properties", {})
        required = schema.get("required", [])
        out = {}
        for name in required + [n for n in props if n not in required]:
            if name in props:
                out[name] = sample_of(schemas, props[name], name, depth + 1)
        if not props and "additionalProperties" in schema:
            return {"BTCUSDC": "76600.5"}
        return out
    if kind == "array":
        if "恒为空" in schema.get("description", ""):
            return []
        items = schema.get("prefixItems") or ([schema["items"]] if "items" in schema else [])
        return [sample_of(schemas, item, field_name, depth + 1) for item in items]
    if kind == "string":
        for enum_values in [schema.get("enum")]:
            if enum_values:
                return enum_values[0]
        return ""
    if kind == "integer":
        return 0
    if kind == "number":
        return 0
    if kind == "boolean":
        return False
    for key in ("oneOf", "anyOf"):
        if schema.get(key):
            return sample_of(schemas, schema[key][0], field_name, depth + 1)
    return None


def type_label(prop):
    if "$ref" in prop:
        return prop["$ref"].split("/")[-1]
    kind = prop.get("type")
    if isinstance(kind, list):
        return " | ".join(x for x in kind if x != "null") or "string"
    if kind:
        return kind
    for key in ("oneOf", "anyOf"):
        if prop.get(key):
            return " | ".join(type_label(branch) for branch in prop[key])
    if "const" in prop:
        return "常量"
    if "enum" in prop:
        return "枚举"
    return "object"


def prefill_of(schemas, schema):
    """Prefill keeps required fields only (clean test case, user edits)."""
    schema = resolve(schemas, schema)
    if not isinstance(schema, dict) or schema.get("type") != "object":
        return sample_of(schemas, schema)
    out = {}
    for name in schema.get("required", []):
        prop = schema.get("properties", {}).get(name, {})
        out[name] = sample_of(schemas, prop, name, 1)
    return out


def type_const_of(variant):
    props = variant.get("properties", {})
    type_prop = props.get("type", {})
    if "const" in type_prop:
        return type_prop["const"]
    return None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--in-dir", default=str(GATEWAY_ROOT / "docs" / ".generated"))
    args = parser.parse_args()
    in_dir = Path(args.in_dir)
    spec = json.loads((in_dir / "openapi.json").read_text())
    matrix = json.loads((in_dir / "version-matrix.json").read_text())
    schemas = spec["components"]["schemas"]

    ws_by_service = {}
    for item in spec.get("x-ws-subscriptions", []):
        ws_by_service.setdefault(item["service"], []).append(item)

    service_of_tag = {"Exchange": "exchange-apiserver", "State": "bybchain-api-server", "Indexer": "biya-indexer"}
    state_types, indexer_types = gateway_routing()

    # ---- model: per-service POST items (one per type) + WS items ----
    post_items = []  # {id, service, kind, type, title, desc, params, required, response, prefill, path}
    for tag in ("State", "Indexer"):
        union_name = {"State": "State__InfoRequest", "Indexer": "Indexer__InfoRequest"}[tag]
        refs = []
        for ref in schemas[union_name].get("oneOf", []):
            nested = resolve(schemas, ref)
            # Flatten nested unions (a service union referenced as one variant).
            if "const" not in nested.get("properties", {}).get("type", {}) and nested.get("oneOf"):
                refs.extend(nested["oneOf"])
            else:
                refs.append(ref)
        for ref in refs:
            variant = resolve(schemas, ref)
            type_value = type_const_of(variant)
            if not type_value:
                continue
            props = variant.get("properties", {})
            required = variant.get("required", [])
            params = [
                {
                    "name": name,
                    "type": type_label(prop),
                    "required": name in required,
                    "desc": prop.get("description", ""),
                }
                for name, prop in props.items()
                if name != "type"
            ]
            resp = RESPONSE_MAP.get((tag, type_value), ("note", "见 operation 响应说明。"))
            post_items.append(
                {
                    "id": f"post-{tag}-{type_value}",
                    "service": tag,
                    "route": info_route_of(state_types, indexer_types, type_value),
                    "type": type_value,
                    "title": type_value,
                    "desc": variant.get("description", ""),
                    "params": params,
                    "response": resp,
                    "prefill": {"type": type_value, **prefill_of(schemas, variant)},
                    "path": "/info",
                }
            )
    # Exchange /exchange action submission.
    post_items.insert(
        0,
        {
            "id": "post-Exchange-exchange",
            "service": "Exchange",
            "route": "Exchange",
            "type": "__exchange__",
            "title": "/exchange · 提交签名动作",
            "desc": "提交已签名的 exchange action；accepted 仅表示通过本地校验进入交易池，不代表链上执行。",
            "params": [
                {"name": "action", "type": "object", "required": True, "desc": "9 种动作 oneOf（order/cancel/cancelByCloid/cancelAll/updateLeverage/batchModify/usdSend/withdraw3/approveAgent），。各动作字段见下表与片段说明。"},
                {"name": "nonce", "type": "integer", "required": True, "desc": "用户 nonce，必须大于 0。"},
                {"name": "signature", "type": "object", "required": True, "desc": "{r, s, v}；r/s 32 字节 hex，v 为 27/28。"},
                {"name": "vaultAddress", "type": "string", "required": False, "desc": "可选，20 字节 hex。"},
                {"name": "expiresAfter", "type": "integer", "required": False, "desc": "可选，参与签名。"},
            ],
            "response": ("schema", "Exchange__ExchangeResult"),
            "prefill": {
                "action": {"type": "order", "orders": [{"a": 0, "b": True, "p": "65000", "s": "0.001", "r": False, "t": {"limit": {"tif": "Gtc"}}}], "grouping": "na"},
                "nonce": 1774952773999,
                "signature": {"r": "0x" + "11" * 32, "s": "0x" + "22" * 32, "v": 27},
            },
            "path": "/exchange",
        },
    )
    RESPONSE_MAP[("Exchange", "__exchange__")] = ("schema", "Exchange__ExchangeResult")

    ws_items = []
    for tag in SERVICE_ORDER:
        for sub in ws_by_service.get(service_of_tag[tag], []):
            ws_items.append(
                {
                    "id": f"ws-{tag}-{sub['type']}",
                    "service": tag,
                    "route": ws_route_of(sub["type"]),
                    "type": sub["type"],
                    "params": sub.get("params", []),
                    "channel": sub.get("pushChannel", ""),
                    "data": sub.get("data", ""),
                    "official": sub.get("official", False),
                    "prefill": {"method": "subscribe", "subscription": ws_prefill(sub) },
                }
            )


    ws_urls = []
    for server in spec.get("servers", []):
        url = server["url"]
        if url.startswith("https://"):
            ws_urls.append("wss://" + url[len("https://"):] + "/ws")
        elif url.startswith("http://"):
            ws_urls.append("ws://" + url[len("http://"):] + "/ws")
    http_servers = [s["url"] for s in spec.get("servers", [])]

    store = {
        "services": [
            {
                "tag": tag,
                "cn": SERVICE_CN[tag],
                "posts": [i for i in post_items if i["service"] == tag],
                "subs": [i for i in ws_items if i["service"] == tag],
            }
            for tag in SERVICE_ORDER
        ],
        "httpServers": http_servers,
        "wsUrls": ws_urls,
        "matrix": matrix,
        "responses": build_responses(schemas),
    }
    page = render(store, spec)
    (in_dir / "portal.html").write_text(page, encoding="utf-8")
    print(f"render-portal: OK items={len(post_items)}post/{len(ws_items)}ws -> {in_dir / 'portal.html'}")


def build_responses(schemas):
    """Pre-rendered response fragments keyed by schema name or [name]."""
    rendered = {}

    def render_schema(name):
        if name in rendered:
            return rendered[name]
        schema = schemas[name]
        rendered[name] = {"kind": "schema", "name": name, "sample": sample_of(schemas, schema)}
        return rendered[name]

    return {"render_schema": render_schema, "schemas": schemas}


def response_html(store, service, type_value):
    kind, payload = RESPONSE_MAP.get((service, type_value), ("note", "见 operation 响应说明。"))
    render_schema = store["responses"]["render_schema"]
    if kind == "schema":
        if isinstance(payload, list):
            inner = render_schema(payload[0])
            sample = [inner["sample"]]
            label = f"{payload[0]} 数组"
        else:
            inner = render_schema(payload)
            sample = inner["sample"]
            label = payload
        return (
            f"<p class=\"resp-label\">{esc(label)}</p>"
            f"<pre class=\"code\">{esc(json.dumps(sample, ensure_ascii=False, indent=2))}</pre>"
        )
    if kind == "sample":
        return f"<pre class=\"code\">{esc(json.dumps(payload, ensure_ascii=False, indent=2))}</pre>"
    return f"<p class=\"note\">{esc(payload)}</p>"


def status_html(service, path):
    key = (service, "__exchange__" if path == "/exchange" else "__info__")
    rows = "".join(
        f"<tr><td><code>{esc(code)}</code></td><td>{esc(text)}</td></tr>"
        for code, text in STATUS_TABLES.get(key, [("200", "成功。")])
    )
    return f"<table class=\"kv\"><thead><tr><th>状态码</th><th>含义</th></tr></thead><tbody>{rows}</tbody></table>"


def render(store, spec):
    nav = render_nav(store)
    main = render_overview(store) + "".join(render_post(store, item) for svc in store["services"] for item in svc["posts"])
    main += "".join(render_ws(store, item) for svc in store["services"] for item in svc["subs"])
    data_json = json.dumps(
        {
            "posts": {i["id"]: {"path": i["path"]} for svc in store["services"] for i in svc["posts"]},
            "httpServers": store["httpServers"],
            "wsUrls": store["wsUrls"],
        },
        ensure_ascii=False,
    )
    # NOTE: raw JSON into <script> (no HTML-escaping: entities are not decoded
    # in script context and would corrupt the literal); break any </sequence.
    page = PAGE.replace("__NAV__", nav).replace("__MAIN__", main).replace(
        "__DATA_JSON__", data_json.replace("</", "<\\/")
    )
    assert "__NAV__" not in page and "__MAIN__" not in page and "__DATA_JSON__" not in page
    return page


def render_nav(store):
    parts = ['<div class="brand"><div><b>BIYA DEX API</b><span>网关聚合文档</span></div></div>']
    parts.append('<input id="nav-search" type="search" placeholder="搜索接口，如 orderStatus / l2Book" autocomplete="off">')
    parts.append('<a class="nav-item nav-overview" href="#overview">总览<span class="nav-sub">版本矩阵 · 路由 · 测试指南</span></a>')
    for svc in store["services"]:
        parts.append(f"<div class=\"nav-group\" data-service=\"{svc['tag']}\">")
        parts.append(f"<div class=\"nav-group-title\">{esc(svc['cn'])}<span>{svc['tag']}</span></div>")
        parts.append("<div class=\"nav-sec\">POST 接口</div>")
        for item in svc["posts"]:
            label = "动作提交" if item["type"] == "__exchange__" else item["type"]
            route = item.get("route")
            if route is None:
                hint = '<span class="nav-hint off">网关未开放</span>'
            elif route != svc["tag"]:
                hint = '<span class="nav-hint warn">→' + route + '</span>'
            else:
                hint = ''
            parts.append(
                f"<a class=\"nav-item\" data-id=\"{item['id']}\" data-keys=\"{esc(item['type'])} {esc(svc['tag'])} {esc(svc['cn'])}\" href=\"#{item['id']}\">"
                f"<span class=\"badge post\">POST</span><span class=\"nav-label\">{esc(label)}</span>{hint}</a>"
            )
        parts.append("<div class=\"nav-sec\">WS 订阅</div>")
        if svc["subs"]:
            for item in svc["subs"]:
                parts.append(
                    f"<a class=\"nav-item\" data-id=\"{item['id']}\" data-keys=\"{esc(item['type'])} ws {esc(svc['tag'])}\" href=\"#{item['id']}\">"
                    f"<span class=\"badge ws\">WS</span><span class=\"nav-label\">{esc(item['type'])}</span></a>"
                )
        else:
            parts.append("<div class=\"nav-empty\">本服务无 WS 订阅</div>")
        parts.append("</div>")
    return "\n".join(parts)


def render_overview(store):
    matrix = store["matrix"]
    rows = "".join(
        f"<tr><td><code>{esc(r['service'])}</code></td><td><code>{esc(r['rev'][:12])}</code></td></tr>"
        for r in [{"service": "gateway", "rev": matrix["gateway_rev"]}] + matrix["backends"]
    )
    return f"""<section id="overview" class="card hero">
<h1>BIYA DEX API <span>网关聚合文档</span></h1>
<p class="lede">前端唯一入口：<code>POST /info</code>、<code>POST /exchange</code>、<code>GET /ws</code> 均打网关，由网关按 type / 订阅分流到三后端，网关不改业务字段、不回退。左侧选接口，右侧看说明、改参数、直接发送。</p>
<p>共 <b>{len([i for svc in store['services'] for i in svc['posts']])}</b> 个 POST 接口、<b>{len([i for svc in store['services'] for i in svc['subs']])}</b> 个 WS 订阅。</p>
<h2>版本矩阵（本次构建拉取）</h2>
<table class="kv"><thead><tr><th>服务</th><th>rev</th></tr></thead><tbody>{rows}</tbody></table>
<p class="note">构建时间 {esc(matrix.get('fetched_at', ''))}。文档内容与上表 rev 严格对应；发版即重拉，旧页面不复用。</p>
<h2>测试指南</h2>
<ul class="guide">
<li>每个 POST 接口自带填好参数的测试用例，改完点发送，结果原样展示（含业务错误体）。</li>
<li>每个 WS 订阅自带订阅报文，点连接即收消息；断开会显示 close code。</li>
<li>右上可切换服务器（公网 / 139 本机）。浏览器无痕模式同样可用，无需登录。</li>
</ul></section>"""


def render_post(store, item):
    if item["params"]:
        param_rows = "".join(
            f"<tr><td><code>{esc(p['name'])}</code>{' <b class=\"req\">*</b>' if p['required'] else ''}</td>"
            f"<td><code>{esc(p['type'])}</code></td><td>{esc(p['desc'])}</td></tr>"
            for p in item["params"]
        )
        params = f"<table class=\"kv\"><thead><tr><th>参数</th><th>类型</th><th>说明</th></tr></thead><tbody>{param_rows}</tbody></table>"
    else:
        params = "<p class=\"note\">无业务参数（仅 type 常量）。</p>"
    route = item.get("route")
    if route is None:
        route_note = '<b class="err">网关未开放（400），仅后端直连可用。</b>'
    elif route != item["service"]:
        route_note = '<b class="warn">注意：经网关走 ' + route + '（本节为 ' + item['service'] + ' 直连口径）。</b>'
    else:
        route_note = {
            "Exchange": "经网关走 <b>交易池</b>。",
            "State": "经网关走 <b>状态服务</b>。",
            "Indexer": "经网关走 <b>Indexer</b>。",
        }[item["service"]]
    prefill = json.dumps(item["prefill"], ensure_ascii=False, indent=2)
    return f"""<section id="{item['id']}" class="card">
<div class="crumb">{esc(item['service'])} · POST {esc(item['path'])}</div>
<h2><span class="badge post">POST</span> {esc(item['title'])} <span class=\"route\">{route_note}</span></h2>
<p>{esc(item['desc'])}</p>
<h3>入参</h3>{params}
<h3>响应</h3>{status_html(item['service'], item['path'])}{response_html(store, item['service'], item['type'])}
<h3>测试用例</h3>
<div class="try" data-kind="post" data-id="{item['id']}">
<div class="try-bar"><label>服务器 <select class="try-server"></select></label>
<div class="modetabs"><button class="mode on" data-mode="json">JSON</button><button class="mode" data-mode="curl">cURL</button><button class="mode" data-mode="ts">TypeScript</button></div>
<button class="send">发送</button><span class="try-meta"></span></div>
<textarea class="try-body" spellcheck="false">{esc(prefill)}</textarea>
<pre class="code try-view hidden"></pre>
<div class="try-bar"><button class="copybtn hidden">复制当前预览</button></div>
<pre class="code try-resp" hidden></pre>
</div></section>"""


def render_ws(store, item):
    official = "官方订阅" if item["official"] else "自加扩展（标准 SDK 不发送）"
    route_note = "经网关走 <b>" + item["route"] + "</b>。"
    return f"""<section id="{item['id']}" class="card">
<div class="crumb">{esc(item['service'])} · WebSocket /ws</div>
<h2><span class="badge ws\">WS</span> {esc(item['type'])} <span class="route\">{official} · {route_note}</span></h2>
<p>{esc(WS_TRANSPORT)}</p>
<table class="kv\"><thead><tr><th>参数</th><th>推送 channel</th><th>数据</th></tr></thead>
<tbody><tr><td><code>{esc(', '.join(item['params']) or '—')}</code></td><td><code>{esc(item['channel'])}</code></td><td>{esc(item['data'])}</td></tr></tbody></table>
<h3>订阅测试</h3>
<div class="try" data-kind="ws" data-id="{item['id']}">
<div class="try-bar"><label>服务器 <select class="try-ws-server\"></select></label>
<button class="ws-connect\">连接并订阅</button><button class="ws-ping\">心跳</button><button class="ws-close\">断开</button></div>
<textarea class="try-body\" spellcheck="false\">{esc(json.dumps(item['prefill'], ensure_ascii=False, indent=2))}</textarea>
<pre class="code ws-log\">等待连接…</pre>
</div></section>"""


PAGE = """<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>BIYA DEX API · 网关聚合文档</title>
<style>
:root{
  --ink:#111111; --muted:#4b5563; --line:#e2e8f0; --paper:#ffffff; --wash:#f6f8fb;
  --accent:#0b6b4f; --accent-ink:#084f3a; --rose:#d61f5f; --sky:#0284c7; --amber:#b45309; --code-bg:#101828; --code-ink:#e6edf7;
  --post:#0b6b4f; --ws:#d61f5f; --radius:12px;
}
*{box-sizing:border-box}
html{scroll-behavior:smooth}
body{margin:0; color:var(--ink); background:var(--wash);
  font-family:"Avenir Next","Helvetica Neue","PingFang SC","Hiragino Sans GB","Microsoft YaHei",sans-serif;
  font-size:15px; line-height:1.7;}
code,pre,textarea,select{font-family:ui-monospace,"SF Mono","Cascadia Code",Consolas,monospace; font-size:13px;}
a{color:var(--sky)}
.layout{display:flex; min-height:100vh;}
aside{width:308px; flex:none; background:#ffffff; color:var(--ink); position:sticky; top:0; height:100vh; overflow-y:auto; padding:20px 14px; border-right:1px solid var(--line);}
.brand{display:flex; gap:10px; align-items:center; padding:2px 6px 14px;}

.brand b{display:block; color:var(--ink); font-size:15px; letter-spacing:.4px;}
.brand span{display:block; font-size:12px; color:var(--muted);}
#nav-search{width:100%; padding:8px 10px; border-radius:8px; border:1px solid var(--line); background:#fff; color:var(--ink); margin-bottom:12px;}
.nav-item{display:block; color:#1f2937; text-decoration:none; padding:6px 8px; border-radius:8px; font-size:13.5px;}
.nav-item:hover{background:#e2e8f0;}
.nav-item.active{background:var(--accent); color:#fff;}
.nav-sub{display:block; font-size:11.5px; color:var(--muted);}
.nav-item.active .nav-sub{color:rgba(255,255,255,.85);}
.nav-group{margin-top:10px; border-top:1px solid var(--line); padding-top:10px;}
.nav-group-title{padding:4px 8px; font-weight:700; color:var(--ink); font-size:14px; display:flex; justify-content:space-between;}
.nav-group-title span{font-weight:400; font-size:11px; color:#fff; background:var(--sky); border:1px solid var(--sky); border-radius:20px; padding:0 8px;}
.nav-sec{padding:8px 8px 2px; font-size:11.5px; letter-spacing:1px; color:var(--muted);}
.nav-label{margin-left:7px;}
.nav-empty{padding:2px 8px 6px 30px; font-size:12.5px; color:#64748b;}
.badge{display:inline-block; font-size:11px; font-weight:700; border-radius:6px; padding:1px 7px; color:#fff; vertical-align:1px;}
.badge.post{background:var(--post);}
.badge.ws{background:var(--ws);}
main{flex:1; min-width:0; padding:28px 36px 80px; max-width:1060px;}
.card{background:var(--paper); border:1px solid var(--line); border-radius:var(--radius); padding:24px 28px; margin-bottom:22px; box-shadow:0 1px 2px rgba(16,24,40,.05); scroll-margin-top:18px;}
.hero h1{margin:0 0 6px; font-size:30px; letter-spacing:.5px;}
.hero h1 span{font-size:17px; color:var(--muted); font-weight:400;}
.lede{color:var(--muted); max-width:70ch;}
h2{font-size:20px; margin:22px 0 8px;}
h2 .route{font-size:13px; color:var(--muted); font-weight:400;}
h3{font-size:15.5px; margin:18px 0 6px; padding-left:10px; border-left:3px solid var(--accent);}
.crumb{font-size:12px; color:var(--muted); letter-spacing:.4px;}
table.kv{width:100%; border-collapse:collapse; margin:8px 0 4px; font-size:13.5px;}
table.kv th,table.kv td{border:1px solid var(--line); padding:7px 10px; text-align:left; vertical-align:top;}
table.kv th{background:#f1f5fa; white-space:nowrap;}
.req{color:#dc2626;}
.note{color:var(--muted); font-size:13.5px;}
.resp-label{font-weight:700;}
pre.code{background:var(--code-bg); color:var(--code-ink); border-radius:10px; padding:14px 16px; overflow:auto; max-height:420px; white-space:pre-wrap; word-break:break-word;}
.guide{margin:6px 0; padding-left:20px;}
.try{border:1px dashed #b9c6d8; border-radius:10px; padding:12px 14px; background:#fafcff;}
.try-bar{display:flex; gap:10px; align-items:center; flex-wrap:wrap; margin-bottom:8px;}
.try-bar select{padding:5px 8px; border-radius:7px; border:1px solid var(--line); background:#fff; max-width:340px;}


.nav-hint{font-size:11px; border-radius:5px; padding:0 6px; margin-left:6px;}
textarea.try-body,pre.try-view{width:100%; height:220px; overflow:auto; resize:none;} textarea.try-body{border:1px solid var(--line); border-radius:8px; padding:10px 12px; background:#fff;} .modetabs{display:flex; gap:6px;} .mode{border:1px solid var(--line); background:#fff; border-radius:7px; padding:4px 12px; cursor:pointer;} .mode.on{background:var(--ink); color:#fff; border-color:var(--ink);} .copybtn{border:1px solid var(--line); background:#fff; border-radius:7px; padding:4px 12px; cursor:pointer; color:var(--sky); font-weight:700;}
button.send,.ws-connect,.ws-ping,.ws-close{border:none; border-radius:8px; padding:7px 18px; cursor:pointer; font-weight:700;}
button.send,.ws-connect{background:var(--accent); color:#fff;}
button.send:hover,.ws-connect:hover{background:var(--accent-ink);}
.ws-ping,.ws-close{background:#e2e8f0; color:var(--ink);}
.try-meta{font-size:12.5px; color:var(--muted);}
.try-resp{margin-top:10px;}
.ok{color:#16a34a;} .err{color:#dc2626;} .warn{color:var(--amber);}
.nav-hint{font-size:11px; border-radius:5px; padding:0 6px; margin-left:6px;}
.nav-hint.warn{background:#451a03; color:#fbbf24;}
.nav-hint.off{background:#334155; color:#94a3b8;}
.ws-log{min-height:60px;}
.hidden{display:none;}
@media (max-width:900px){ aside{width:230px;} main{padding:20px;} }
</style>
</head>
<body>
<div class="layout"><aside>__NAV__</aside><main>__MAIN__</main></div>
<script>
var STORE = __DATA_JSON__;
function $(s, r){ return (r||document).querySelector(s); }
function $all(s, r){ return Array.prototype.slice.call((r||document).querySelectorAll(s)); }
// nav search + active state
var search = $("#nav-search");
search.addEventListener("input", function(){
  var q = search.value.trim().toLowerCase();
  $all(".nav-item[data-id]").forEach(function(a){
    a.style.display = (!q || (a.getAttribute("data-keys")||"").toLowerCase().indexOf(q) >= 0) ? "" : "none";
  });
  $all(".nav-group").forEach(function(g){
    var any = $all(".nav-item[data-id]", g).some(function(a){ return a.style.display !== "none"; });
    g.style.display = any || !q ? "" : "none";
  });
});
var sections = $all("main section[id]");
function markActive(){
  var cur = null;
  sections.forEach(function(s){ if (s.getBoundingClientRect().top < 120) cur = s.id; });
  $all(".nav-item").forEach(function(a){ a.classList.toggle("active", a.getAttribute("href") === "#" + cur); });
}
document.addEventListener("scroll", markActive); markActive();
// fill server selects
$all(".try[data-kind='post']").forEach(function(box){
  var sel = $(".try-server", box);
  STORE.httpServers.forEach(function(u){ var o = document.createElement("option"); o.value = u; o.textContent = u; sel.appendChild(o); });
});
$all(".try[data-kind='ws']").forEach(function(box){
  var sel = $(".try-ws-server", box);
  STORE.wsUrls.forEach(function(u){ var o = document.createElement("option"); o.value = u; o.textContent = u; sel.appendChild(o); });
});
function curlFor(server, path, body){ return "curl -X POST '" + server + path + "' -H 'content-type: application/json' --data-raw '" + body.replace(/'/g, "'\\''") + "'"; }
function tsFor(server, path, body){
  var pretty;
  try { pretty = JSON.stringify(JSON.parse(body), null, 2); } catch(e){ pretty = body; }
  var q = String.fromCharCode(34);
  var nl = String.fromCharCode(10);
  return '// ' + server + path + nl + 'const res = await fetch(' + q + server + path + q + ', {' + nl + '  method: ' + q + 'POST' + q + ',' + nl + '  headers: { ' + q + 'Content-Type' + q + ': ' + q + 'application/json' + q + ' },' + nl + '  body: JSON.stringify(' + pretty + ', null, 2),' + nl + '});' + nl + 'const data = await res.json();';
}
// post try-it
$all(".try[data-kind='post']").forEach(function(box){
  var id = box.getAttribute("data-id");
  var meta = STORE.posts[id];
  var area = $(".try-body", box), resp = $(".try-resp", box), info = $(".try-meta", box);
  var server = function(){ return $(".try-server", box).value; };
  var view = $(".try-view", box), copyBtn = $(".copybtn", box);
  function show(mode){
    $all(".mode", box).forEach(function(x){ x.classList.toggle("on", x.getAttribute("data-mode") === mode); });
    if (mode === "json"){ area.classList.remove("hidden"); view.classList.add("hidden"); copyBtn.classList.add("hidden"); }
    else { area.classList.add("hidden"); view.classList.remove("hidden"); copyBtn.classList.remove("hidden");
      view.textContent = mode === "curl" ? curlFor(server(), meta.path, area.value) : tsFor(server(), meta.path, area.value); }
  }
  $all(".mode", box).forEach(function(x){ x.addEventListener("click", function(){ show(x.getAttribute("data-mode")); }); });
  area.addEventListener("input", function(){ var m = $(".mode.on", box).getAttribute("data-mode"); if (m !== "json") show(m); });
  copyBtn.addEventListener("click", function(){
    function done(){ copyBtn.textContent = "已复制"; setTimeout(function(){ copyBtn.textContent = "复制当前预览"; }, 1200); }
    if (navigator.clipboard && navigator.clipboard.writeText){ navigator.clipboard.writeText(view.textContent).then(done, function(){ fallback(); }); }
    else { fallback(); }
    function fallback(){ var ta = document.createElement("textarea"); ta.value = view.textContent; document.body.appendChild(ta); ta.select(); try { document.execCommand("copy"); done(); } catch(e){} document.body.removeChild(ta); }
  });
  $(".send", box).addEventListener("click", function(){
    var payload;
    try { payload = JSON.parse(area.value); } catch(e){ resp.classList.remove("hidden"); resp.innerHTML = "<span class='err'>JSON 解析失败：</span>" + e.message; return; }
    info.textContent = "发送中…"; resp.classList.add("hidden");
    var t0 = Date.now();
    fetch(server() + meta.path, {method: "POST", headers: {"Content-Type": "application/json"}, body: JSON.stringify(payload)})
      .then(function(r){ return r.text().then(function(t){ return {status: r.status, body: t}; }); })
      .then(function(r){
        info.innerHTML = "HTTP <b>" + r.status + "</b> · " + (Date.now() - t0) + "ms";
        resp.classList.remove("hidden"); resp.textContent = "HTTP " + r.status + String.fromCharCode(10) + r.body;
      })
      .catch(function(e){ info.textContent = "请求失败"; resp.classList.remove("hidden"); resp.textContent = String(e); });
  });
});
function escHtml(s){ return s.replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;"); }
function stamp(){ var d = new Date(); return d.toTimeString().slice(0,8); }
// ws try-it
$all(".try[data-kind='ws']").forEach(function(box){
  var id = box.getAttribute("data-id");
  var area = $(".try-body", box), log = $(".ws-log", box), sock = null;
  function line(cls, text){ log.innerHTML += "<div><span>[" + stamp() + "]</span> <span class='" + cls + "'>" + escHtml(text) + "</span></div>"; log.scrollTop = log.scrollHeight; }
  $(".ws-connect", box).addEventListener("click", function(){
    if (sock){ line("err", "已有连接，先断开。"); return; }
    var url = $(".try-ws-server", box).value;
    var msg;
    try { msg = JSON.stringify(JSON.parse(area.value)); } catch(e){ line("err", "订阅 JSON 解析失败：" + e.message); return; }
    line("", "连接 " + url + " …");
    try { sock = new WebSocket(url); } catch(e){ line("err", String(e)); return; }
    sock.onopen = function(){ line("ok", "已连接，发送订阅"); sock.send(msg); };
    sock.onmessage = function(ev){ line("", "<< " + ev.data); };
    sock.onerror = function(){ line("err", "连接错误（可能是 Origin 未放行或网络问题）"); };
    sock.onclose = function(ev){ line("err", "连接关闭 code=" + ev.code + " reason=" + (ev.reason || "(空)") + " clean=" + ev.wasClean); sock = null; };
  });
  $(".ws-ping", box).addEventListener("click", function(){
    if (!sock){ line("err", "未连接。"); return; }
    sock.send('{"method":"ping"}'); line("", ">> ping");
  });
  $(".ws-close", box).addEventListener("click", function(){ if (sock){ sock.close(); } });
});
</script>
</body>
</html>
"""


if __name__ == "__main__":
    main()
