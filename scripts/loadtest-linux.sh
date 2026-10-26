#!/usr/bin/env bash
# M4 / C10K 压测（Linux 专用；Windows 单机动态端口约 16K，见 scripts/m4-memory.ps1）
#
# 覆盖三个口径（各场景日志分开，互不覆盖）：
#   1) C10K 空闲（纯 TCP 透传内存口径）：建立 10000 条空闲连接并保持 60s，采样 RSS / CPU / 句柄
#   2) 1K/s 建连（纯 TCP 透传内存口径）：以 ~1000 次/秒的速率持续建连（30000 条、每条保持 10s）
#   3) 协议口径（真实 SOCKS5）：echo_upstream(6100) + mock_socks5_upstream(6200) +
#      balancer(5700，上游指向 mock)，以 1000 QPS 持续 60s 跑 examples/flood_socks5，
#      记录成功率与握手 p99；同一次压测内采样 balancer RSS/CPU
#
# 用法：
#   ./scripts/loadtest-linux.sh                # 默认 bufferSize=2048 + C10K + 1K/s + 协议口径
#   BUFFER_SIZE=8192 ./scripts/loadtest-linux.sh
#   MODE=c10k ./scripts/loadtest-linux.sh      # 只跑 C10K
#   MODE=rate ./scripts/loadtest-linux.sh      # 只跑 1K/s
#   MODE=proto ./scripts/loadtest-linux.sh     # 只跑真实 SOCKS5 协议口径
#
# 前置：
#   - 已 cargo build --release --bins --examples
#   - ulimit -n 至少 65535（脚本会检查并提示）
#   - 本机可用临时端口充足：net.ipv4.ip_local_port_range 建议 "10240 65535"
#
# 输出（均位于 target/m4-logs/，文件互不覆盖）：
#   linux-c10k-<bufferSize>.txt   C10K 空闲场景采样与结论
#   linux-rate-<bufferSize>.txt   1K/s 建连场景采样与结论
#   linux-proto-<bufferSize>.txt  真实 SOCKS5 协议口径采样、成功率与 p99

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
RATE="${RATE:-1000}"                        # 目标建连速率（条/秒）；flood 用绝对时间调度保证长期平均精确
PROTO_QPS="${PROTO_QPS:-1000}"              # 协议口径目标 QPS
PROTO_DURATION="${PROTO_DURATION:-60}"      # 协议口径持续秒数
PROTO_CONCURRENCY="${PROTO_CONCURRENCY:-64}" # 协议口径最大在途连接数

BIN="$ROOT/target/release/Socks5BalancerRust"
ECHO_BIN="$ROOT/target/release/examples/echo_upstream"
MOCK_BIN="$ROOT/target/release/examples/mock_socks5_upstream"
FLOOD_BIN="$ROOT/target/release/examples/flood"
FLOOD_SOCKS5_BIN="$ROOT/target/release/examples/flood_socks5"

