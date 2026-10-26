#!/usr/bin/env bash
# 24h 长稳（soak）脚本：持续真实 SOCKS5 建连/断开，周期采样 RSS / fd / 控制面统计（Linux 专用）。
#
# 目的：回答「长时间运行会不会内存/fd 缓慢泄漏」——用真实协议流量（不是纯 TCP 空闲连接）
#       持续打 balancer，每 60s 记录一次进程 RSS、fd 数与 /api/v1/status 的 active/accepted，
#       结束时对 RSS / fd 做线性回归，打印每秒与每小时增长斜率。
#
# 用法：
#   ./scripts/soak-linux.sh [时长]          # 时长默认 24h；支持 24h / 90m / 3600s / 纯秒数
#   SOAK_QPS=1000 SOAK_CONCURRENCY=64 ./scripts/soak-linux.sh 1h
#   SAMPLE_INTERVAL=60 FLOOD_CHUNK=3600 ./scripts/soak-linux.sh 24h
#
# 前置：
#   - **Linux**：依赖 /proc/<pid>/status 与 /proc/<pid>/fd；/proc 不可用时直接报错退出（Windows 用 scripts/m4-memory.ps1）
#   - 已 cargo build --release --bins --examples
#   - ulimit -n 足够（建议 65535；脚本会检查并提示）
#   - 端口 5700/5701/5702/6100/6200 空闲（与 scripts/loadtest-linux.sh 不要同时跑）
#
# 拓扑：
#   echo_upstream(127.0.0.1:6100)          CONNECT 目标
#   mock_socks5_upstream(127.0.0.1:6200)   balancer 的上游（alice:secret 认证）
#   balancer(127.0.0.1:5700)               被测进程
#   balancer 控制面(127.0.0.1:5701)        采样 /api/v1/status
# 压测流量：flood_socks5 按目标 QPS 建连 → SOCKS5 握手 → CONNECT echo → ping 回显 → RST 关闭。
#
# 输出（全部在 target/soak/）：
#   soak-<时间戳>.csv    采样：timestamp,elapsed_s,rss_mb,fd_count,active,accepted
#   flood-round*.out     每轮 flood_socks5 的完整输出（含末尾一行汇总）
#   summary.txt          每轮汇总 + 最终 RSS/fd 增长斜率摘要
#   bal.out / cfg.json   balancer 日志与生效配置
#
# 注意：
#   - echo/mock 每个连接都会打印日志，24h 会产生巨量日志并干扰采样，默认丢弃到 /dev/null；
#     需要排障时用 SOAK_CHILD_LOG=target/soak/child.log 打开。
#   - balancer 日志级别用 warn，避免 24h 写盘拖累性能。
#   - curl 缺失时 active/accepted 记为 NA，不影响 RSS/fd 采样。
#   - 判定建议：RSS 斜率应接近 0（允许分配器缓存的小幅波动）；fd 不应随时间单调增长。
#   - 本脚本面向专用压测机，需要真实跑满时长；CI 只做 bash -n 语法检查。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SOAK_DIR="$ROOT/target/soak"
mkdir -p "$SOAK_DIR"

# ---------- 参数与配置 ----------
# 时长解析：支持 24h / 90m / 3600s / 纯秒数（只接受正整数，非法输入返回空由调用方报错）。
parse_duration_secs() {
    local raw="$1" num mult
    case "$raw" in
        *s) num="${raw%s}"; mult=1 ;;
        *m) num="${raw%m}"; mult=60 ;;
        *h) num="${raw%h}"; mult=3600 ;;
        *) num="$raw"; mult=1 ;;
    esac
    if ! [[ "$num" =~ ^[0-9]+$ ]]; then
        return 0
    fi
    echo $((num * mult))
}

DURATION_RAW="${1:-24h}"
DURATION_SECS="$(parse_duration_secs "$DURATION_RAW")"
if ! [[ "$DURATION_SECS" =~ ^[1-9][0-9]*$ ]]; then
    echo "时长参数非法：$DURATION_RAW（示例：24h / 90m / 3600s / 86400）" >&2
    exit 1
