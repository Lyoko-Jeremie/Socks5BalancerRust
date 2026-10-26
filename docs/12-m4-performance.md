# 12 · M4 性能与内存基线（Windows + Linux 实测）

> 日期：2026-10-26（Windows 基线上午、Linux 实测与 mimalloc 调优当晚）
> 对应计划：[06-rust-port-assessment.md](06-rust-port-assessment.md) §10 的 **M4**
> 决策依据：[07-decisions.md](07-decisions.md) ADR-005（缓冲与内存模型）、ADR-003（线程）
> 脚本：`scripts/m4-memory.ps1`（Windows）、`scripts/loadtest-linux.sh`（Linux C10K / 1K/s / SOCKS5 协议口径）
> 本次 Linux 环境：Manjaro Linux（kernel 6.12.108-1）、**32 vCPU / 64 GB / 无 swap**，
> 项目位于 `~/Socks5BalancerRust`，release 构建（`lto="fat"`）。

---

## 1. 目标与口径

| 目标 | 口径 | 状态（2026-10-26） |
|---|---|---|
| 运行时内存 ≤100MB | 10,000 空闲连接进程 RSS | **Linux 实测**：默认策略 110–116 MB（超标）；`connectionTracking.detail=false` + mimalloc 低 RSS 策略后 **92.98 MB（达标）**；Windows 1000 连接推算 103.6 MB（仅参考） |
| C10K | 10,000 并发空闲长连接 | ✅ **Linux 实测 10000/10000 成功**（峰值 RSS 114.72 MB，CPU 1.12 s/60 s） |
| 1000 新连接/秒 | 持续建连速率 ≥1000/s | ⚠️ 未验证：`examples/flood` 的 rate 模式串行，实测仅 469/s（工具限制，见 §3.2） |
| 2–4 核 / 100MB | 多线程 runtime（线程数可配） | ⏳ 未实测（需 `taskset` 限核复跑） |
| 极端性能 | 每连接 2×`bufferSize` 预分配 + 单在途读写 | ✅ 模型与实测一致（见 §4） |
| 真实 SOCKS5 吞吐 | 1000 QPS × 60 s | ✅ **999.3 QPS / 100% 成功 / p99 0.786 ms**（见 §3.3） |

> **方案 B**（[07](07-decisions.md) ADR-005）：性能基线 `bufferSize=8192`；100MB 达标口径用
> `bufferSize=2048`。本文用两个平台的真实数据校准换算关系。

---

## 2. Windows 实测

### 2.1 基线（mimalloc 默认归还策略）

方法（`scripts/m4-memory.ps1`）：
1. 启动 balancer（首包超时 60s，保证测量窗口内连接不超时）；
2. 采集空闲 RSS；
3. `flood` 建立 1000 条空闲连接并保持 30s（全速建连）；
4. 每 500ms 采样进程工作集/私有提交，取峰值；
5. 每连接增量 =（峰值 − 空闲）/ 1000。

| `bufferSize` | 空闲 RSS | 峰值 RSS | 每连接 RSS | 每连接 Private | 每连接保守值 | 10K 推算 | 判定 |
|---|---|---|---|---|---|---|---|
| 2048 | 13.09 MB | 21.61 MB | 8,933 B | 8,958 B | **8,958 B** | **98.5 MB** | 临界（推算） |
| 4096 | 13.09 MB | 25.56 MB | 13,079 B | 13,439 B | **13,439 B** | 141.3 MB | ❌ 超出 |
| 8192 | 13.10 MB | 25.62 MB | 13,124 B | **21,074 B** | **21,074 B** | 214.1 MB | ❌ 超出 |

> 注：8192 档 RSS 被工作集修剪/内存压缩低估（13,124 B），私有提交 21,074 B 才反映真实分配；
> Windows 单机动态端口约 16,384，1000 连接为可测上限，10K 为线性推算。

### 2.2 mimalloc 归还策略 A/B/C（Windows 是否有效）

同一脚本、同一口径，只改进程环境变量（`mimalloc v3`）：

| 场景 | 环境变量 | 2048 档 perConnRSS | perConnPrivate | 10K 推算 |
|---|---|---|---|---|
| A 代码默认策略（= 0/0） | 不设置（代码内套默认） | 9,228 B | 9,482 B | 103.6 MB |
| B 显式 0/0 | `MIMALLOC_PURGE_DELAY=0`、`MIMALLOC_ARENA_EAGER_COMMIT=0` | 9,294 B | 9,605 B | 104.8 MB |
| C 对照（mimalloc 默认值） | `MIMALLOC_PURGE_DELAY=10`、`MIMALLOC_ARENA_EAGER_COMMIT=2` | 9,286 B | 9,961 B | 108.2 MB |

