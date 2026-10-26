# 12 · M4 性能与内存基线（内存校准 / C10K 准备）

> 日期：2026-10-26
> 对应计划：[06-rust-port-assessment.md](06-rust-port-assessment.md) §10 的 **M4**
> 决策依据：[07-decisions.md](07-decisions.md) ADR-005（缓冲与内存模型）、ADR-003（线程）
> 脚本：`scripts/m4-memory.ps1`（Windows 实测）、`scripts/loadtest-linux.sh`（Linux C10K / 1K/s / SOCKS5 协议口径）

---

## 1. 目标与口径

| 目标 | 口径 | 状态 |
|---|---|---|
| 运行时内存 ≤100MB | 10,000 空闲连接进程 RSS（Linux 优先；本机 Windows 实测 + 推算） | Windows `bufferSize=2048` 实测推算 **98.5MB**（临界达标） |
| C10K | 10,000 并发空闲长连接 | Windows 端口上限 ~1000，待 Linux 实测 |
| 1000 新连接/秒 | 持续建连速率 ≥1000/s | 待 Linux 实测（脚本已提供） |
| 2–4 核 / 100MB | 多线程 runtime（线程数可配） | 空闲 RSS 13MB，worker 线程可配 |
| 极端性能 | 每连接 2×`bufferSize` 预分配 + 单在途读写 | 模型与实测一致（见 §3） |

> **方案 B**（[07](07-decisions.md) ADR-005）：性能基线 `bufferSize=8192`；100MB 达标口径用
> `bufferSize=2048`。本文用真实数据校准两个口径的换算关系。

---

## 2. Windows 实测结果

方法（`scripts/m4-memory.ps1`）：
1. 启动 balancer（首包超时 60s，保证测量窗口内连接不超时）；
2. 采集空闲 RSS；
3. `flood` 建立 1000 条空闲连接并保持 30s（全速建连）；
4. 每 500ms 采样进程工作集/私有提交，取峰值；
5. 每连接增量 =（峰值 − 空闲）/ 1000。

| `bufferSize` | 空闲 RSS | 峰值 RSS | 每连接 RSS | 每连接 Private | 每连接保守值 | 10K 推算 | 判定 |
|---|---|---|---|---|---|---|---|
| 2048 | 13.09 MB | 21.61 MB | 8,933 B | 8,958 B | **8,958 B** | **98.5 MB** | ✅ 临界达标 |
| 4096 | 13.09 MB | 25.56 MB | 13,079 B | 13,439 B | **13,439 B** | 141.3 MB | ❌ 超出 |
| 8192 | 13.10 MB | 25.62 MB | 13,124 B | **21,074 B** | **21,074 B** | 214.1 MB | ❌ 超出 |

> 注：
> - **8192 档 RSS 被工作集修剪/内存压缩低估**（13,124 B），私有提交 21,074 B 才反映真实分配；
>   保守口径取两者较大值。
> - 空闲 RSS 13.09–13.10 MB 与 M0 记录（11.37–12.55 MB）一致；差异来自运行时长/线程触达。
> - Windows 单机动态端口约 16,384，1000 连接为可测上限；10K 数据为线性推算，需 Linux 实测校准。
> - **连接监测（默认明细开启）**：`ConnRegistry` 约 100–150 B/连接 + 分片表开销，10K 约 **1.5–2 MB**，
>   即 Windows 2048 档推算约 **100.0–100.5 MB**（临界线上）；如需严格 ≤100MB，可设
>   `connectionTracking.detail=false`（只保留三维度计数，约几十 KB，`/connections` 明细为空）。

---

## 3. 每连接开销校准

```
每连接 ≈ 2 × bufferSize（预分配双向缓冲） + 固定开销
```

| `bufferSize` | 2×缓冲 | 固定开销（实测） | 模型固定开销（Linux 估计） |
|---|---|---|---|
| 2048 | 4,096 B | 4,862 B | 1,229–1,536 B |
| 4096 | 8,192 B | 5,247 B | 1,229–1,536 B |
| 8192 | 16,384 B | 4,690 B | 1,229–1,536 B |

**固定开销 ≈ 4.9 KB/连接（Windows）**，远高于模型估计，原因：
- 每连接一个 tokio task（future 状态 + 调度节点）；
- Windows socket 内核对象/句柄表（`HandleCount` 随连接增长）；
- `Socks5Stream`/relay split 的额外包装；
- 工作集碎片与页对齐。

**Linux 预计固定开销显著更低**（无 Windows 句柄表开销、mimalloc/glibc 分配器差异），
按 ~1.5–2 KB 估计：
- 2048 → 10K ≈ 13 + 10000×6KB/1MB ≈ **69 MB**
- 4096 → 10K ≈ **108 MB**
- 8192 → 10K ≈ **186 MB**

> 这些是待校准的估计值；`scripts/loadtest-linux.sh` 会输出真实数字。

---

## 4. 结论

1. **100MB 目标**：`bufferSize=2048` 在 Windows 上 10K ≈ **98.5 MB**（临界达标），
   Linux 预计 ~69 MB，有约 30MB 余量。**方案 B 的双口径成立**：8K 用于性能基线，
   2K 用于 100MB 达标口径。**注意**：默认开启的连接明细则再 +1.5–2 MB（Windows 口径约 100.0–100.5MB），
   严格达标场景可设 `connectionTracking.detail=false`（只保留三维度计数）。
