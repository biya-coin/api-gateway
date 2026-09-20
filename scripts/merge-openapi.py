#!/usr/bin/env python3
"""Merge the three backend openapi.json fragments into one gateway spec.

Usage (run from api-gateway/):
    python3 scripts/merge-openapi.py [--local-root ..] [--out-dir docs/.generated]

Reads docs/api-sources.json, loads each backend fragment from the sibling
checkout (or --local-root override), validates it, and writes:
    <out-dir>/openapi.json        merged spec, embedded into the gateway image
    <out-dir>/version-matrix.json rev matrix rendered at the top of /docs

Fail-closed: any missing file, rev mismatch, tag/prefix violation,
unresolvable $ref, or schema-name collision aborts the build.
"""

import argparse
import json
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

GATEWAY_ROOT = Path(__file__).resolve().parent.parent
PREFIX = {"Exchange": "Exchange__", "State": "State__", "Indexer": "Indexer__"}


def fail(message):
    print(f"merge-openapi: ERROR: {message}", file=sys.stderr)
    sys.exit(1)


def git_rev(path):
    try:
        return subprocess.check_output(
            ["git", "-C", str(path), "rev-parse", "HEAD"], text=True
        ).strip()
    except subprocess.CalledProcessError:
        return None


def load_fragment(local_root, entry, strict):
    service = entry["service"]
    repo_dir = local_root / {
        "exchange-apiserver": "exchange-apiserver",
        "bybchain-api-server": "bybchain-api-server",
        "biya-indexer": "biya-indexer",
    }[service]
    spec_file = repo_dir / entry["spec_path"]
    if not spec_file.is_file():
        fail(f"{service}: missing {spec_file}")
    spec = json.loads(spec_file.read_text())
    if spec.get("openapi") != "3.1.0":
        fail(f"{service}: expected openapi 3.1.0, got {spec.get('openapi')}")
    tags = [t["name"] for t in spec.get("tags", [])]
    if tags != [entry["tag"]]:
        fail(f"{service}: expected single tag [{entry['tag']}], got {tags}")
    schemas = spec.get("components", {}).get("schemas", {})
    bad = [k for k in schemas if not k.startswith(PREFIX[entry["tag"]])]
    if bad:
        fail(f"{service}: schemas without {PREFIX[entry['tag']]} prefix: {bad}")
    refs = set()

    def collect(node):
        if isinstance(node, dict):
            for k, v in node.items():
                if k == "$ref" and isinstance(v, str):
                    refs.add(v)
                else:
                    collect(v)
        elif isinstance(node, list):
            for item in node:
                collect(item)

    collect(spec)
    missing = {
        r.split("/")[-1]
        for r in refs
        if r.startswith("#/components/schemas/") and r.split("/")[-1] not in schemas
    }
    if missing:
        fail(f"{service}: unresolvable $refs: {sorted(missing)}")
    actual_rev = git_rev(repo_dir)
    if strict and actual_rev != entry["rev"]:
        fail(
            f"{service}: checkout rev {actual_rev} != manifest rev {entry['rev']}; "
            "update docs/api-sources.json or release the backend first"
        )
    return spec, actual_rev or entry["rev"]


