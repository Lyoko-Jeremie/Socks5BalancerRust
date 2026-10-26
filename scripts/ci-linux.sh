#!/usr/bin/env bash
# Linux CI：格式 / clippy / 单测 / release 构建 / 健康检查冒烟
#
# 用法： ./scripts/ci-linux.sh
# 前置： Rust 稳定版工具链（edition 2024 需要 1.85+）
#
# 说明：完整协议冒烟（SOCKS5/SOCKS4/HTTP/控制面）见 PowerShell 脚本；
#       这里做 Linux 上可自动化的最小冒烟：启动 balancer + 访问 /healthz + /api/v1/status。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
LOGS="$ROOT/target/ci-logs"
mkdir -p "$LOGS"

step() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

step "cargo fmt --check"
cargo fmt --all -- --check

step "cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

step "cargo test"
cargo test

step "cargo build --release --bins --examples"
cargo build --release --bins --examples

step "冒烟：启动 balancer 并检查控制面"
BIN="$ROOT/target/release/Socks5BalancerRust"
ECHO_BIN="$ROOT/target/release/examples/echo_upstream"

# CI 用最小配置（禁用外部依赖，端口随机选 5700/5701）
CFG="$LOGS/ci-config.json"
cat >"$CFG" <<'EOF'
{
  "listenHost": "127.0.0.1",
  "listenPort": 5700,
  "stateServerHost": "127.0.0.1",
  "stateServerPort": 5701,
  "bufferSize": 2048,
  "maxConnections": 1000,
  "logLevel": "info",
  "upstream": [ { "name": "dead", "host": "127.0.0.1", "port": 5702 } ],
  "healthCheck": {
    "tcp": { "period": 1000, "timeout": 500, "startDelay": 100 },
    "downBackoff": { "initial": 1000, "multiplier": 2.0, "max": 5000, "jitter": 0.1 }
  }
}
EOF

"$ECHO_BIN" 127.0.0.1:5704 >"$LOGS/echo.out" 2>&1 &
ECHO_PID=$!
"$BIN" -c "$CFG" >"$LOGS/bal.out" 2>&1 &
BAL_PID=$!
cleanup() {
    kill "$BAL_PID" 2>/dev/null || true
    kill "$ECHO_PID" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

wait_port() {
    local port="$1"
    for _ in $(seq 1 100); do
        if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
            exec 3>&- 3<&-
            return 0
        fi
        sleep 0.1
    done
    return 1
}

wait_port 5700 || { echo "balancer 未监听 5700" >&2; cat "$LOGS/bal.out" >&2; exit 1; }
wait_port 5701 || { echo "控制面未监听 5701" >&2; cat "$LOGS/bal.out" >&2; exit 1; }

if ! command -v curl >/dev/null 2>&1; then
    echo "缺少 curl，跳过 HTTP 断言" >&2
else
    if ! curl -fsS --max-time 5 http://127.0.0.1:5701/healthz | grep -qx ok; then
        echo "/healthz 未返回 ok" >&2
        cat "$LOGS/bal.out" >&2
        exit 1
    fi
    status_json="$(curl -fsS --max-time 5 http://127.0.0.1:5701/api/v1/status)"
    if ! printf '%s' "$status_json" | grep -q '"schema":"v1"'; then
        echo "/api/v1/status 缺少 schema=v1：$status_json" >&2
        exit 1
    fi
    if ! printf '%s' "$status_json" | grep -q '"stats"'; then
        echo "/api/v1/status 缺少 stats：$status_json" >&2
        exit 1
    fi
    upstreams_json="$(curl -fsS --max-time 5 http://127.0.0.1:5701/api/v1/upstreams)"
    if printf '%s' "$upstreams_json" | grep -q '"authUser"'; then
        echo "/api/v1/upstreams 泄露认证字段" >&2
        exit 1
    fi
fi

echo
echo "CI 全部通过"