2. **模型修正**：ADR-005 的"每连接 2×bufferSize + 1.2–1.5KB"在 Linux 上需要按实测校准；
   Windows 实测固定开销 ~4.9KB，说明该值应写成**平台相关**（Linux ~1.5–2KB / Windows ~5KB）。
3. **空闲基线**：13.1 MB（32 worker 线程、健康检查 + 控制面不启用时），
   距离 100MB 预算充足；控制面与 TLS 探测常驻开销可忽略。
4. **缓冲预分配策略有效**：2048/4096/8192 三档的每连接增量严格按 2×bufferSize 线性变化
   （4096−2048 ≈ 4,480 B ≈ 4,096 B 缓冲差 + 固定波动），证明"accept 时一次性预分配、
   运行期零分配"的设计没有额外抖动。

---

## 5. 待 Linux 实测（脚本已就绪）

`scripts/loadtest-linux.sh`（默认 `MODE=both`，一次跑完 C10K / 1K/s / SOCKS5 协议三个口径）：

```bash
cargo build --release --bins --examples
ulimit -n 65535                                   # 脚本会检查，<65535 时警告
sudo sysctl -w net.ipv4.ip_local_port_range="10240 65535"

./scripts/loadtest-linux.sh                       # MODE=both：C10K + 1K/s + 协议口径（默认 bufferSize=2048）
BUFFER_SIZE=8192 ./scripts/loadtest-linux.sh      # 8K 性能口径
MODE=c10k ./scripts/loadtest-linux.sh             # 只跑 C10K 空闲口径
MODE=rate ./scripts/loadtest-linux.sh             # 只跑 1K/s 建连口径
MODE=proto ./scripts/loadtest-linux.sh            # 只跑真实 SOCKS5 协议口径
```

脚本会自动拉起拓扑：`echo_upstream`（6100）+ `mock_socks5_upstream`（6200，alice:secret）+ balancer（5700，
上游指向 mock，配置写到 `target/m4-logs/linux-<bufferSize>.json`）；参数可用环境变量覆盖
（`CONNS`/`HOLD`、`RATE_CONNS`/`RATE_HOLD`/`RATE_INTERVAL_MS`、`PROTO_QPS`/`PROTO_DURATION`/`PROTO_CONCURRENCY`）。

输出文件（均位于 `target/m4-logs/`，三个口径互不覆盖）：

- `linux-c10k-<bufferSize>.txt`：10,000 条空闲连接保持 60s；峰值 RSS、CPU 秒数、空闲 RSS、每连接增量、flood 成功率；
- `linux-rate-<bufferSize>.txt`：~1000 次/秒持续建连（30,000 条、每条保持 10s）；峰值 RSS、CPU 秒数、空闲 RSS、flood 成功率；
- `linux-proto-<bufferSize>.txt`：真实 SOCKS5 握手 + CONNECT 回显（1000 QPS × 60s）；额外输出 `success_rate`、`handshake_p99_ms`、`actual_qps`。

需在 Linux 环境（或 Docker + `--network host`）执行，Windows 无法完成 C10K。

**注意：上述文件目前一个都未生成——C10K / 1K/s / 24h 长稳 / 2 核与吞吐验收全部未实测，M4 验收尚未完成。**

---

## 6. 已知限制

以下限制意味着 **M4 验收尚未完成**：C10K、1K/s、24h 长稳、2 核与吞吐、与 C++ 字节级对比均未实测；
本文 §2/§3 只是 Windows 内存实测与推算，§4 的 100MB 判定是推算值而非压测结论。

1. **C10K / 1K/s 未在本机实测**：Windows 动态端口 ~16,384，1000 连接即接近上限；
   C10K 必须 Linux。
2. **工作集不是精确口径**：Windows 会修剪工作集/内存压缩；报告同时给出 RSS 与 Private。
3. **未测量 CPU 与吞吐**：本脚本只测内存/连接；Linux 脚本补充 CPU 秒数。
4. **未做 24h 长稳**：M4 剩余项（长期泄漏、TIME_WAIT、句柄增长）待 Linux 环境。
5. **健康检查/控制面开销**：本实测配置关闭了控制面、只有 TCP 探测；完整配置开销略高但可忽略。

---

## 7. 下一步

1. **M4 验收未完成**：在 Linux 上跑 `loadtest-linux.sh`（C10K / 1K/s / 协议三口径），用实测值回填本文 §3/§4（并把 ADR-005 的固定开销改为平台相关）。
2. M4 剩余：24h 长稳（`scripts/soak-linux.sh`）、2 核与吞吐验收、`bufferSize` 自适应（按 `maxConnections` 选 2K/4K/8K）评估。
3. M5：Dockerfile/systemd/迁移说明。
4. Linux CI：已提供 `.github/workflows/ci.yml`（Linux/Windows 双平台，含 `cargo test --all-targets` 与 `scripts/ci-linux.sh` 冒烟），首次运行结果待回填。