fi

SOAK_QPS="${SOAK_QPS:-1000}"
SOAK_CONCURRENCY="${SOAK_CONCURRENCY:-64}"
SAMPLE_INTERVAL="${SAMPLE_INTERVAL:-60}"
FLOOD_CHUNK="${FLOOD_CHUNK:-3600}"
LISTEN_PORT="${LISTEN_PORT:-5700}"
STATE_PORT="${STATE_PORT:-5701}"
TARGET_PORT="${TARGET_PORT:-6100}"
UPSTREAM_PORT="${UPSTREAM_PORT:-6200}"
BUFFER_SIZE="${BUFFER_SIZE:-2048}"

# ---------- 平台前置检查 ----------
if [[ ! -r /proc/self/status || ! -d /proc/self/fd ]]; then
    echo "本脚本需要 Linux（/proc 不可用）；Windows 请用 scripts/m4-memory.ps1" >&2
    exit 1
fi

# ---------- 二进制检查 ----------
BIN="$ROOT/target/release/Socks5BalancerRust"
ECHO_BIN="$ROOT/target/release/examples/echo_upstream"
MOCK_BIN="$ROOT/target/release/examples/mock_socks5_upstream"
FLOOD_SOCKS5_BIN="$ROOT/target/release/examples/flood_socks5"
for f in "$BIN" "$ECHO_BIN" "$MOCK_BIN" "$FLOOD_SOCKS5_BIN"; do
    if [[ ! -x "$f" ]]; then
        echo "缺少 $f，请先：cargo build --release --bins --examples" >&2
        exit 1
    fi
done

NOFILE="$(ulimit -n)"
if [[ "$NOFILE" =~ ^[0-9]+$ ]] && ((NOFILE < 65535)); then
    echo "警告：ulimit -n=$NOFILE < 65535；长时间高 QPS 建连建议调高（ulimit -n 65535）" >&2
fi

# ---------- 工具函数 ----------
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
    local v
    v="$(awk '/VmRSS:/ {printf "%.2f", $2/1024}' "/proc/$1/status" 2>/dev/null || true)"
    echo "${v:-0}"
}

fd_count() {
    local v
    v="$(ls "/proc/$1/fd" 2>/dev/null | wc -l | tr -d ' ' || true)"
    echo "${v:-0}"
}

# 线性回归斜率（最小二乘）；列号由参数传入（2=elapsed_s）。
slope_of() {
    local col="$1" label="$2"
    awk -F, -v col="$col" -v label="$label" '
        NR == 1 { next }
        $(col) ~ /^[0-9]+([.][0-9]+)?$/ {
            n++
            x = $2 + 0
            y = $(col) + 0
            sx += x
            sy += y
            sxx += x * x
            sxy += x * y
            if (n == 1) { first = y; first_t = x }
            last = y
            last_t = x
        }
        END {
            if (n < 2) {
                printf "%s: 有效样本不足（%d），无法计算斜率\n", label, n
                exit 0
            }
            den = n * sxx - sx * sx
            slope = (den == 0) ? 0 : (n * sxy - sx * sy) / den
            printf "%s: 样本=%d 首值=%.2f@%ds 末值=%.2f@%ds 斜率=%.6f/s (≈%.3f/h) 总增量=%.2f\n", \
                label, n, first, first_t, last, last_t, slope, slope * 3600, last - first
        }
    ' "$CSV"
}

# 最终摘要：RSS / fd 斜率 + 末尾采样（中断时也会由 EXIT trap 调用一次）。
summarize() {
    echo
    echo "== 长稳采样摘要（$CSV） =="
    if [[ ! -s "$CSV" ]]; then
        echo "无采样数据"
        return 0
    fi
    local rows
    rows=$(($(wc -l <"$CSV") - 1))
    echo "采样行数=$rows 覆盖时长=$(( $(date +%s) - START_TS ))s"
    slope_of 3 "RSS(MB)"
    slope_of 4 "fd"
    echo "最近 5 条采样："
    tail -n 5 "$CSV" | sed 's/^/  /'
    echo "判定建议：RSS 斜率接近 0（允许分配器缓存小幅波动）；fd 不应随时间单调增长。"
    echo "各轮 flood_socks5 汇总见 $SUMMARY_FILE"
}

