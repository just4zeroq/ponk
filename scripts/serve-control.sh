#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# PONK 控制面服务端启动脚本
#
# 对应：docs/09（控制面）· docs/13 §2.4（与 serve 同进程）· docs/13 §5.1（JWT）
#
# **配置全部来自 `control.toml`**（密钥 / 端口 / 存储 / TTL / 首启引导），**不读环境变量**：
#   优先级：CLI 参数 > control.toml > 内置默认          （main.rs::parse_opts）
#
# 用法：
#   cp control.example.toml control.toml && $EDITOR control.toml      # 首次
#   scripts/serve-control.sh                # 前台启动（systemd / 容器直接托管）
#   scripts/serve-control.sh --check        # 只打印从配置文件解析出的值，不启动
#   PONK_CONFIG=/etc/ponk/control.toml scripts/serve-control.sh        # 指定其它配置文件
#
# 前端（web/dist）单独托管 —— http.rs 未实现 ServeDir（docs/13 §2.4 阶段项）。
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# 配置查找顺序与 main.rs::load_file_config 保持一致：
#   PONK_CONFIG > ./control.toml > <repo>/crates/control/control.toml
if [[ -n "${PONK_CONFIG:-}" ]]; then
    CONFIG="$PONK_CONFIG"
elif [[ -f "$REPO/control.toml" ]]; then
    CONFIG="$REPO/control.toml"
else
    CONFIG="$REPO/crates/control/control.toml"
fi

# ── 1. 可执行：release → debug，兼容 Windows 的 .exe（Git Bash 下产物带后缀）────
find_bin() {
    local cand
    for cand in "$1" "$1.exe"; do
        if [[ -x "$cand" || -f "$cand" ]]; then echo "$cand"; return 0; fi
    done
    return 1
}
BIN="${PONK_CONTROL_BIN:-}"
if [[ -z "$BIN" ]]; then
    BIN="$(find_bin "$REPO/target/release/ponk-control" || true)"
    [[ -z "$BIN" ]] && BIN="$(find_bin "$REPO/target/debug/ponk-control" || true)"
fi
if [[ -z "$BIN" ]]; then
    echo "错误：找不到可执行文件。请先构建：" >&2
    echo "    cargo build --release -p ponk-control      # 生产" >&2
    echo "    cargo build -p ponk-control                # 调试" >&2
    exit 1
fi

# ── 2. 配置文件（必填项都在里面）──────────────────────────────────────────────
if [[ ! -f "$CONFIG" ]]; then
    echo "错误：找不到配置文件 $CONFIG" >&2
    echo "      cp $REPO/crates/control/control.example.toml $CONFIG   # 然后编辑该文件" >&2
    exit 1
fi

# 从 toml 粗读 —— 仅用于**前置提示与 --check 展示**；严格校验由二进制完成
toml_get() { # toml_get <key> → 引号包裹的同名键的首个值
    sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p" "$CONFIG" | head -1
}
HTTP_ADDR="$(toml_get addr)"
HTTP_ENABLED="$(sed -n 's/^[[:space:]]*enabled[[:space:]]*=[[:space:]]*\(.*\)/\1/p' "$CONFIG" | head -1)"
JWT_SECRET_SET="$(grep -cE '^[[:space:]]*jwt_secret[[:space:]]*=' "$CONFIG" || true)"
HTTP_ON=0
if [[ -n "$HTTP_ADDR" && "$HTTP_ENABLED" != "false" ]]; then HTTP_ON=1; fi

# 开了 HTTP 却没配密钥 → 直接拒绝启动（不自动生成随机密钥，否则重启后令牌全失效）
if [[ "$HTTP_ON" == "1" && "$JWT_SECRET_SET" == "0" ]]; then
    echo "错误：配置文件启用了 HTTP（addr = \"$HTTP_ADDR\"）但没有 [auth].jwt_secret。" >&2
    echo "      在 control.toml 的 [auth] 段设置： jwt_secret = \"\$(openssl rand -hex 32)\"" >&2
    echo "      说明：不自动生成随机密钥 —— 重启会使所有令牌失效（docs/13 §5.1）" >&2
    exit 1
fi

if [[ "$HTTP_ON" == "1" ]]; then
    WEB="$REPO/web/dist"
    if [[ -f "$WEB/index.html" ]]; then
        echo "提示：前端产物就绪（$WEB）——由反代/静态服务器单独托管："
        echo "      nginx: proxy_pass http://$HTTP_ADDR  +  root $WEB"
        echo "      本地预览: (cd web && npx vite preview --port 5173)"
    else
        echo "提示：未找到 $WEB/index.html —— 构建： (cd web && npm run build)"
    fi
fi

# ── 3. 配置自检（--check 只打印不启动）──────────────────────────────────────
MODE=serve
if [[ "${1:-}" == "--check" ]]; then MODE=check; shift; fi

STORE="$(toml_get path)"
TTL="$(toml_get jwt_ttl)"
INST="$(toml_get instance)"
BOOT="$(toml_get bootstrap)"
echo "──────────────────────────────────────────────────────"
echo " 配置   : $CONFIG"
echo " 可执行 : $BIN"
echo " 存储   : ${STORE:-./control-store（内置默认）}"
echo " 实例   : ${INST:-cp-1（内置默认）}"
if [[ "$HTTP_ON" == "1" ]]; then
    echo " HTTP   : $HTTP_ADDR"
else
    echo " HTTP   : <关闭（无 addr 或 enabled=false）>"
fi
echo " TTL    : ${TTL:-8h（内置默认）}    引导   : ${BOOT:-default}"
if [[ "$JWT_SECRET_SET" == "0" ]]; then
    echo " 密钥   : 未配置"
else
    echo " 密钥   : 已配置（存在配置文件内，不回显）"
fi
if grep -qE '^[[:space:]]*admin_password[[:space:]]*=' "$CONFIG"; then
    echo " 管理员 : admin（口令来自配置文件，不回显）"
else
    echo " 管理员 : admin（内置默认 admin123，生产请在 [auth].admin_password 覆盖）"
fi
echo " 日志   : ${RUST_LOG:-ponk_control=info}"
echo "──────────────────────────────────────────────────────"

if [[ "$MODE" == "check" ]]; then
    echo "（--check：仅自检，未启动）"
    exit 0
fi

# ── 4. 前台启动（交由 systemd / 容器托管；信号直达进程）────────────────────
ARGS=(serve --config "$CONFIG")
echo "启动： $BIN ${ARGS[*]}"
exec "$BIN" "${ARGS[@]}"