def routing_tables():
    """Extract the gateway routing ownership from src/routing.rs so /docs
    can never disagree with the code. Falls back to None on parse failure."""
    try:
        code = (GATEWAY_ROOT / "src" / "routing.rs").read_text()
    except OSError:
        return None, None

    def string_list(name):
        match = re.search(name + r": &\[&str\] = &\[(.*?)\];", code, re.S)
        if not match:
            return None
        return re.findall(r'"([^"]+)"', match.group(1))

    return string_list("INDEXER_TYPES"), string_list("STATE_TYPES")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--local-root", default=str(GATEWAY_ROOT.parent))
    parser.add_argument("--out-dir", default=str(GATEWAY_ROOT / "docs" / ".generated"))
    parser.add_argument("--gateway-rev", default=None)
    parser.add_argument("--no-strict", action="store_true")
    args = parser.parse_args()

    local_root = Path(args.local_root)
    out_dir = Path(args.out_dir)
    strict = not args.no_strict
    manifest = json.loads((GATEWAY_ROOT / "docs" / "api-sources.json").read_text())
    gateway_rev = args.gateway_rev or git_rev(GATEWAY_ROOT) or "unknown"
    if gateway_rev is None:
        fail("cannot determine gateway rev")

    fragments = {}
    revs = []
    for entry in manifest["backends"]:
        spec, actual = load_fragment(local_root, entry, strict)
        fragments[entry["tag"]] = (entry, spec)
        revs.append(
            {"service": entry["service"], "rev": actual, "source": f"git:{entry['branch']}@{actual}"}
        )

    merged_schemas = {}
    for tag, (_, spec) in fragments.items():
        for name, schema in spec.get("components", {}).get("schemas", {}).items():
            if name in merged_schemas:
                fail(f"schema name collision: {name}")
            merged_schemas[name] = schema
    merged_schemas["Gateway__Error"] = {
        "type": "object",
        "required": ["error"],
        "properties": {"error": {"type": "string"}},
    }

    # Only gateway-routed paths are published: /info (merged below) and
    # /exchange. Backend ops probes (/health*, /v1/replica, /l4Book) are not
    # reachable via the gateway and stay out of the unified page.
    merged_paths = {}
    for tag, (_, spec) in fragments.items():
        for path, item in spec.get("paths", {}).items():
            if path in ("/info", "/exchange"):
                continue
            print(f"merge-openapi: skip non-gateway path {path} ({tag})", file=sys.stderr)

    info_requests = []
    info_responses = []
    for tag in ("Exchange", "State", "Indexer"):
        _, spec = fragments[tag]
        info_item = spec["paths"].get("/info", {}).get("post", {})
        request_ref = (
            info_item.get("requestBody", {})
            .get("content", {})
            .get("application/json", {})
            .get("schema", {})
        )
        response_ref = (
            info_item.get("responses", {})
            .get("200", {})
            .get("content", {})
            .get("application/json", {})
            .get("schema", {})
        )
        if request_ref:
            info_requests.append(request_ref)
        if response_ref:
            info_responses.append(response_ref)
    if len(info_requests) != 3:
        fail(f"expected /info in all 3 fragments, found {len(info_requests)}")

    indexer_types, state_types = routing_tables()
    if indexer_types is None or state_types is None:
        print("merge-openapi: WARN: cannot parse src/routing.rs, routing table omitted", file=sys.stderr)
        routing_md = "路由归属以网关 src/routing.rs 为准（本次构建未能解析，见仓库代码）。"
    else:
        routing_md = (
            "网关按 `type` 分流（以本次构建的 `src/routing.rs` 为准）：\n\n"
            f"- State：{', '.join(f'`{t}`' for t in state_types)}\n"
            f"- Indexer：{', '.join(f'`{t}`' for t in indexer_types)}\n\n"
            "WS 订阅：`assetCtxs` / `clearinghouseState` 走状态服务，其余走 indexer。"
        )

    ws_sections = []
    ws_combined = []
    for tag in ("Exchange", "State", "Indexer"):
        entry, _ = fragments[tag]
        if not entry.get("ws_path"):
            continue
        service = entry["service"]
        repo_dir = local_root / {
            "exchange-apiserver": "exchange-apiserver",
            "bybchain-api-server": "bybchain-api-server",
            "biya-indexer": "biya-indexer",
        }[service]
        ws_file = repo_dir / entry["ws_path"]
        if not ws_file.is_file():
            ws_sections.append(f"### {tag}\n\nWS 订阅文档待补（{entry['ws_path']} 缺失）。")
            continue
        ws_doc = json.loads(ws_file.read_text())
        subs = ws_doc.get("subscriptions", [])
        ws_combined.extend([{**s, "service": service} for s in subs])
        rows = "\n".join(
            f"| `{s['type']}` | {', '.join(f'`{p}`' for p in s.get('params', [])) or '—'} "
            f"| `{s.get('pushChannel', '')}` | {s.get('data', '')} "
            f"| {'官方' if s.get('official') else '自加扩展'} |"
            for s in subs
        )
        ws_sections.append(
            f"### {tag}（{len(subs)} 种）\n\n| 订阅 | 参数 | 推送 channel | 数据 | 来源 |\n"
            f"|---|---|---|---|---|\n{rows}"
        )
    if not ws_combined:
        ws_sections.append("WS 订阅文档待补：三后端均未提供 ws-subscriptions.json。")

    rev_rows = "\n".join(
        f"| `{r['service']}` | `{r['rev']}` |" for r in [{"service": "gateway", "rev": gateway_rev}] + revs
    )
    rev_md = (
        "## 版本矩阵（本次构建拉取）\n\n| 服务 | rev |\n|---|---|\n" + rev_rows
    )
    merged = {
        "openapi": "3.1.0",
        "info": {
            "title": "BIYA DEX API（网关聚合）",
            "version": gateway_rev,
            "description": (
                "前端唯一入口：`POST /info`、`POST /exchange`、`GET /ws` 均打网关，由网关按 type/订阅分流到三后端，"
                "网关不改业务字段、不回退。\n\n" + rev_md + "\n\n" + routing_md + "\n\n## WebSocket 订阅\n\n" + "\n\n".join(ws_sections)
            ),
        },
        "tags": [
            {"name": "Exchange", "description": "交易池：签名动作接收与本地校验入池。"},
            {"name": "State", "description": "状态服务：当前状态查询。"},
            {"name": "Indexer", "description": "Indexer：历史、订单簿与行情订阅。"},
        ],
        "paths": {
            "/exchange": fragments["Exchange"][1]["paths"]["/exchange"],
            "/info": {
                "post": {
                    "tags": ["Exchange", "State", "Indexer"],
                    "summary": "统一查询入口（网关按 type 分流）",
                    "requestBody": {
                        "required": True,
                        "content": {"application/json": {"schema": {"oneOf": info_requests}}},
                    },
                    "responses": {
                        "200": {
                            "description": "后端查询结果（下游错误也可能以 200 + 业务错误体返回，见各服务说明）。",
                            "content": {"application/json": {"schema": {"oneOf": info_responses}}},
                        },
                        "400": {
                            "description": "未知 type 或参数错误（网关直接返回，不进后端）。",
                            "content": {
                                "application/json": {"schema": {"$ref": "#/components/schemas/Gateway__Error"}}
                            },
                        },
                        "404": {"description": "后端返回的未知类型（透传）。", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Gateway__Error"}}}},
                        "501": {"description": "后端未实现（透传）。", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Gateway__Error"}}}},
                        "503": {"description": "后端未就绪或未配置（网关直接返回）。", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Gateway__Error"}}}},
                    },
                }
            },
            **merged_paths,
        },
        "components": {"schemas": merged_schemas},
        "x-backend-revs": {r["service"]: r["rev"] for r in revs},
        "x-ws-subscriptions": ws_combined,
    }

    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "openapi.json").write_text(json.dumps(merged, ensure_ascii=False, indent=2) + "\n")
    matrix = {
        "gateway_rev": gateway_rev,
        "fetched_at": datetime.now(timezone.utc).isoformat(),
        "backends": revs,
    }
    (out_dir / "version-matrix.json").write_text(json.dumps(matrix, ensure_ascii=False, indent=2) + "\n")
    print(
        f"merge-openapi: OK gateway={gateway_rev[:12]} "
        + " ".join(f"{r['service']}={r['rev'][:12]}" for r in revs)
        + f" schemas={len(merged_schemas)} ws={len(ws_combined)} -> {out_dir}"
    )


if __name__ == "__main__":
    main()