**结论：Windows 上这三个策略差异在测量噪声内（RSS 9.2–9.3 KB/连接），这两个选项对 Windows
工作集几乎没有影响**（Windows 的页面提交/工作集由 OS 管理，与 Linux 的 `MADV_*` 语义不同）。
Windows 侧 `bufferSize=2048` 的 10K 估算仍在 **103–108 MB**，100MB 硬指标在 Windows 上不达标
（M4 的 100MB 口径以 Linux 为准）。

---

## 3. Linux 实测（Manjaro / 32C / 64G）

> 项目在 `~/Socks5BalancerRust`，`cargo build --release --bins --examples`（1m46s），
> 脚本内 `ulimit -n 131072`；拓扑为 `echo_upstream:6100` + `mock_socks5_upstream:6200` + balancer:5700。
> 所有场景 `ok=… failed=0`。

### 3.1 C10K 空闲（`bufferSize=2048`，10,000 连接 × 60s）

| 指标 | 数值 |
|---|---|
| 建立连接 | **ok=10000 / failed=0**（spawn 0.008s） |
| 空闲 RSS | 20.32 MB |
| **峰值 RSS** | **114.72 MB**（默认策略） |
| CPU | 1.12 s / 60 s（≈1.9% 单核） |
| 每连接增量 | **9,899 B** |

### 3.2 1K/s 建连（30,000 连接，hold 10s）

- 连接结果：**ok=30000 / failed=0**；
- 实际建连速率：**469/s**（`spawn=63.93s`），未达 1000/s；
- 原因：`examples/flood` 的 rate 模式是**串行**建连（每连接 `connect + sleep(1ms)`，实测每连接约
  2.1 ms），单线程上限约 470/s。**这是压测工具限制，不是 balancer 的建连瓶颈**（协议口径下
  balancer 能稳定吃下 1000 QPS，见 §3.3）。要验证 1K/s 需把 flood 改成并发建连 + 令牌桶。

### 3.3 真实 SOCKS5 协议口径（1000 QPS × 60s，并发 64）

| 指标 | 数值 |
|---|---|
| 请求数 | **ok=60000 / failed=0** |
| 成功率 | **100.00%** |
| actual_qps | **999.3** |
| 握手延迟 p50 / p95 / **p99** | 0.325 ms / 0.600 ms / **0.786 ms** |
| CPU | 8.35 s / 60 s（≈14% 单核） |
| 峰值 RSS | 107.19 MB（上一场景 10K 连接释放后未完全回落，见 §3.4） |

> 覆盖完整链路：入站 SOCKS5 协商 + 上游 SOCKS5 认证/CONNECT + 双向回显转发。
> p99 远低于 06 §9.2 的 ≤500 ms 线。

### 3.4 10K 连接内存口径对比（每场景 10,000 连接 × 60s）

| 配置 | 空闲 RSS | 峰值 RSS | 每连接 | 判定（≤100 MB） |
|---|---|---|---|---|
| 2048 + `detail=true`（默认） | 19.96 MB | **116.38 MB** | 10,110 B | ❌ |
| 2048 + `detail=false` | 20.06 MB | **110.44 MB** | 9,477 B | ❌ |
| **2048 + `detail=false` + mimalloc 低 RSS 策略** | **12.87 MB** | **92.98 MB** | **8,400 B** | ✅ **达标** |
| 8192 + `detail=false`（性能基线口径） | 20.05 MB | 224.46 MB | 21,434 B | —（基线口径） |

「mimalloc 低 RSS 策略」= `MIMALLOC_PURGE_DELAY=0` + `MIMALLOC_ARENA_EAGER_COMMIT=0`；
**当前版本已把它设为程序默认行为**（`src/main.rs::apply_mimalloc_low_rss_profile`，可用同名环境变量覆盖），
systemd 单元同时显式设置（双保险，见 [13 §3](13-m5-deployment.md)）。

- 关闭连接明细（`connectionTracking.detail=false`）省 **≈6 MB**（10K × 633 B），
  其中 100–150 B 是明细条目、其余是分片表扩容；
- mimalloc 策略再省 **≈17.5 MB**（峰值）并把空闲基线从 20 MB 压到 12.9 MB；
- 连接释放后 RSS 回落较慢（mimalloc 延迟归还），10s 后仍在下降；做长稳/反复建连测试时需注意。

---

## 4. 每连接开销校准（平台相关）

```
每连接 ≈ 2 × bufferSize（双向预分配缓冲） + 固定开销
```

