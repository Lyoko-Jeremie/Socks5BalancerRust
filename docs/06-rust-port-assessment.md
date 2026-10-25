# 100MB / C10K / 1K-per-sec 目标可行性评估与 Rust 架构映射

> **2026-10-26 修订（重要）**：用户已就 7 项关键取舍作出决策，权威记录见 [07-decisions.md](07-decisions.md)。本文已按其重写，与 07 冲突处以 07 为准。
>
> 决策要点：控制面精简不鉴权（ADR-001）；健康检查 fail-fast / 不重试 / 异常后端降频 / 全参数可配置（ADR-002）；最大线程数可配置（ADR-003）；SOCKS5 入站仅 `0x00`/`0x02`（ADR-004）；**转发缓冲双向 8KB、会话建立时预分配（ADR-005）**；TLS 探测对齐 C++ 且用成熟库（ADR-006）；尽量少自研（ADR-007）。
>
> 本文把上述决策收敛成可执行的预算、选型、模块划分与验收标准。**预估 ≠ 实测**，M0/M4 阶段必须用真实压测校准。

---

## 1. 需求解读：把硬指标变成验收口径

用户原始要求：

> 极致的性能；运行时内存最大不超过 100MB；支持持有 C10K（10,000）并发连接；支持同一秒内新增创建 1,000 个连接；可运行在最小 2~4 core、100MB RAM 的受限系统上。

| # | 指标 | 精确定义（建议写入验收标准） | 测量方法 |
|---|---|---|---|
| G1 | 并发连接 10,000 | 10,000 条**已完成 SOCKS5 协商、处于可转发状态**的下游连接 + 对应上游连接同时存在 | `ss -s` + 进程 fd 数 + 控制面 `/api/v1/status` |
| G2 | 每秒新建 1,000 连接 | 持续 60 s，每秒 1,000 次"建链→SOCKS5 握手→小请求→关闭"，成功率 ≥99.9%，p99 建链延迟有上界 | 自写 tokio 压测程序 |
| G3 | 运行时内存 ≤100MB | **进程 RSS**（Linux `VmRSS` / Windows Private Working Set）峰值 ≤100MB | 压测期每 100ms 采样，取 p99 与峰值 |
| G4 | 2~4 core 可用 | `taskset -c 0,1`（2 核）下 G1+G2 仍满足 | 限制 CPU 亲和重跑 |
| G5 | 吞吐不退化 | 200 并发流式传输合计 ≥500 Mbps @2 核 | iperf3 经代理 |
| G6 | 稳定性 | 上述负载连续 24h 无崩溃、无 fd 泄漏、RSS 无单调增长 | 长稳脚本 |

> **内存口径必须写清**：G3 指**进程 RSS**，不含内核 socket 缓冲；但 100MB RAM 的整机约束包含内核内存，因此必须显式设置 `SO_RCVBUF/SO_SNDBUF`（见 §3.4）并把 `VM` 侧建议写入部署文档。

---

## 2. C++ 现状 vs 目标（数据来自 [05](05-build-config-deploy.md) §8 实测）

| 指标 | C++ 现状 | 差距 | 结论 |
|---|---|---|---|
| G1 内存 | **实测 22,039 B/连接 → 10K ≈210 MiB**（`sizeof(TcpRelaySession)`=17,296 B，其中 16,384 B 是 2×8KB） | 需 ≤10 KB/连接（按 100MB 全给连接算） | **重写内存模型** |
| G2 建链速率 | 每连接 ~18–24 次堆分配、1 次无缓存 DNS、6–7 次全局统计锁、8–15 条同步日志、accept 串行、无 connect 超时 | 预算 2 CPU-ms/连接（2 核） | **重写连接建立路径** |
| G3 CPU | 每包 4 次全局锁 + map 查找；日志无过滤 | 每包 <100 ns | **把可观测性移出热路径** |
| G4 多核 | 单 io_context 无分片；`min(核数, threadNum)≤2` 退化为单线程 | 按核分片 | **执行模型重设** |
| G5 吞吐 | 单在途 read/write、8KB、无 `TCP_NODELAY` | 微调 | 保留 + 小改 |
| G6 稳定性 | D8/D9 竞态、EMFILE 自旋、无首包超时、引用环泄漏 | 系统性修复 | 重写即解决 |

---

## 3. 内存预算（按 ADR-005：8KB×2 预分配）

### 3.1 预算模型