for f in "$BIN" "$ECHO_BIN" "$MOCK_BIN" "$FLOOD_BIN" "$FLOOD_SOCKS5_BIN"; do
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
    echo "== 1K/s 建连：$RATE_CONNS 连接 hold=${RATE_HOLD}s rate=${RATE}/s ==" | tee -a "$out"
    local idle
    idle=$(rss_mb "$BAL_PID")
    # 第 4 个参数 interval=0、第 5 个参数为目标速率：flood 用绝对时间调度，
    # 长期平均速率 = RATE（旧版 sleep(1ms) 串行控速实测只有约 469/s，无法验收 1K/s）。
    "$FLOOD_BIN" 127.0.0.1:"$LISTEN_PORT" "$RATE_CONNS" "$RATE_HOLD" 0 "$RATE" >"$LOGS/flood-rate.out" 2>&1 &
    local flood_pid=$!
    PIDS+=($flood_pid)
    sample_while "$BAL_PID" "$((RATE_CONNS / RATE + RATE_HOLD))" "$out"
    wait "$flood_pid" 2>/dev/null || true
    echo "idleRssMB=$idle" | tee -a "$out"
    local line
    line="$(tail -1 "$LOGS/flood-rate.out" 2>/dev/null || true)"
    echo "flood: $line" | tee -a "$out"
    # 统一 key=value 便于 grep 汇总（缺字段显式 NA）
    local spawn_rate p99
    spawn_rate="$(printf '%s' "$line" | grep -o 'spawn rate=[0-9.]*' | tr ' ' '_' || true)"
    p99="$(printf '%s' "$line" | grep -o 'connect_p99_ms=[0-9.]*' || true)"
    echo "summary ${spawn_rate:-spawn_rate=NA} ${p99:-connect_p99_ms=NA}" | tee -a "$out"
    if [[ -z "$p99" ]]; then
        echo "flood 未输出建连延迟汇总行（见 $LOGS/flood-rate.out）" >&2
        return 1
    fi
}

run_proto() {
    # 协议口径：真实 SOCKS5 建连 → 握手 → CONNECT(echo:6100) → ping 回显 → RST 关闭。
    # 上游是 mock_socks5_upstream(6200)，因此 balancer 的「入站握手 + 上游建连/认证 + 双向转发」
    # 全链路都被覆盖，不是纯 TCP 空闲连接口径。
    local out="$LOGS/linux-proto-$BUFFER_SIZE.txt"
    : >"$out"
    echo "== 协议口径（真实 SOCKS5）：目标 ${PROTO_QPS} QPS，持续 ${PROTO_DURATION}s，并发=${PROTO_CONCURRENCY} ==" | tee -a "$out"
    local idle
    idle=$(rss_mb "$BAL_PID")
    "$FLOOD_SOCKS5_BIN" 127.0.0.1:"$LISTEN_PORT" 127.0.0.1:6100 \
        "$PROTO_CONCURRENCY" "$PROTO_QPS" "$PROTO_DURATION" >"$LOGS/flood-proto.out" 2>&1 &
    local flood_pid=$!
    PIDS+=($flood_pid)
    sample_while "$BAL_PID" "$PROTO_DURATION" "$out"
    wait "$flood_pid" 2>/dev/null || true
    echo "idleRssMB=$idle" | tee -a "$out"
    local line success_rate p99 actual_qps
    line="$(tail -1 "$LOGS/flood-proto.out" 2>/dev/null || true)"
    echo "flood_socks5: $line" | tee -a "$out"
    success_rate="$(printf '%s' "$line" | grep -o 'success_rate=[0-9.]*%' || true)"
    p99="$(printf '%s' "$line" | grep -o 'handshake_p99_ms=[0-9.]*' || true)"
    actual_qps="$(printf '%s' "$line" | grep -o 'actual_qps=[0-9.]*' || true)"
    # 统一成 key=value 便于 grep 汇总；缺字段时显式标 NA，避免误读。
    echo "summary ${success_rate:-success_rate=NA} ${p99:-handshake_p99_ms=NA} ${actual_qps:-actual_qps=NA}" | tee -a "$out"
    if [[ -z "$success_rate" ]]; then
        echo "flood_socks5 未输出汇总行（见 $LOGS/flood-proto.out）" >&2
        return 1
    fi
    if [[ "$success_rate" != "success_rate=100.00%" ]]; then
        echo "警告：协议口径成功率不足 100%：$success_rate（见 $LOGS/flood-proto.out）" >&2
    fi
}

start_topology
case "$MODE" in
    c10k) run_c10k ;;
    rate) run_rate ;;
    proto) run_proto ;;
    both) run_c10k; run_rate; run_proto ;;
    *) echo "未知 MODE=$MODE（c10k / rate / proto / both）" >&2; exit 1 ;;
esac
echo "完成，报告见 $LOGS/"
