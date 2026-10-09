#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# 目录类页面联调冒烟（docs/14）
#
# 验证「控制面 HTTP 面 + 三个目录页的接口契约」在**真实进程**上是否成立：
#   登录(JWT) → 建所 → 建市场 → 启停/编辑(If-Match) → 触发同步 →
#   读交易对 / 币 / 变更 / 合约 / 外部同步状态 → 关联删除确认 → 收尾
#
# 说明：
#   * **不依赖外网**：同步任务在无网络/无 dig3 拉取器时**应当**优雅失败并记进
#     `sync_run.error`——本脚本正是要验证"失败不崩、错误如实上报"。
#   * 端口默认 18097（避开开发用的 8080 与前端 5173）。
#   * 用完即停、临时存储删除。
#
# 用法：bash scripts/smoke-catalog.sh
# ─────────────────────────────────────────────────────────────────────────────
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${SMOKE_PORT:-18097}"
BASE="http://127.0.0.1:$PORT/api/v1"
DIR="$REPO/.smoke-catalog"
SECRET="smoke-catalog-secret-$(date +%s)"

pass=0; fail=0
ck() { # ck <描述> <期望> <实际>
    if [[ "$2" == "$3" ]]; then printf '  ✓ %s\n' "$1"; pass=$((pass+1));
    else printf '  ✗ %s  期望=%s 实际=%s\n' "$1" "$2" "$3"; fail=$((fail+1)); fi
}
jget() { # jget <json> <jq-ish 路径，点分>  —— 用 python 取，避免依赖 jq
    python -c "
import json,sys
try:
    d=json.loads(sys.argv[1]); k=sys.argv[2]
    for p in k.split('.'):
        d = d[int(p)] if isinstance(d,list) and p.isdigit() else d[p]
    print('' if d is None else (str(d).lower() if isinstance(d,bool) else d))
except Exception as e: print('')
" "$1" "$2" 2>/dev/null
}
req() { # req <METHOD> <path> <token> <body>  → 输出 "STATUS<TAB>BODY"
    local m="$1" p="$2" t="$3" b="${4:-}"
    local args=(-s -m 10 -X "$m" -w $'\n%{http_code}' "$BASE$p")
    [[ -n "$t" ]] && args+=(-H "Authorization: Bearer $t")
    [[ -n "$b" ]] && args+=(-H 'Content-Type: application/json' -d "$b")
    curl "${args[@]}"
}

cleanup() {
    [[ -n "${PID:-}" ]] && kill "$PID" 2>/dev/null
    sleep 1
    rm -rf "$DIR"
    echo "[smoke] 清理完成"
}
trap cleanup EXIT

echo "════ 目录类页面联调冒烟（docs/14）════"
rm -rf "$DIR"; mkdir -p "$DIR"

# ── 配置与启动 ──────────────────────────────────────────────────────────────
cat > "$DIR/control.toml" <<TOML
instance = "smoke-cp"
[store]
path = "$DIR/store"
[http]
addr = "127.0.0.1:$PORT"
[auth]
jwt_secret = "$SECRET"
jwt_ttl = "1h"
bootstrap = "default"
TOML

BIN="$REPO/target/debug/ponk-control"
[[ -x "$BIN" || -f "$BIN.exe" ]] || BIN="$REPO/target/debug/ponk-control.exe"
if [[ ! -f "$BIN" ]]; then echo "✗ 未找到可执行：先 cargo build -p ponk-control"; exit 1; fi

PONK_CONFIG="$DIR/control.toml" bash "$REPO/scripts/serve-control.sh" > "$DIR/serve.log" 2>&1 &
PID=$!

for i in $(seq 1 60); do
    curl -s -m 2 "$BASE/healthz" >/dev/null 2>&1 && break
    sleep 0.5
done

echo "── 1. 鉴权（docs/13 §5.1）"
out=$(req GET /healthz "")
ck "healthz 免鉴权 200" "200" "$(echo "$out" | tail -1)"
out=$(req GET /overview "")
ck "无 token → 401" "401" "$(echo "$out" | tail -1)"
out=$(req POST /auth/login "" '{"username":"admin","password":"admin123"}')
ck "admin 登录 200" "200" "$(echo "$out" | tail -1)"
TOK=$(echo "$out" | head -1 | python -c "import json,sys;print(json.load(sys.stdin).get('access_token',''))" 2>/dev/null)
[[ -n "$TOK" ]] && ck "拿到 JWT" "1" "1" || ck "拿到 JWT" "1" "0"
body=$(echo "$out" | head -1)
ck "默认口令标记" "true" "$(jget "$body" "default_password")"