| 平台 | `bufferSize` | 2×缓冲 | 实测每连接 | 固定开销 |
|---|---|---|---|---|
| Windows | 2048 | 4,096 B | 8,958 B | ≈4.9 KB |
| Windows | 4096 | 8,192 B | 13,439 B | ≈5.2 KB |
| Windows | 8192 | 16,384 B | 21,074 B | ≈4.7 KB |
| **Linux** | 2048 | 4,096 B | 9,477 B（detail=false） | **≈5.3 KB** |
| **Linux** | 2048 | 4,096 B | 8,400 B（+mimalloc 策略） | **≈4.2 KB** |
| **Linux** | 8192 | 16,384 B | 21,434 B | **≈4.9 KB** |

**关键修正**：早前文档估计"Linux 固定开销 1.2–2 KB、10K≈69 MB"**不成立**。Linux 实测固定开销
约 **4.2–5.3 KB/连接，与 Windows 基本持平**；差异主要来自分配器归还策略（mimalloc 默认保留空闲页
导致 RSS 偏高），而不是平台本身。因此：

- `bufferSize=2048` → 10K ≈ 110 MB（默认）/ **93 MB（mimalloc 低 RSS 策略）**；
- `bufferSize=8192` → 10K ≈ 224 MB（性能基线口径，不用于 100MB 验收）。

固定开销的构成（两个平台共有）：每连接 1 个 tokio task（future 状态 + 调度节点）、
relay split/包装、连接监测条目、socket/内核对象映射、分配器页内碎片与页对齐。

---

## 5. 结论（对照 [06 §9.2](06-rust-port-assessment.md)）

| 验收项 | 达标线 | 实测 | 状态 |
|---|---|---|---|
| 10K 空闲连接 | 10000 并发 | 10000 成功（Linux） | ✅ |
| 10K 空闲 RSS（2K 口径） | ≤100 MB（硬指标）/ ≤80 MB（严格线） | **92.98 MB**（调优后）/ 110–116 MB（默认） | ✅ 硬指标 / ❌ 严格线 |
| 10K 空闲 RSS（8K 口径） | ≈197 MB | 224.46 MB | ⚠️ 基线口径，超 14% |
| 1K/s 成功率 ≥99.9% | ≥99.9% | 工具仅 469/s | ⏳ 未验证（改 flood） |
| 1K/s p99 ≤500 ms | ≤500 ms | 协议口径 p99 0.786 ms | 参考达标 |
| 200 并发 ≥500 Mbps | ≥500 Mbps | 未测（无 iperf3-over-SOCKS5 工具） | ⏳ |
| 杀上游摘除 ≤1 `tcp.period` | — | Windows 已 6/6（mk 定性） | ⏳ Linux 量化 |
| 24h 长稳 | 无崩溃/fd 泄漏/RSS 单调增长 | 未做 | ⏳ |
| 2 核限制 | 以上在 2 核下成立 | 未做 | ⏳ |
| 与 C++ 字节级对比 | — | 未做 | ⏳ |

一句话：**C10K 与 SOCKS5 吞吐已实测达标；100MB 需 mimalloc 低 RSS 策略（已设为默认）才能达标；
1K/s、24h、2 核、吞吐、字节级对比仍未验证。**

---

## 6. 已知限制 / 未完成项

1. **1K/s 建连未验证**：`examples/flood` rate 模式串行（实测 469/s），需改为并发建连 + 令牌桶后重测。
2. **100MB 的严格 ≤80MB 线未达**（92.98 MB）：如需更低，只能继续压固定开销（分片表/任务开销）或
   接受 06 §9.2 的"硬指标 ≤100MB"口径。
3. **Windows 100MB 不达标**（103–108 MB）：M4 的 100MB 口径以 Linux 为准；Windows 仅作参考。
4. **24h 长稳未做**：`scripts/soak-linux.sh` 已就绪；本次只做了 60s 保持（RSS 平稳、无增长迹象）。
5. **2 核与吞吐未测**：需 `taskset -c 0,1` 复跑与自建 SOCKS5 吞吐工具。
6. **连接释放后 RSS 回落慢**：mimalloc 延迟归还，长稳观察时应以"稳态峰值"而非"释放后立即值"判定。
7. **健康检查/控制面开销**：本次压测配置关闭了控制面（`stateServerPort=0`）、仅 TCP 探测；
   完整配置会略高，但控制面为按需处理、开销可忽略。

---

## 7. 下一步

1. 修 `examples/flood` 的 rate 模式（并发建连 + 令牌桶），补测 1K/s 成功率与 p99，回填 §3.2/§5。
2. 24h 长稳：`scripts/soak-linux.sh 24h`，观察 RSS/fd/TIME_WAIT。
3. 2 核复跑：`taskset -c 0,1 ./scripts/loadtest-linux.sh`；吞吐用自建 SOCKS5 压测（iperf3 无法直接走 SOCKS5）。
4. Linux CI 首跑并回填结果（`.github/workflows/ci.yml`）。
5. 如要继续压内存：评估连接监测分片表预分配、减少每连接固定结构（属优化项，需单独评审）。