# ---------- 进程与清理 ----------
PIDS=()
FLOOD_PID=""
SUMMARY_DONE=0
SUMMARY_FILE="$SOAK_DIR/summary.txt"
START_TS=0
CSV=""

cleanup() {
    local code=$?
    if [[ -n "${FLOOD_PID:-}" ]]; then
        kill "$FLOOD_PID" 2>/dev/null || true
        wait "$FLOOD_PID" 2>/dev/null || true
    fi
    for pid in "${PIDS[@]:-}"; do
        [[ -n "$pid" ]] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    if ((SUMMARY_DONE == 0)); then
        SUMMARY_DONE=1
        summarize || true
    fi
    exit "$code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ---------- 拓扑 ----------
CFG="$SOAK_DIR/cfg.json"
cat >"$CFG" <<EOF
{
  "listenHost": "127.0.0.1",
  "listenPort": $LISTEN_PORT,
  "stateServerHost": "127.0.0.1",
  "stateServerPort": $STATE_PORT,
  "traditionTcpRelay": false,
  "bufferSize": $BUFFER_SIZE,
  "maxConnections": 40000,
  "firstPacketTimeout": 60000,
  "connectTimeout": 2000,
  "handshakeTimeout": 10000,
  "logLevel": "warn",
  "upstream": [
    { "name": "mockAuth", "host": "127.0.0.1", "port": $UPSTREAM_PORT, "authUser": "alice", "authPwd": "secret" }
  ],
  "healthCheck": {
    "tcp": { "period": 5000, "timeout": 2000, "startDelay": 1000 },
    "downBackoff": { "initial": 30000, "multiplier": 2.0, "max": 600000, "jitter": 0.2 },
    "probeJitter": 0.2
  }
}
EOF

CHILD_LOG="${SOAK_CHILD_LOG:-/dev/null}"
"$ECHO_BIN" 127.0.0.1:"$TARGET_PORT" >"$CHILD_LOG" 2>&1 &
PIDS+=("$!")
"$MOCK_BIN" 127.0.0.1:"$UPSTREAM_PORT" alice:secret >"$CHILD_LOG" 2>&1 &
PIDS+=("$!")
"$BIN" -c "$CFG" >"$SOAK_DIR/bal.out" 2>&1 &
BAL_PID=$!
PIDS+=("$BAL_PID")

wait_port "$TARGET_PORT" || { echo "echo 未就绪" >&2; exit 1; }
wait_port "$UPSTREAM_PORT" || { echo "mock 上游未就绪" >&2; exit 1; }
wait_port "$LISTEN_PORT" || { echo "balancer 未就绪" >&2; cat "$SOAK_DIR/bal.out" >&2; exit 1; }
wait_port "$STATE_PORT" || { echo "控制面未就绪" >&2; cat "$SOAK_DIR/bal.out" >&2; exit 1; }
sleep 1

# ---------- 采样 ----------
CSV="$SOAK_DIR/soak-$(date +%Y%m%d-%H%M%S).csv"
echo "timestamp,elapsed_s,rss_mb,fd_count,active,accepted" >"$CSV"
: >"$SUMMARY_FILE"

# 采一条：时间戳、经过秒、RSS(MB)、fd 数、控制面 active/accepted。
sample_once() {
    local now_ts="$1"
    local elapsed=$((now_ts - START_TS))
    local rss fd active accepted json a b
    rss="$(rss_mb "$BAL_PID")"
    fd="$(fd_count "$BAL_PID")"
    active="NA"
    accepted="NA"
    if command -v curl >/dev/null 2>&1; then
        json="$(curl -fsS --max-time 3 "http://127.0.0.1:$STATE_PORT/api/v1/status" 2>/dev/null || true)"
        if [[ -n "$json" ]]; then
            a="$(printf '%s' "$json" | grep -o '"active":[0-9]*' | head -n1 | cut -d: -f2 || true)"
            b="$(printf '%s' "$json" | grep -o '"accepted":[0-9]*' | head -n1 | cut -d: -f2 || true)"
            active="${a:-NA}"
            accepted="${b:-NA}"
        fi
    fi
    printf '%s,%s,%s,%s,%s,%s\n' "$(date -Is)" "$elapsed" "$rss" "$fd" "$active" "$accepted" >>"$CSV"
}

START_TS=$(date +%s)
DEADLINE=$((START_TS + DURATION_SECS))
echo "== 长稳开始：时长=${DURATION_SECS}s QPS=${SOAK_QPS} 并发=${SOAK_CONCURRENCY} 采样=${SAMPLE_INTERVAL}s 单轮=${FLOOD_CHUNK}s ==" | tee -a "$SUMMARY_FILE"
echo "balancer pid=$BAL_PID 初始 RSS=$(rss_mb "$BAL_PID")MB fd=$(fd_count "$BAL_PID")" | tee -a "$SUMMARY_FILE"

# 基线采样（未加压），便于对比。
sample_once "$START_TS"

ROUND=0
FAILED_ROUNDS=0
while (($(date +%s) < DEADLINE)); do
    now=$(date +%s)
    remaining=$((DEADLINE - now))
    chunk=$((remaining < FLOOD_CHUNK ? remaining : FLOOD_CHUNK))
    ROUND=$((ROUND + 1))

    flood_out="$SOAK_DIR/flood-round${ROUND}-$(date +%Y%m%d-%H%M%S).out"
    echo "== 第 $ROUND 轮 flood_socks5：${chunk}s 目标${SOAK_QPS}QPS 并发${SOAK_CONCURRENCY} ==" | tee -a "$SUMMARY_FILE"
    "$FLOOD_SOCKS5_BIN" 127.0.0.1:"$LISTEN_PORT" 127.0.0.1:"$TARGET_PORT" \
        "$SOAK_CONCURRENCY" "$SOAK_QPS" "$chunk" >"$flood_out" 2>&1 &
    FLOOD_PID=$!

    sample_last=$(date +%s)
    while kill -0 "$FLOOD_PID" 2>/dev/null; do
        now=$(date +%s)
        if ((now >= DEADLINE)); then
            break
        fi
        sleep_for=$((sample_last + SAMPLE_INTERVAL - now))
        if ((sleep_for < 1)); then
            sleep_for=1
        fi
        left=$((DEADLINE - now))
        if ((sleep_for > left)); then
            sleep_for=$left
        fi
        sleep "$sleep_for"
        now=$(date +%s)
        sample_once "$now"
        sample_last=$now
    done
    wait "$FLOOD_PID" 2>/dev/null || true
    FLOOD_PID=""

    round_line="$(tail -n 1 "$flood_out" 2>/dev/null || true)"
    echo "round $ROUND: ${round_line:-<无输出>}" | tee -a "$SUMMARY_FILE"
    if [[ "$round_line" == *"flood_socks5 done:"* ]]; then
        FAILED_ROUNDS=0
    else
        FAILED_ROUNDS=$((FAILED_ROUNDS + 1))
        echo "警告：第 $ROUND 轮 flood_socks5 未正常结束（见 $flood_out）" >&2
        if ((FAILED_ROUNDS >= 3)); then
            echo "连续 3 轮压测客户端异常退出，终止长稳并保留现场" >&2
            exit 1
        fi
    fi
    # 轮次间隔 2s，避免异常快速失败时忙循环。
    if (($(date +%s) < DEADLINE)); then
        sleep 2
    fi
done

summarize
SUMMARY_DONE=1
echo
echo "长稳结束，采样 CSV：$CSV"
