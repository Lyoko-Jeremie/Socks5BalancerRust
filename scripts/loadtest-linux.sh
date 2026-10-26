#!/usr/bin/env bash
# M4 / C10K 压测（Linux 专用；Windows 单机动态端口约 16K，见 scripts/m4-memory.ps1）
#
# 覆盖两个口径：
#   1) C10K 空闲：建立 10000 条空闲连接并保持 60s，采样 RSS / CPU / 句柄
#   2) 1K/s 建连：以 ~1000 次/秒的速率持续建连（30000 条、每条保持 10s）
#
# 用法：
#   ./scripts/loadtest-linux.sh                # 默认 bufferSize=2048 + C10K + 1K/s
#   BUFFER_SIZE=8192 ./scripts/loadtest-linux.sh
#   MODE=c10k ./scripts/loadtest-linux.sh      # 只跑 C10K
#   MODE=rate ./scripts/loadtest-linux.sh      # 只跑 1K/s
#
# 前置：
#   - 已 cargo build --release --bins --examples
#   - ulimit -n 至少 65535（脚本会检查并提示）
#   - 本机可用临时端口充足：net.ipv4.ip_local_port_range 建议 "10240 65535"
#
# 输出：target/m4-logs/linux-<mode>.txt（RSS/CPU 采样与结论）

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
LOGS="$ROOT/target/m4-logs"
mkdir -p "$LOGS"

MODE="${MODE:-both}"
BUFFER_SIZE="${BUFFER_SIZE:-2048}"
LISTEN_PORT="${LISTEN_PORT:-5700}"
CONNS="${CONNS:-10000}"
HOLD="${HOLD:-60}"
RATE_CONNS="${RATE_CONNS:-30000}"
RATE_HOLD="${RATE_HOLD:-10}"
RATE_INTERVAL_MS="${RATE_INTERVAL_MS:-1}"   # ~1000 次/秒

BIN="$ROOT/target/release/Socks5BalancerRust"
ECHO_BIN="$ROOT/target/release/examples/echo_upstream"
MOCK_BIN="$ROOT/target/release/examples/mock_socks5_upstream"
FLOOD_BIN="$ROOT/target/release/examples/flood"

for f in "$BIN" "$ECHO_BIN" "$MOCK_BIN" "$FLOOD_BIN"; do
    if [[ ! -x "$f" ]]; then
        echo "缺少 $f，请先：cargo build --release --bins --examples" >&2
        exit 1
    fi
done

NOFILE="$(ulimit -n)"
if (( NOFILE < 65535 )); then
    echo "警告：ulimit -n=$NOFILE < 65535；C10K 需要更高上限（ulimit -n 65535）" >&2
fi

