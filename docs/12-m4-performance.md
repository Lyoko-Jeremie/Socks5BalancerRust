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
| 运行时内存 ≤100MB | 10,000 空闲连接进程 RSS | **Linux 实测**：改动前 110–116 MB（超标）；当前默认（mimalloc 低 RSS 策略 + 明细开启）**97.25 MB（达标）**；`connectionTracking.detail=false` 可再降到 ~91 MB；Windows 1000 连接推算 103–108 MB（仅参考） |
| C10K | 10,000 并发空闲长连接 | ✅ **Linux 实测 10000/10000 成功**（峰值 RSS 114.72 MB，CPU 1.12 s/60 s） |
| 1000 新连接/秒 | 持续建连速率 ≥1000/s | ✅ **999.9/s，30000/30000 成功，p99 0.295 ms**（见 §3.2） |
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

| 指标 | 数值 |
|---|---|
| 连接结果 | **ok=30000 / failed=0** |
| 实际建连速率 | **999.9/s**（`spawn=30.002s`，目标 1000/s） |
| 建连延迟 p50 / p95 / p99 | 0.092 / 0.200 / **0.295 ms** |
| 峰值 RSS / CPU | 97.14 MB / 2.33 s（40 s 窗口） |

> 早期 `examples/flood` 的 rate 模式用 `sleep(1ms)` 串行控速，实测只有 469/s，是**压测工具**问题；
> 已改为绝对时间调度（每 10ms 补齐 `floor(rate×elapsed)` 条，每连接仍独立 task 并发建连），
> 改后 1K/s 口径通过。详见 `examples/flood.rs` 注释。

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
| 2048 + `detail=true` + mimalloc 默认策略（改动前） | 19.96 MB | **116.38 MB** | 10,110 B | ❌ |
| 2048 + `detail=false` + mimalloc 默认策略 | 20.06 MB | **110.44 MB** | 9,477 B | ❌ |
| 2048 + `detail=false` + mimalloc 策略（**环境变量**） | 12.87 MB | **92.98 MB** | 8,400 B | ✅ |
| **2048 + `detail=true` + mimalloc 策略（当前代码默认，`.init_array` 初始化段）** | **13.11 MB** | **97.25 MB** | **8,823 B** | ✅ **默认配置即达标** |
| 8192 + `detail=false`（性能基线口径） | 20.05 MB | 224.46 MB | 21,434 B | —（基线口径） |

**mimalloc 低 RSS 策略** = `MIMALLOC_PURGE_DELAY=0` + `MIMALLOC_ARENA_EAGER_COMMIT=0`。
当前版本通过 **C 运行时初始化段在 `main` 之前**设置（Linux `.init_array` / Windows `.CRT$XCU`，
见 `src/main.rs`），**默认生效、无需任何环境变量**：

- 为什么不能放在 `main` 里：mimalloc 在首次分配时初始化、arena 在创建时读取
  `arena_eager_commit`；实测 `main` 里 `mi_option_set` 读回确实是 0，但 10K 峰值仍是 **110.4 MB**
  （对已创建的 arena 无效）。改到初始化段后同口径降到 **97.25 MB**。
- 运维仍可用同名环境变量覆盖（mimalloc 初始化时读取，设置更早，`detail=false` 时可到 92.98 MB）；
  systemd 单元里也显式写了这两个变量（双保险，见 [13 §3](13-m5-deployment.md)）。
- 关闭连接明细（`connectionTracking.detail=false`）可在默认策略基础上再省 ≈6 MB（历史对照
  116.38→110.44）；调优后的 `detail=false` 未单独复测，估计 ~91 MB。
- 连接释放后 RSS 回落较慢（mimalloc 延迟归还），长稳/反复建连测试时应看"稳态峰值"。

### 3.5 多核利用与吞吐上限（补充实验，2026-10-27）

先修正压测工具：`examples/echo_upstream` 与 `mock_socks5_upstream` 原来是 `current_thread`
单线程运行时，约 6K QPS 就先饱和，导致测出来的"balancer 上限"其实是上游 mock 的上限；改为
`#[tokio::main]` 多线程后才测到真实数字。

| 目标 QPS / 并发 | 实际 QPS | 成功率 | p50 / p99 | balancer 总 CPU |
|---|---|---|---|---|
| 4000 / 256 | 3992 | 100% | 0.625 / 2.27 ms | 0.50 核 |
| 8000 / 512 | 7834 | 100% | 31.9 / 79.1 ms | ≈1.4 核（分散在 10+ 个 worker 线程，各 0.08–0.15） |
| 12000 / 1024 | 7750–8050 | 100% | 64–68 / 150–202 ms | 1.40–1.42 核 |
| 32000 / 2048 | 7722 | 100% | 137 / 1114 ms | 1.42 核 |

**结论**：

1. **多线程调度本身有效**：8000 QPS 时 balancer 的 CPU 分散在 10+ 个 worker 线程（每线程
   0.08–0.15 核），不是单线程跑满；即 tokio work-stealing + 每连接 task 的多核分发工作正常。
2. **但吞吐在约 8K QPS 处封顶**，继续提高目标 QPS 只增加延迟（p99 从 79 ms 涨到 1114 ms），
   CPU 停在 **1.4 核 / 32 核（4.4%）**——这是典型的**单进程内串行段**（非 CPU 饱和）。