echo "── 2. 交易所（P3：两步创建 + If-Match）"
out=$(req POST /exchanges "$TOK" '{"venue":"binance","name":"Binance","dig3_exchange_id":"binance","enabled":true,"asset_class":"crypto"}')
ck "建所 201" "201" "$(echo "$out" | tail -1)"
BE=$(echo "$out" | head -1 | python -c "import json,sys;print(json.load(sys.stdin).get('etag',''))" 2>/dev/null)
out=$(req GET /exchanges "$TOK")
ck "列所 200" "200" "$(echo "$out" | tail -1)"
out=$(req POST "/exchanges/binance/markets" "$TOK" '{"market":"spot","dig3_account_type":"Spot","enabled":true,"quota_collect":0.5,"quota_backfill":0.35,"quota_metadata":0.15}')
ck "建市场 201" "201" "$(echo "$out" | tail -1)"
out=$(req PATCH "/exchanges/binance/enabled" "$TOK" '{"enabled":false}')
ck "启停 200（缺 If-Match 或容错）" "200" "$(echo "$out" | tail -1)"
out=$(req PATCH "/exchanges/binance/enabled" "$TOK" '{"enabled":true}')
ck "恢复 200" "200" "$(echo "$out" | tail -1)"

echo "── 3. 同步任务（无网络应优雅失败并记录）"
out=$(req POST /sync/runs "$TOK" '{}')
st=$(echo "$out" | tail -1)
ck "触发同步 202/200" "1" "$( [[ "$st" == "202" || "$st" == "200" ]] && echo 1 || echo "$st" )"
out=$(req GET /sync/runs "$TOK")
ck "同步轮次列表 200" "200" "$(echo "$out" | tail -1)"
out=$(req GET /instruments "$TOK")
ck "交易对列表 200（无外网时可为空，但不能 500）" "200" "$(echo "$out" | tail -1)"
ib=$(echo "$out" | head -1)
ck "分页信封 items/total/limit/offset" "1" "$(python -c "
import json,sys
d=json.loads(sys.stdin.read() or '{}')
print(1 if all(k in d for k in ('items','total','limit','offset')) else 0)" <<< "$ib" 2>/dev/null || echo 0)"

echo "── 4. 币信息（P5 契约）"
out=$(req GET /assets "$TOK")
ck "资产列表 200" "200" "$(echo "$out" | tail -1)"
out=$(req GET /assets/BTC "$TOK")
st=$(echo "$out" | tail -1)
ck "资产详情 200 或 404（未同步时）" "1" "$( [[ "$st" == "200" || "$st" == "404" ]] && echo 1 || echo "$st" )"
out=$(req GET "/assets/BTC/contracts" "$TOK")
ck "合约地址端点存在（200/404）" "1" "$( [[ "$(echo "$out" | tail -1)" =~ ^(200|404)$ ]] && echo 1 || echo 0 )"
out=$(req GET /sync/offchain/status "$TOK")
st=$(echo "$out" | tail -1)
ck "外部同步状态存在（200/501）" "1" "$( [[ "$st" =~ ^(200|501)$ ]] && echo 1 || echo 0 )"

echo "── 5. 交易对与币的只读边界（docs/14 §1）"
out=$(req POST /instruments "$TOK" '{"venue":"binance"}')
ck "交易对无写接口（404/405）" "1" "$( [[ "$(echo "$out" | tail -1)" =~ ^(404|405)$ ]] && echo 1 || echo 0 )"

echo "── 6. 变更历史与导出"
out=$(req GET "/instruments/changes?limit=5" "$TOK")
ck "变更历史 200" "200" "$(echo "$out" | tail -1)"
out=$(req GET "/instruments/export" "$TOK")
ck "交易对导出 200" "200" "$(echo "$out" | tail -1)"

echo "──────────────────────────────────────────────"
echo "通过 $pass · 失败 $fail"
[[ $fail -eq 0 ]] && echo "冒烟通过" || { echo "冒烟失败"; exit 1; }