| 分区 | 预算 | 说明 |
|---|---|---|
| 进程固定开销 | **≤18 MB** | 代码段（静态链接+strip 约 4–6MB）、tokio runtime、线程栈 RSS、控制面、配置、日志缓冲、rustls 根证书 |
| 指标与统计 | **≤4 MB** | 分片原子 + 固定容量客户端表 + 上游结构；**不按连接数存容器** |
| 连接与任务 | **≈17.5 KB × N** | 含 2×8KB 缓冲与元数据（见 3.2） |
| 分配器碎片/瞬时峰值 | **≤5 MB** | mimalloc arena |
| 内核 socket 缓冲 | 不计入 RSS | 但计入整机内存，须显式限制（3.4） |

### 3.2 每连接内存明细（目标 ≤18 KB）

| 项 | 大小 | 说明 |
|---|---|---|
| 双向转发缓冲 | **16,384 B** | 2 × 8KB，会话创建时一次性分配（ADR-005），运行期零分配 |
| tokio task（Future + 调度节点） | 0.8–1.5 KB | 必须实测 `size_of`；大局部变量 `Box` 化 |
| 会话状态 | ~100–200 B | `SocketAddr` 而非 `String`；状态枚举、上游 index、计数器引用 |
| 注册表项（控制面查询/关闭用） | ~50–100 B | 分片 slab；可选采样 |
| `Arc<Ctx>` 等引用 | ~50 B | — |
| **合计** | **≈17.5–18 KB** | 10K 连接 = 160 MiB 缓冲 + 约 15 MB 元数据 |

### 3.3 与 100MB 的冲突（必须让用户裁决）

| 配置 | 每连接 | 10,000 连接（含固定 18MB + 统计 4MB） | 100MB |
|---|---|---|---|
| **ADR-005 原样：2×8KB** | ≈17.5–18 KB | **≈197–200 MB** | ❌ 超约 2 倍 |
| 方案 B：`bufferSize=4K` | ≈9.5–10 KB | ≈117 MB | ⚠️ 仍略超 |
| 方案 B：`bufferSize=2K` | ≈5.5–6 KB | **≈77 MB** | ✅ |
| 100MB 可承载连接数（8K 缓冲） | — | **≈4,400–4,700** | — |

**已裁决（U1 = 方案 B，2026-10-26）**：默认 `bufferSize=8192` 保持"双向 8K 预分配"；`bufferSize` 做成配置项（2048/4096/8192）；100MB 达标压测使用 2048（≈77 MB），性能基准使用 8192（≈197 MB）。两组口径都要跑，见 §9.2。

> 本文后续的预算/验收数字都以**方案 B 的默认 8K、达标配置 2K** 两种口径并列给出。

### 3.4 内核 socket 缓冲（整机 100MB 的关键）

| 平台 | 默认 | 建议 |
|---|---|---|
| Linux `tcp_rmem` | `4096 87380 6291456`（自动调优可达 6MB/连接） | 显式 `SO_RCVBUF/SO_SNDBUF=32–64KB`；部署文档给 `sysctl` 建议 |
| Windows `SO_RCVBUF/SNDBUF` | 默认 64KB（MSDN，待实测） | 同上 |
| 每 fd 内核结构 | ≈1KB | 20,000 fd ≈20MB 内核内存 |

> 20,000 个 socket（10K 双向）若放任自动调优，内核内存可轻易达到数 GB。**这是"100MB RAM 整机"比"100MB RSS"更苛刻之处**，必须在启动时自检并打印建议值。

---

## 4. CPU 时延预算（G2 / G4）

2 核机器每秒只有 2 CPU-秒；1,000 连接/秒 ⇒ **每连接建链预算 2 ms**（留一半给转发则 1 ms）。

| 建链步骤 | C++ 现状 | Rust 目标 | 措施 |
|---|---|---|---|
| accept | ~5–20 µs | ~3–10 µs | 多 accept task + 错误退避（EMFILE 必须 sleep） |
| 选择上游 | ~1–3 µs（全局锁+map） | ~50–200 ns | 预选数组 + 原子游标 + 无字符串 |
| DNS | **0.1–20 ms（最坏）** | **0** | 启动解析 + TTL 刷新（hickory-resolver），热路径只连 `SocketAddr` |
| connect（异步） | 0 CPU，但**无超时可能挂 130s** | 0 CPU + 超时 | `timeout(connectTimeout)` |
| 创建会话 | ~2–10 µs（17KB malloc + 多 shared_ptr） | ~0.5–2 µs | 一次 16KB 分配 + 紧凑结构 |
| 统计/注册 | ~2–5 µs（6–7 次全局锁） | ~100–300 ns | 分片原子 |
| 首包嗅探/握手 | ~5–50 µs | ~2–10 µs | 复用缓冲、零拷贝解析、日志关 |
| 日志 | **~10–100 µs/连接** | ~0（release 默认 info；连接级日志采样） | `tracing` + 编译期裁剪 + 异步 writer |
| **合计** | 作者自述 >70/s 即崩（Node 版） | **~10–30 µs** | 余量充足 |

