#!/usr/bin/env bash
# Linux CI：格式 / clippy / 单测 / release 构建 / 数据面 + 控制面冒烟
#
# 用法： ./scripts/ci-linux.sh
# 前置： Rust 稳定版工具链（edition 2024 需要 1.85+）
#
# 拓扑（端口固定，失败即 exit 1）：
#   mock_socks5_upstream  127.0.0.1:5702  （balancer 的上游）
#   echo_upstream         127.0.0.1:5704  （CONNECT 目标）
#   balancer              127.0.0.1:5700  （SOCKS5 入站）
#   balancer 控制面       127.0.0.1:5701
#
# 冒烟覆盖：控制面 /healthz、/api/v1/status、/api/v1/upstreams，以及真实数据面
# socks5_client 经 balancer 访问 echo 并断言回显一致（修复此前「上游是 dead 5702、
# echo 5704 从未被使用」的数据面盲区）。
#
# 说明：完整协议冒烟（SOCKS5/SOCKS4/HTTP/认证/控制面写操作）见 PowerShell 脚本；
#       这里做 Linux 上可自动化的最小冒烟。

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

step "冒烟：启动 balancer + mock 上游 + echo，检查控制面与数据面"
BIN="$ROOT/target/release/Socks5BalancerRust"
ECHO_BIN="$ROOT/target/release/examples/echo_upstream"
MOCK_BIN="$ROOT/target/release/examples/mock_socks5_upstream"
CLIENT_BIN="$ROOT/target/release/examples/socks5_client"
for f in "$BIN" "$ECHO_BIN" "$MOCK_BIN" "$CLIENT_BIN"; do
    if [[ ! -x "$f" ]]; then
        echo "缺少 $f，请先：cargo build --release --bins --examples" >&2
        exit 1
    fi
done

# CI 用最小配置（监听 5700/控制面 5701；上游 mock 5702，目标 echo 5704）
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
  "upstream": [ { "name": "mock", "host": "127.0.0.1", "port": 5702 } ],
  "healthCheck": {
    "tcp": { "period": 1000, "timeout": 500, "startDelay": 100 },
    "downBackoff": { "initial": 1000, "multiplier": 2.0, "max": 5000, "jitter": 0.1 }
  }
}
EOF

"$MOCK_BIN" 127.0.0.1:5702 >"$LOGS/mock.out" 2>&1 &
MOCK_PID=$!
"$ECHO_BIN" 127.0.0.1:5704 >"$LOGS/echo.out" 2>&1 &
ECHO_PID=$!
"$BIN" -c "$CFG" >"$LOGS/bal.out" 2>&1 &
BAL_PID=$!
cleanup() {
    kill "$BAL_PID" 2>/dev/null || true
    kill "$ECHO_PID" 2>/dev/null || true
    kill "$MOCK_PID" 2>/dev/null || true
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

wait_port 5702 || { echo "mock 上游未监听 5702" >&2; cat "$LOGS/mock.out" >&2; exit 1; }
wait_port 5704 || { echo "echo 目标未监听 5704" >&2; cat "$LOGS/echo.out" >&2; exit 1; }
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

step "数据面冒烟：socks5_client 经 balancer(5700) 访问 echo(5704)"
PAYLOAD="ci-data-plane-ok"
DATA_RESP=""
DATA_OK=0
# 上游健康状态可能还没就绪（startDelay 100ms + period 1s），最多重试 10s。
for _ in $(seq 1 40); do
    DATA_RESP="$("$CLIENT_BIN" 127.0.0.1:5700 127.0.0.1:5704 "$PAYLOAD" 2>&1 || true)"
    if [[ "$DATA_RESP" == "OK $PAYLOAD" ]]; then
        DATA_OK=1
        break
    fi
    sleep 0.25
done
if (( DATA_OK != 1 )); then
    echo "数据面冒烟失败：经 balancer 访问 echo 未得到一致回显" >&2
    echo "socks5_client 最后输出：${DATA_RESP:-<无输出>}" >&2
    echo "--- bal.out ---" >&2
    cat "$LOGS/bal.out" >&2
    echo "--- mock.out ---" >&2
    cat "$LOGS/mock.out" >&2
    exit 1
fi
echo "数据面回显一致：$DATA_RESP"

echo
echo "CI 全部通过"