3. 关闭/降级连接监测（`detail=false`、`enabled=false`）吞吐不变（7.84–8.0K QPS），
   说明连接注册表的锁**不是**主因；剩余热路径串行点见 §6 第 1 条。
4. 全链路（balancer + mock 上游 + echo 上游 + flood 客户端）总 CPU 约 4–5 核，32 核机器上
   仍有余量，因此这是"单进程设计上的串行段"而非硬件瓶颈。

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
| **Linux** | 2048 | 4,096 B | 8,823 B（`detail=true` + mimalloc 策略，当前默认） | **≈4.6 KB** |
| **Linux** | 2048 | 4,096 B | 8,400 B（`detail=false` + 环境变量策略） | **≈4.2 KB** |
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
| 10K 空闲 RSS（2K 口径） | ≤100 MB（硬指标）/ ≤80 MB（严格线） | **97.25 MB**（默认配置：`detail=true` + mimalloc 策略）/ 116.4 MB（改动前） | ✅ 硬指标 / ❌ 严格线 |
| 10K 空闲 RSS（8K 口径） | ≈197 MB | 224.46 MB | ⚠️ 基线口径，超 14% |
| **1K/s 成功率** | ≥99.9% | **100%**（30,000/30,000，实测 999.9/s） | ✅ |
| **1K/s p99** | ≤500 ms | **0.295 ms**（建连延迟） | ✅ |
| 200 并发 ≥500 Mbps | ≥500 Mbps | 未测（无 iperf3-over-SOCKS5 工具） | ⏳ |
| 杀上游摘除 ≤1 `tcp.period` | — | Windows 已 6/6（mk 定性） | ⏳ Linux 量化 |
| 24h 长稳 | 无崩溃/fd 泄漏/RSS 单调增长 | 未做 | ⏳ |
| 2 核限制 | 以上在 2 核下成立 | 未做 | ⏳ |
| 与 C++ 字节级对比 | — | 未做 | ⏳ |

一句话：**C10K、100MB（2K 口径）与 SOCKS5 吞吐已实测达标**（默认配置 10K 峰值 97.25 MB、
1000 QPS p99 0.786 ms）；1K/s、24h、2 核、吞吐、字节级对比仍未验证。

---

## 6. 已知限制 / 未完成项

1. **多核扩展有串行段**：单进程吞吐约 **8K QPS** 封顶（CPU 仅 1.4/32 核，延迟随并发线性上升）。
   热路径审计出的候选串行点（按怀疑度）：
   - `UpstreamSelector::next*` 的全局 `Mutex<SelectState>`（每连接一次，`Random` 规则还每次
     分配 `Vec<usize>`）；
   - `Upstream::addr()` 的 `Mutex<SocketAddr>`（每连接一次，单上游时所有线程竞争同一把锁）；
   - 每连接 `early_data = buf[..].to_vec()` 的一次堆分配；
   - `Stats` 全局 Relaxed 原子（`active` CAS 与每块字节 `fetch_add`，跨核同一缓存行）；
   - per-core listener `replicas = workers.clamp(1, 4)`，高建连速率时 accept 任务数上限 4。
   远端无 `perf`/`strace`，未能定位到单一函数，只能给出候选与实测特征。
2. **100MB 的严格 ≤80MB 线未达**（默认配置 97.25 MB）：如需更低，只能继续压固定开销
   （分片表/任务开销）或接受 06 §9.2 的"硬指标 ≤100MB"口径。
3. **Windows 100MB 不达标**（103–108 MB）：M4 的 100MB 口径以 Linux 为准；Windows 仅作参考。
4. **24h 长稳未做**：`scripts/soak-linux.sh` 已就绪；本次只做了 60s 保持（RSS 平稳、无增长迹象）。
5. **2 核与 iperf 式吞吐未测**：需 `taskset -c 0,1` 复跑与 SOCKS5 大流量吞吐工具（本轮只做了 QPS/建连口径）。
6. **连接释放后 RSS 回落慢**：mimalloc 延迟归还，长稳观察时应以"稳态峰值"而非"释放后立即值"判定。
7. **健康检查/控制面开销**：本次压测配置关闭了控制面（`stateServerPort=0`）、仅 TCP 探测；
   完整配置会略高，但控制面为按需处理、开销可忽略。

---

## 7. 下一步

1. **多核扩展优化**（§3.5/§6）：选路无锁化（如每 worker 本地 cursor + 原子、`Random` 去掉
   `Vec` 分配）、上游地址改 `AtomicU64`/`RwLock` 读路径、`Stats` 按核分片或批量合并、
   `replicas` 提高到 `min(workers, 核数)`，然后用 16K+ QPS 复测吞吐与 CPU。
2. 24h 长稳：`scripts/soak-linux.sh 24h`，观察 RSS/fd/TIME_WAIT。
3. 2 核复跑：`taskset -c 0,1 ./scripts/loadtest-linux.sh`；吞吐用 iperf 式 SOCKS5 大流量压测
   （本轮只做了 QPS/建连口径，未测 `iperf3` 吞吐）。
4. Linux CI 首跑并回填结果（`.github/workflows/ci.yml`）。
5. 如要继续压内存：评估连接监测分片表预分配、减少每连接固定结构（属优化项，需单独评审）。