> 结论：**1,000/s 在 2 核上是低负载**（预算 2ms，实际 10–30µs）。真正的风险是内存（ADR-005）与稳定性。

---

## 5. Rust 技术选型（ADR-007：优先成熟库）

### 5.1 运行时与线程（ADR-003）

```rust
let worker = cfg.worker_threads().unwrap_or_else(|| {
    let n = std::thread::available_parallelism().map(|v| v.get()).unwrap_or(2);
    if cfg.thread_num > 0 { n.min(cfg.thread_num) } else { n }
});
tokio::runtime::Builder::new_multi_thread()
    .worker_threads(worker)                         // threadNum/workerThreads 可配置
    .max_blocking_threads(cfg.max_blocking_threads) // 默认 2
    .thread_stack_size(cfg.thread_stack_size)       // 默认 512KB
    .enable_all()
    .build()
```

- **单 runtime**：控制面与健康检查作为普通 task，不单独开线程（ADR-003）；若后续发现探测阻塞明显，可把健康检查迁到独立 `current_thread` runtime（可选优化）。
- 总线程数 = `worker + max_blocking_threads`，启动日志打印实际值。

### 5.2 依赖清单

| 领域 | crate |
|---|---|
| 运行时 | `tokio`（rt-multi-thread/net/time/sync/io-util/signal/macros） |
| 网络/选项 | `tokio-util`、`socket2` |
| 配置 | `serde`、`serde_json` |
| 日志 | `tracing`、`tracing-subscriber`、`tracing-appender` |
| 控制面 | `hyper` 1.x + `hyper-util` + `http` + `http-body-util`（或 `axum`） |
| HTTP 解析（嗅探） | `httparse` |
| TLS（探测） | `rustls`、`tokio-rustls`、`rustls-platform-verifier`（系统信任库，见 ADR-006） |
| HTTP 客户端（探测） | `hyper`（http1 handshake） |
| SOCKS5 上游客户端 | `tokio-socks`（需验证认证/超时/resolver 支持） |
| DNS | `hickory-resolver`（TTL 缓存） |
| 分配器 | `mimalloc` |
| 锁/快照 | `parking_lot`、`arc-swap` |
| 随机抖动 | `fastrand` |
| CLI | `clap`（derive） |
| 错误 | `thiserror`（边界）、热路径手写 `enum` |

**自研例外仅 3 处**（ADR-007）：首包嗅探/分派（≤300 行）、SOCKS5 入站方法协商+RFC1929（≤400 行）、HTTP CONNECT 解析后接管（≤150 行）。其余一律用库。

