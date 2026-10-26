# 09 · M1 里程碑报告（SOCKS5 协议 + 选路 + 健康检查）

> 日期：2026-10-26
> 对应计划：[06-rust-port-assessment.md](06-rust-port-assessment.md) §10 的 **M1**
> 决策依据：[07-decisions.md](07-decisions.md)（ADR-002 健康检查 / ADR-004 入站协议 / ADR-005 缓冲）
> 代码位置：`src/handshake/`、`src/upstream/`、`src/health/`、`src/relay/`、`configs/`、`scripts/`
> 提交范围：`6ce8270` → `3247045`（M1.1 ~ M1.4）

---

## 1. 里程碑目标与完成度

| M1 目标（06 §10 / 07 ADR） | 完成 | 验收证据 |
|---|---|---|
| SOCKS5 入站方法协商（仅 0x00/0x02，ADR-004） | ✅ | 支持 `NMETHODS>1`（修正 C++ 只认 `05 01 00` 的缺陷）；不匹配回 `05 FF` |
| RFC1929 用户名密码认证 | ✅ | 成功/失败（`01 00` / `01 01`）双路径端到端验证 |
| CONNECT 请求解析（IPv4/IPv6/域名）+ BIND/UDP 拒绝 | ✅ | 域名交上游解析（socks5h）；BIND 回 `05 07`；未知 ATYP 回 `05 08` |
| 上游 SOCKS5 客户端握手（含上游认证） | ✅ | 用 `tokio-socks`（ADR-007 优先成熟库）；上游错误 REP 精确映射回客户端 |
| 首包/握手超时（防哑连接） | ✅ | `firstPacketTimeout` 5s 实测断开；`handshakeTimeout` 覆盖入站协商与上游握手 |
| 上游选择 5 条规则（loop/random/one_by_one/change_by_time/force_only_one） | ✅ | 每规则单测 + 配置校验；`serverChangeTime` 可配 |
| 健康检查：TCP 探活 + fail-fast | ✅ | 一次探测失败即 DOWN；停上游 1.6s 后客户端收到 REP 0x01 |
| 健康检查：DOWN 后指数退避（避免打扰自恢复） | ✅ | initial→×2→cap max，带抖动；单测覆盖封顶 |
| 健康检查：数据面失败立即降级 + 快速重探 | ✅ | 建连/上游 I/O 故障 → `mark_degraded`，1s 后重探 |
| 全部上游不可用 → 快速拒绝（不黑洞） | ✅ | CONNECT 回 REP 0x01，不挂起 |
| TLS + HTTP readiness 探测（M1.5 / ADR-006） | ✅ | 本地自签 HTTPS 目标 9/9 通过；系统根证书或 `caFile` 自定义信任根；状态码可配 |
| liveness + readiness 同时满足才可用（ADR-002） | ✅ | readiness 失败（坏目标）时上游不可用；TCP 恢复后 readiness 探测恢复 |
| 每客户端 hint 粘性（C++ `hintUpstreamIndex`） | ✅ 后续补齐 | 粘性型规则（`one_by_one`/`change_by_time`）按客户端 IP 复用上次成功上游；hint 表上限 4096，超限整体清空 |

**M1 结论：M1.1–M1.5 全部完成并通过端到端验收；C10K/1K 压测与 M2 为下一步。**

---

## 2. 交付物

### 2.1 代码（Rust 合计 3,963 行，M0 为 1,534 行）

| 模块 | 行数 | 职责 |
|---|---|---|
| `src/handshake/socks5.rs` | 411 | 服务端协商：方法选择、RFC1929、CONNECT 解析、REP 错误码 |
| `src/health/mod.rs` | 516 | 健康注册表：TCP liveness + TLS/HTTP readiness、fail-fast、指数退避、数据面降级 |
| `src/health/tls_probe.rs` | 143 | 上游 SOCKS5 → TLS（系统根/caFile）→ hyper HTTP GET → 状态码判定 |
| `src/relay/mod.rs` | 461 | tradition 透传 / SOCKS5 终结双路径、超时、错误应答、双向转发 |
| `src/upstream/mod.rs` | 298 | 5 条选路规则、启动期解析（IPv4 优先）、`is_available` 回调 |
| `src/config.rs` | 978 | 旧配置兼容、认证、超时、选路规则、healthCheck（含 TLS 探测）结构化校验 |
| `src/main.rs` | 276 | 装配、探测任务启动、周期日志、优雅关闭 |
| `src/app.rs`/`stats.rs`/`net`/`logging` | ~391 | 上下文、统计、监听、日志（M0 基础） |
| `examples/mock_socks5_upstream.rs` | 206 | 测试用 SOCKS5 代理（0x00 / 0x02、IPv4 优先、首块诊断日志） |
| `examples/tls_echo_server.rs` | 80 | 本地自签 HTTPS 目标（rcgen + rustls），供可复现 readiness 验收 |
| `examples/socks5_client.rs` | 78 | 测试客户端（复用 tokio-socks，`--auth`） |
| `examples/echo_upstream.rs`/`flood.rs` | 92 | echo 目标 / 并发压测（M0 已有） |