PIDS=()
cleanup() {
    for pid in "${PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT

wait_port() {
    local port="$1" tries=100
    for ((i = 0; i < tries; i++)); do
        if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
            exec 3>&- 3<&-
            return 0
        fi
        sleep 0.1
    done
    return 1
}

rss_mb() {
    local pid="$1"
    awk '/VmRSS:/ {printf "%.2f", $2/1024}' "/proc/$pid/status" 2>/dev/null || echo "0"
}

cpu_ticks() {
    local pid="$1"
    awk '{print $14+$15}' "/proc/$pid/stat" 2>/dev/null || echo "0"
}

# ---------- 拓扑：echo(6100) + mock 上游(6200) + balancer(5700) ----------
start_topology() {
    "$ECHO_BIN" 127.0.0.1:6100 >"$LOGS/echo.out" 2>&1 &
    PIDS+=($!)
    "$MOCK_BIN" 127.0.0.1:6200 alice:secret >"$LOGS/mock.out" 2>&1 &
    PIDS+=($!)
    wait_port 6100 || { echo "echo 未就绪" >&2; exit 1; }
    wait_port 6200 || { echo "mock 上游未就绪" >&2; exit 1; }

    local cfg="$LOGS/linux-$BUFFER_SIZE.json"
    cat >"$cfg" <<EOF
{
  "listenHost": "127.0.0.1",
  "listenPort": $LISTEN_PORT,
  "stateServerPort": 0,
  "traditionTcpRelay": false,
  "bufferSize": $BUFFER_SIZE,
  "maxConnections": 40000,
  "firstPacketTimeout": 60000,
  "connectTimeout": 2000,
  "handshakeTimeout": 10000,
  "logLevel": "info",
  "upstream": [
    { "name": "mockAuth", "host": "127.0.0.1", "port": 6200, "authUser": "alice", "authPwd": "secret" }
  ],
  "healthCheck": {
    "tcp": { "period": 5000, "timeout": 2000, "startDelay": 1000 },
    "downBackoff": { "initial": 30000, "multiplier": 2.0, "max": 600000, "jitter": 0.2 },
    "probeJitter": 0.2
  }
}
EOF
    "$BIN" -c "$cfg" >"$LOGS/bal.out" 2>&1 &
    BAL_PID=$!
    PIDS+=($BAL_PID)
    wait_port "$LISTEN_PORT" || { echo "balancer 未就绪" >&2; exit 1; }
    sleep 2
    echo "拓扑就绪：balancer pid=$BAL_PID bufferSize=$BUFFER_SIZE 空闲RSS=$(rss_mb "$BAL_PID")MB"
}

# 采样：RSS 峰值 + CPU 秒数
sample_while() {
    local pid="$1" total_secs="$2" out="$3"
    local start_ticks
    start_ticks=$(cpu_ticks "$pid")
    local peak=0 step cpu_start
    cpu_start=$start_ticks
    for ((s = 0; s < total_secs * 2; s++)); do
        local rss
        rss=$(rss_mb "$pid")
        if awk -v a="$rss" -v b="$peak" 'BEGIN{exit !(a>b)}'; then
            peak="$rss"
        fi
        sleep 0.5
    done
    local end_ticks
    end_ticks=$(cpu_ticks "$pid")
    local hz
    hz=$(getconf CLK_TCK)
    echo "mode_out=$out peakRssMB=$peak cpuSecs=$(awk -v a="$cpu_start" -v b="$end_ticks" -v h="$hz" 'BEGIN{printf "%.2f",(b-a)/h}')" | tee -a "$out"
}

run_c10k() {
    local out="$LOGS/linux-c10k-$BUFFER_SIZE.txt"
    : >"$out"
    echo "== C10K 空闲：$CONNS 连接 hold=${HOLD}s bufferSize=$BUFFER_SIZE ==" | tee -a "$out"
    local idle
    idle=$(rss_mb "$BAL_PID")
    "$FLOOD_BIN" 127.0.0.1:"$LISTEN_PORT" "$CONNS" "$HOLD" 0 >"$LOGS/flood-c10k.out" 2>&1 &
    local flood_pid=$!
    PIDS+=($flood_pid)
    sample_while "$BAL_PID" "$HOLD" "$out"
    wait "$flood_pid" 2>/dev/null || true
    echo "idleRssMB=$idle" | tee -a "$out"
    echo "flood: $(tail -1 "$LOGS/flood-c10k.out")" | tee -a "$out"
    local peak
    peak=$(grep -o 'peakRssMB=[0-9.]*' "$out" | tail -1 | cut -d= -f2)
    echo "每连接增量 ≈ $(awk -v p="$peak" -v i="$idle" -v n="$CONNS" 'BEGIN{printf "%.0f", (p-i)*1024*1024/n}') B" | tee -a "$out"
}

run_rate() {
    local out="$LOGS/linux-rate-$BUFFER_SIZE.txt"
    : >"$out"
    echo "== 1K/s 建连：$RATE_CONNS 连接 hold=${RATE_HOLD}s interval=${RATE_INTERVAL_MS}ms ==" | tee -a "$out"
    local idle
    idle=$(rss_mb "$BAL_PID")
    "$FLOOD_BIN" 127.0.0.1:"$LISTEN_PORT" "$RATE_CONNS" "$RATE_HOLD" "$RATE_INTERVAL_MS" >"$LOGS/flood-rate.out" 2>&1 &
    local flood_pid=$!
    PIDS+=($flood_pid)
    sample_while "$BAL_PID" "$((RATE_CONNS * RATE_INTERVAL_MS / 1000 + RATE_HOLD))" "$out"
    wait "$flood_pid" 2>/dev/null || true
    echo "idleRssMB=$idle" | tee -a "$out"
    echo "flood: $(tail -1 "$LOGS/flood-rate.out")" | tee -a "$out"
}

start_topology
case "$MODE" in
    c10k) run_c10k ;;
    rate) run_rate ;;
    both) run_c10k; run_rate ;;
    *) echo "未知 MODE=$MODE（c10k / rate / both）" >&2; exit 1 ;;
esac
echo "完成，报告见 $LOGS/"