### 5.3 Cargo profile

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
strip = true
panic = "unwind"      # 保每连接 panic 隔离（tokio 捕获 task panic）
overflow-checks = false
```

---

## 6. 建议的工程结构

```
src/
├── main.rs                # 配置 → runtime → 组件 → 信号
├── config.rs              # serde 结构 + 默认值 + 旧字段兼容 + 校验
├── net/{listener,sockopt}.rs
├── relay/{mod,buffers,registry}.rs     # 每连接 task；双向 8K 预分配
├── handshake/{mod,socks5,socks4,http,upstream}.rs
├── upstream/{mod,select,resolve}.rs
├── health/{mod,tcp,tls}.rs             # fail-fast + DOWN 退避 + 可配置目标
├── observe/{mod,stats,latency}.rs      # 分片原子 + 1s 聚合
├── control/{mod,api}.rs                # 精简 4 端点、无鉴权
└── platform/mod.rs
```

与 C++ 的对应：`TcpRelayServer/TcpRelaySession → net+relay`、`FirstPackAnalyzer/ProxyHandshakeAuth → handshake`（两条产品线合并）、`UpstreamPool → upstream`、`TcpTest/ConnectTestHttps → health`、`TcpRelayStatisticsInfo/DelayCollection → observe`、`StateMonitorServer/EmbedWebServer → control`（后者不移植）。

---

## 7. 数据面设计约束（按决策更新）

### 7.1 一条连接 = 一个 task，线性 `async fn`
禁止热路径出现全局锁；禁止跨 `.await` 持有 `std::sync::MutexGuard`；状态显式化（`Connecting → Sniffing → Handshaking → Relaying → Closing`）。

### 7.2 缓冲：**双向 8KB、会话建立时预分配**（ADR-005）
- 每连接一个 `Box<[u8; 16384]>`（或两个 8KB），**在 accept 成功后立即分配**，与 session 同生命周期；
- 运行期**零分配、零增长、零池化竞争**；relay 循环直接在其上 `read`/`write_all`；
- 允许的调优只有配置项 `bufferSize`（2048/4096/8192，默认 8192；已裁决 U1=方案 B）；
- **禁止** `Vec::new()`/`BytesMut::resize`/按需增长/池化借还（违反 ADR-005）。

### 7.3 统计：分片 + 原子 + 离线聚合
`AtomicU64::fetch_add(Relaxed)`；每 1s 由聚合任务生成 `Arc<Snapshot>`；禁止热路径加锁；客户端维度用固定容量（Top-N + 淘汰），**不按连接数存容器**（C++ 的 multi_index 副本是反面教材）。

### 7.4 健康检查：fail-fast + 快速切换 + 异常后端降频（ADR-002）
- 单次探测失败即 DOWN，**无连续失败阈值、无采样、无重试**；
- 数据面 connect/首包/握手失败 → 立即把后端标为 `degraded` 并**切换下一个可用后端**；
- DOWN 后按指数退避探测（`initial/multiplier/max/jitter` 可配置），**避免打断自恢复**；
- 恢复 = 降频后**一次**探测成功即 UP；
- 探测目标（host/port/sni/path/expectStatus）与所有时间（period/timeout/startDelay/backoff）均可配置；
- 单写者：只有探测器改状态，数据面只上报失败事件；
- 全池不可用：新连接立即拒绝（RST），**不黑洞**。

### 7.5 超时与闸门
`connectTimeout`（真正使用）、`firstPacketTimeout`（10–30s）、`handshakeTimeout`（10s）、`maxConnections`（默认 10,000，超出 RST）、`shutdownTimeout`。

### 7.6 socket 选项（平台差异集中在 `net::sockopt`）
`TCP_NODELAY`（**必须开**，C++ 缺失）、`SO_REUSEADDR`、Linux `SO_REUSEPORT`（按核 accept）、显式 `SO_RCVBUF/SO_SNDBUF`（32–64KB）、错误路径 `SO_LINGER={1,0}`、可选 `TCP_DEFER_ACCEPT`。

### 7.7 关闭语义与取消
正常关闭可优雅 half-close（可选）；错误关闭 RST；控制面关连接用 `CancellationToken`/`abort`，统计计数在 `Drop` 中归还；进程退出按"停 accept → 广播取消 → 带超时等待 → 退出"。

### 7.8 日志纪律
release 默认 info；热点禁止格式化；每连接**不允许**逐包日志；连接建立/关闭日志可采样（如 1%）。

---

## 8. 控制面（ADR-001：精简、不鉴权）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/v1/status` | 进程/总量/后端/监听/健康摘要（≤64KB，无密码） |
| GET | `/api/v1/upstreams` | 后端逐条状态 |
| POST | `/api/v1/control` | 控制命令（set_rule/enable/disable/check/close_conn/close_all） |
| GET | `/healthz` | 存活探针 |

- **不实现鉴权、不设 CORS、不返回密码/base64**；
- 默认 `127.0.0.1:5110`，启动日志强警告绑定非回环的风险；
- 不移植 `EmbedWebServer`、不复用原 `html/` 前端（U2 已裁决：不复用，后续如需页面另写极简静态页）；
- 端点处理是纯内存操作，可与数据面同 runtime。

---

## 9. 验收与压测方案

### 9.1 工具

自写 tokio 压测程序（连接保持、建链速率、SOCKS5 握手）、iperf3（吞吐）、RSS 采样脚本、24h 长稳脚本。对照组：同机同配置跑 C++ 版。

### 9.2 达标线（按方案 B 双口径）