### 2.2 测试与配置

- `configs/m1-inbound-noauth.json` / `m1-inbound-auth.json` / `m1-health.json`
- `scripts/m1-smoke.ps1`：10 项端到端检查（正向链路 + 失败路径 + 协议边界）
- `scripts/m1-health.ps1`：6 项健康检查检查（在线 → DOWN → 恢复）
- 单元测试 **48 个**（M0 18 → M1 48）

---

## 3. 验收结果

### 3.1 协议端到端（`scripts/m1-smoke.ps1`，10/10 PASS）

| # | 检查 | 结果 |
|---|---|---|
| 1 | 无认证入站 + 带认证上游（alice:secret） | ✅ 回显正确 |
| 2 | multiListen 端口透传 | ✅ |
| 3 | 域名目标（由上游解析，socks5h） | ✅ |
| 4 | 入站认证成功 | ✅ |
| 5 | 入站认证失败 → `01 01` | ✅ 客户端收到 "Password auth failure, code: 1" |
| 6 | 未提供认证方法 → `05 FF` | ✅ "No acceptable auth methods" |
| 7 | BIND 命令 → `05 07` | ✅ |
| 8 | 非 SOCKS5 首包（HTTP） | ✅ 服务端断开 |
| 9 | 首包超时（5s） | ✅ 5s 后断开 |
| 10 | 上游不可达 → REP 0x04 | ✅ "Host unreachable" |

### 3.2 健康检查（`scripts/m1-health.ps1`，6/6 PASS）

| # | 检查 | 结果 |
|---|---|---|
| 1 | 上游在线 → 转发成功 | ✅ |
| 2 | 停上游 1.6s → 客户端快速失败（REP 0x01） | ✅ 不挂起 |
| 3 | 日志记录 fail-fast DOWN | ✅ |
| 4 | 日志记录降级/标记 DOWN | ✅ |
| 5 | 重启上游 4.5s → 转发恢复 | ✅ |
| 6 | 日志记录"上游恢复可用" | ✅ |

### 3.3 TLS + HTTP readiness 探测（`scripts/m1-tls.ps1`，9/9 PASS）

> 使用本地自签 HTTPS 目标（`examples/tls_echo_server`，127.0.0.1:18443），不依赖外网、可复现；
> balancer 通过 `healthCheck.target.caFile` 信任自签证书（对齐 C++ 的证书校验语义，不关闭校验）。

| # | 检查 | 结果 |
|---|---|---|
| 1 | TLS+HTTP 探测通过后经该上游转发成功 | ✅ |
| 2 | 日志记录 readiness 探测启用 | ✅ |
| 3 | 日志记录 TLS/HTTP 探测成功 | ✅ |
| 4 | 本地 TLS 服务收到 `GET / HTTP/1.1` 探测请求 | ✅ |
| 5 | 坏目标（不可解析）→ readiness 失败日志 | ✅ |
| 6 | 坏目标时上游不可用（CONNECT 被拒） | ✅ |
| 7 | 停上游 → TCP 探活失败 → 快速失败 | ✅ |
| 8 | 日志记录 TCP 探活失败 | ✅ |
| 9 | 重启上游 → 自动恢复 | ✅ |

### 3.4 质量门禁

| 项 | 结果 |
|---|---|
| `cargo test` | ✅ 53 passed / 0 failed |
| `cargo clippy --all-targets -- -D warnings` | ✅ |
| `cargo fmt --check` | ✅ |
| 源码编码 | ✅ 全部 UTF-8 无 BOM（脚本除外，需 BOM） |

---

## 4. 实现要点（与 C++ 版差异）

| 主题 | C++ 版 | Rust M1 |
|---|---|---|
| 入站方法协商 | 只认 `05 01 00`，其它首包交给"HTTP 探测"或挂死 | 完整解析 `NMETHODS`，不匹配回 `05 FF` |
| 认证 | `ProxyHandshakeAuth` 状态机 | `read_exact` 线性流程，RFC1929 终结 |
| 分帧 | 首包回放 + 状态机 | **`read_exact` 精确分帧，无超读**（HTTP 场景 M2 才需回放） |
| 上游握手 | 自研状态机 | `tokio-socks` 客户端（成熟库） |
| 错误应答 | 上游失败后下游常收不到错误码 | 先连上游/握手，成功后才回 `05 00`；失败回具体 REP |
| 选路 | 全局锁 + `recursive_mutex` + multi_index | 轻量 `Mutex` 状态（每连接一次，不在热路径） |
| 健康检查 | 周期探测 + 连续失败计数 + 防抖 | **fail-fast**（一次失败即 DOWN）+ 指数退避 + 数据面降级（ADR-002） |
| 全 DOWN | 继续尝试（黑洞） | 立即回 REP 0x01（快速失败） |