| 项 | 默认 8K 口径 | 100MB 口径（bufferSize=2K） |
|---|---|---|
| 10K 空闲连接 RSS | ≈197MB（**不满足 100MB，方案 A**） | **≤80MB** |
| 10K 空闲 + 1K/s 建链峰值 | ≈200MB | ≤100MB |
| **10K "建连不发首包" 60s 后 RSS 不增长** | 必须成立（C++ 会永久泄漏） | 必须成立 |
| 1K/s 建链成功率 60s | ≥99.9% | ≥99.9% |
| 1K/s 建链 p99 | ≤500ms | ≤500ms |
| 200 并发 iperf3 合计 | ≥500Mbps @2 核 | ≥500Mbps @2 核 |
| 杀上游后摘除时间 | **≤1 个 `tcp.period`**（数据面失败即时降级） | 同 |
| DOWN 后端探测间隔 | 按退避增大（30s→60s→…→10min） | 同 |
| 恢复后探测间隔 | 立即回到常规周期 | 同 |
| 24h 长稳 | 无崩溃、fd 无泄漏、RSS 无单调增长 | 同 |
| 2 核限制 | 以上成立 | 同 |

---

## 10. 里程碑

| 阶段 | 目标 | 验收 |
|---|---|---|
| **M0 骨架** ✅*（2026-10-26 完成，见 [08-m0-milestone.md](08-m0-milestone.md)）* | 配置（含新字段兼容）、日志、runtime（线程可配）、多监听、纯 TCP 透传 | 实测：空闲 RSS 12.55 MB（32 线程）/ 11.37 MB（2 线程）；500/1000 连接全成功；`workers` 与 `bufferSize` 生效；clippy/fmt/18 单测全绿 |
| **M1 核心** | SOCKS5 0x00/0x02 方法协商、上游选择 5 规则、TLS/TCP 健康检查（fail-fast+降频）、超时/背压/连接上限 | 健康检查用例：杀上游 ≤1 周期摘除；10K 空闲 RSS 按 bufferSize 达标；1K/s ≥99% |
| **M2 协议完整** | SOCKS4/4a、HTTP CONNECT/forward、上游认证、TLS 探测对齐（ADR-006） | 各协议端到端用例 + 与 C++ 字节级对比 |
| **M3 控制面** | 精简 4 端点、无鉴权、统计快照 | 端点契约测试；响应无密码 |
| **M4 达标** | 压测调优、内存校准、24h 长稳 | §9.2 全部达标（方案 B 双口径：8K 性能基线 + 2K 内存达标） |
| **M5 交付** | Dockerfile/systemd/迁移说明 | 可替换原版 |

> 关键：**M1 结束时就跑 10K/1K 压测**，用真实数字校准 8K/2K 双口径（方案 B 已裁决），不要等到最后。

---

## 11. 风险登记表

| ID | 风险 | 影响 | 概率 | 缓解 |
|---|---|---|---|---|
| R1 | **ADR-005（8K×2）与 100MB@C10K 数学互斥** | **高**（核心指标二选一） | 确定 | **已裁决方案 B**：默认 8K（性能基线 ≈197MB）；100MB 达标压测用 `bufferSize=2048`（≈77MB）；验收双口径 |
| R2 | tokio task 单连接 >3KB | 高 | 中 | M0 实测 `size_of`；大局部变量 Box 化 |
| R3 | 协议行为不等价（多方法、SOCKS4、HTTP 边界） | 中 | 高 | 与 C++ 逐用例抓包对比；fuzz 零 panic |
| R4 | Windows/Linux 行为差异（RST/TIME_WAIT/REUSEPORT/linger） | 中 | 高 | 平台封装 + 双平台 CI + Windows 单独压测 |
| R5 | TLS 探测用 `rustls-platform-verifier` 在企业代理/私有 CA 环境下行为差异 | 中 | 中 | 提供 `webpki-roots`/自定义 CA 兜底（配置项），默认系统库 |
| R6 | `tokio-socks` 能力不足（认证/超时/resolver） | 中 | 中 | 先做 1 天技术验证；不满足则升级为 ADR-007 例外并登记 |
| R7 | 内核 socket 缓冲在 20K socket 下吃爆整机内存 | **高**（100MB 机器） | 中 | 显式 `SO_*BUF` + 部署 sysctl 建议 + 启动自检警告 |
| R8 | 健康检查 fail-fast 导致"抖动即下线"（网络瞬时抖动） | 中 | 中 | 这是用户明确取舍；用 DOWN 退避 + 数据面快速切换兜底；可观测 `health_transitions_total` |
| R9 | 控制面无鉴权被误绑公网 | **高**（安全） | 中 | 默认回环 + 启动警告 + 文档；可选 token 留作未来扩展（当前不做） |
| R10 | 复制 C++ 的"无超时→挂起引用环→永久泄漏" | **高** | 中 | 全链路超时；编码规范禁止挂起 future 持有父对象强引用；压测含"建连不发首包" |
| R11 | 范围蔓延（UDP/未实现规则） | 中 | 高 | 严格按 §13 清单 |
| R12 | 缓冲每连接一次 16KB 分配造成 malloc 抖动 | 低 | 中 | mimalloc + 预留 arena；M1 观察分配延迟 |

---

## 12. 决策状态

**已决策**（详见 [07-decisions.md](07-decisions.md)）：

| ADR | 内容 |
|---|---|
| ADR-001 | 控制面精简、不鉴权 |
| ADR-002 | 健康检查 fail-fast、不重试、异常后端降频、目标与时间可配置 |
| ADR-003 | 最大线程数可配置 |
| ADR-004 | SOCKS5 入站仅 0x00/0x02 |
| ADR-005 | 双向 8KB 预分配 + `bufferSize` 可配（2048/4096/8192）；**U1 已裁决方案 B** |
| ADR-006 | TLS 探测对齐 C++、用成熟库 |
| ADR-007 | 优先成熟库、自研仅 3 处 |
| ADR-008 | UTF-8 无 BOM、中文注释、clippy/fmt |

**已裁决（2026-10-26 第二轮）**：

| # | 事项 | 裁决 |
|---|---|---|
| U1 | ADR-005 的 A/B/C 方案 | **方案 B**：默认 8K；`bufferSize` 可配；100MB 用 2048（≈77MB），性能基线用 8192（≈197MB） |
| U2 | 是否复用原 `html/` 前端 | **不复用**：只提供 4 个 `/api/v1/*` 端点 |
| U3 | `bufferSize` 是否可配 | **可配**（随 U1 生效，默认 8192） |

**仍待确认（不阻塞开工，按默认执行）**：

| # | 事项 | 默认 |
|---|---|---|
| U4 | 健康探测 `target.host` 是否允许为空（仅 TCP 探活） | 不允许为空（启动校验失败） |
| U5 | 是否需要 `reload_config` 热重载 | 暂不做 |
| U6 | 是否保留 SOCKS4/4a | 保留 |

---

## 13. 必须有 / 可以不同 / 可以砍

### 必须有（行为等价）
混合端口（SOCKS5 0x00/0x02、SOCKS4/4a、HTTP CONNECT/forward）、5 条选路规则 + hint 粘性、双重健康探测（TCP + TLS/HTTP 真实链路）、死后端隔离 + 自动恢复 + 全下线拒绝、`config.json` 旧字段兼容、状态/控制 API、双向转发语义、Windows+Linux、**全链路超时**。

### 可以不同（实现层）
单 runtime + tokio task（替代 strand+回调链）、两条协议产品线合并为一条、分片原子统计（替代 multi_index+全局锁）、`tracing`（替代 boost::log）、`hyper`（替代 Beast）、`rustls`（替代 OpenSSL）、精简控制面（替代 8 端点 + 内嵌 Web）。

### 可以砍（不做）
`EmbedWebServer` 及静态站点、`ConnectionTracker` 整层、`internalBehavior` 死配置、`/clientInfo` `/authInfo` `/delayInfo` 全量明细、原 `html/` 前端兼容（U2 已裁决不做）、SOCKS5 BIND/UDP（固定回 0x07）。

> 注意：`ConnectTestHttps` 的 **TLS 探测不能砍**（ADR-006 要求对齐），但实现方式从"自建 HTTP 服务 + OpenSSL"改为"rustls + hyper 客户端"，代码量大幅下降。

---

## 14. 一句话结论

> 功能可以 1:1 对齐，架构不能 1:1 翻译。**已按用户决策固化：8K×2 预分配（`bufferSize` 可配，U1=方案 B）、fail-fast 健康检查、精简无鉴权控制面（U2=不复用前端）、线程数可配、TLS 探测用成熟库**。内存与 C10K 的冲突已有明确路径：性能基线用 `bufferSize=8192`（10K ≈197–200MB），100MB 达标压测用 `bufferSize=2048`（10K ≈77MB）。建议先做 M0+M1 薄切片，在 M1 结束就用 10K/1K 压测拿到真实数字。