### 4.1 目标侧错误 vs 上游故障（关键细节）

`is_upstream_fault()` 只把 `Io` / `ProxyServerUnreachable` / `InvalidResponseVersion` / `UnknownAuthMethod` / `InvalidReservedByte` 视为上游故障并降级；`HostUnreachable` / `ConnectionRefused` 等目标侧错误**不降级**，避免"目标不可达"误判为"上游宕机"而把整池打挂。

### 4.2 数据面降级与快速重探

- 建连失败/超时、上游握手 I/O 故障 → `mark_degraded`：立即不可用 + 1s 后快速重探；
- 重探失败 → 进入正常退避（initial→×2→cap）；
- 重探成功 → 恢复、退避重置、日志 "上游恢复可用"。

### 4.3 readiness 探测（M1.5）

- **liveness 通过后才做 readiness**：TCP 都不通时跳过 TLS/HTTP，节省资源；
- readiness 有独立周期、超时与退避（`healthCheck.connect`），失败不影响 liveness 的探测节奏；
- 只有 `liveness && readiness` 才被选择器选用（ADR-002）；
- 证书校验**不关闭**：默认 `rustls-platform-verifier`（系统根），配置 `caFile` 时改用自定义 PEM 信任根（企业自签 CA / 测试），对齐 C++ 的 `ssl::verify_peer`；
- HTTP 请求行使用 origin-form（`GET /path HTTP/1.1`），`Host` 头按端口规则生成；
- 探测并发由 `maxProbeConcurrency` 的信号量限制。

---

## 5. 已知限制

1. **外网 TLS 目标可达性取决于部署环境**：本机对部分外网 IPv4/IPv6 的 TLS 数据会被 RST（curl 直连亦不一致），因此验收改用**本地自签 HTTPS 目标**（`examples/tls_echo_server` + `caFile`），保证可复现。生产部署时 `healthCheck.target` 应配置为**从本机可达**的目标。
2. **每客户端 hint 粘性已补齐**：粘性型规则（`one_by_one`/`change_by_time`）按客户端 IP 复用上次成功的上游；`loop`/`random`/`force_only_one` 保持原规则语义。
3. **`sleepTime` / `additionCheckPeriod` 已实现**：最近 `sleepTime` 内无新连接时暂停常规探测（DOWN 后端仍按退避重探）；全池不可用时按 `additionCheckPeriod` 加速探测。
4. **C10K/1K 建连压测未做**：Windows 单机动态端口 16,384 的限制仍在（M0 已记录），需 Linux 或多机验证；M1 的 SOCKS5 握手会额外消耗端口与时间。
5. **Linux 未实测**：平台差异已用 `cfg` 分隔，CI 待补。
6. **`slowImpl` 握手兼容未实现**：C++ 的兼容模式（部分上游不标准）计划 M2。
7. **`rustls-platform-verifier` 首次调用会加载系统根证书**：冷启动探测可能略慢，属预期。

---

## 6. 如何运行

```powershell
cargo build --release

# 协议端到端（10 项）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m1-smoke.ps1

# 健康检查端到端（6 项）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m1-health.ps1

# TLS + HTTP readiness 端到端（9 项，本地自签 HTTPS，不依赖外网）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m1-tls.ps1

# 手工：起 mock 上游与 balancer，再用客户端验证
.\target\release\examples\mock_socks5_upstream.exe 127.0.0.1:6200 alice:secret
.\target\release\Socks5BalancerRust.exe -c configs\m1-inbound-noauth.json
.\target\release\examples\socks5_client.exe 127.0.0.1:5100 127.0.0.1:6100 "hello"
```

> `configs/m1-tls-local.json` 里 `caFile` 指向 `target/m1-tls-logs/cert.pem`，
> 该证书由 `tls_echo_server` 启动时生成；单独手工运行时先启动 TLS 服务再启动 balancer。

---

## 7. 下一步

1. **M2：SOCKS4/4a、HTTP CONNECT/forward、首包嗅探与回放** ✅*（2026-10-26 完成，见 [10-m2-milestone.md](10-m2-milestone.md)）*
2. **M3：控制面最小集** ✅*（2026-10-26 完成，见 [11-m3-milestone.md](11-m3-milestone.md)）*
3. **C10K / 1K 建连压测**（Linux 优先）：连接数上限、RSS、握手 CPU 占用、`bufferSize` 对比。
4. **Linux CI**：双平台构建/单测/冒烟脚本（bash 版）。
