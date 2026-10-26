# 13 · M5 部署与迁移说明

> 日期：2026-10-26
> 对应计划：[06-rust-port-assessment.md](06-rust-port-assessment.md) §10 的 **M5**
> 交付物：`deploy/socks5balancer.service`、`deploy/config.example.json`、
> `scripts/ci-linux.sh`、`scripts/loadtest-linux.sh`、`scripts/soak-linux.sh`、`.github/workflows/ci.yml`、`fuzz/`
> 决策依据：[07-decisions.md](07-decisions.md)（ADR-001 控制面、ADR-003 线程、ADR-005 内存、U7 Docker 暂不实现）

---

## 1. 交付物清单

| 文件 | 用途 |
|---|---|
| `deploy/socks5balancer.service` | systemd 单元（`LimitNOFILE=131072`、最小权限加固、SIGTERM 优雅关闭） |
| `deploy/config.example.json` | 生产示例配置（多上游 + TCP/TLS 双探测 + 回环控制面 + 2K 内存口径） |
| `scripts/ci-linux.sh` | Linux CI：fmt/clippy/test/release + 控制面冒烟 |
| `scripts/loadtest-linux.sh` | Linux C10K / 1K/s / 真实 SOCKS5 协议口径压测（见 [12](12-m4-performance.md)） |
| `scripts/soak-linux.sh` | Linux 24h 长稳（RSS/fd/连接数采样与增长斜率摘要） |
| `scripts/m4-memory.ps1` | Windows 内存基线实测 |
| `.github/workflows/ci.yml` | GitHub Actions CI：Windows/Linux 双平台 `fmt`/`clippy`/`test`，Linux 追加 release 构建与冒烟，另含独立编码检查 job |
| `fuzz/` | cargo-fuzz 目标（SOCKS5 / HTTP / SOCKS4 解析）；需 nightly，CI 不运行（见 `fuzz/README.md`） |

> **容器化不在本期交付范围**（U7 决策，见 [07](07-decisions.md)）。仓库里仍保留 `Dockerfile` 与
> `.dockerignore` 供将来参考，但**不作为验收/交付项、未经验证、不承诺可用**；本文件不提供容器部署步骤，
> 生产部署请用 §2（二进制）或 §3（systemd）。

---

## 2. 二进制部署

```bash
# 构建（Linux x86_64）
cargo build --release --bins

# 安装
install -m 0755 target/release/Socks5BalancerRust /usr/local/bin/
install -d -o balancer -g balancer /etc/socks5balancer
install -m 0640 -o root -g balancer deploy/config.example.json /etc/socks5balancer/config.json

# 校验配置并试运行
/usr/local/bin/Socks5BalancerRust -c /etc/socks5balancer/config.json
```

> 仓库根目录的 `config.example.json` 是**开发用常用字段示例**（不是全字段：完整字段以
> `src/config.rs` 与本文件 §5 的字段说明为准）；生产请用 `deploy/config.example.json`
> （已按 100MB 口径设置 `bufferSize=2048`、`maxConnections=10000`）。
>
> **日志**：程序只写 stdout（`tracing`，由 `RUST_LOG`/`logLevel` 控制级别），**不写文件**。
> systemd 部署下由 journald 收集（`journalctl -u socks5balancer`）。
> 因此不需要预先创建 `/var/log/socks5balancer`（单元里的 `LogsDirectory=` 由 systemd 按需创建，
> 仅为将来支持文件日志预留）。

---

## 3. systemd 部署

```bash
useradd --system --no-create-home --shell /usr/sbin/nologin balancer
install -m 0644 deploy/socks5balancer.service /etc/systemd/system/
# 单元里 Documentation=file:/usr/share/doc/socks5balancer/README.md 指向本文件，
# 必须一并安装，否则 `systemctl status` 的帮助链接是断的
install -Dm644 README.md /usr/share/doc/socks5balancer/README.md
systemctl daemon-reload
systemctl enable --now socks5balancer
systemctl status socks5balancer
journalctl -u socks5balancer -f
```

单元要点：
- `LimitNOFILE=131072`：C10K 场景必需（默认 1024 会在几百连接时报 `EMFILE`）；
- `Restart=on-failure`：崩溃自动拉起（健康检查异常**不会**触发重启，仅摘除上游）；
- `ProtectSystem=strict` + `ReadOnlyPaths=/etc/socks5balancer`：配置只读；
- `TimeoutStopSec=20s`：与程序内 12s 优雅收尾匹配。

---

## 4. 容器化部署（暂不实现）

**本期决策（U7）：暂不实现 Docker 交付。** 生产部署请使用 §2（二进制）或 §3（systemd）。

- 仓库中的 `Dockerfile` 与 `.dockerignore` **保留但不维护**：不参与 CI、未做构建/运行验证、
  不作为验收项；如将来启用容器化，需要重新验证构建（`--locked` 依赖、运行镜像精简、非 root 用户）、
  控制面/监控面的回环绑定与端口发布、`HEALTHCHECK` 行为，并在此处补回部署步骤。
- 若自行基于保留的 Dockerfile 试用，注意此前已识别的两个坑：
  1. **控制面/监控面端口不要直接 `-p` 映射**：`deploy/config.example.json` 里
     `stateServerHost`/`monitorServerHost` 都是 `127.0.0.1`（容器内回环），Docker 的端口发布
     转发到容器 `eth0`，宿主访问不到容器回环，映射了也连不上。确实需要访问时，先把配置改成
     `"stateServerHost": "0.0.0.0"`（监控面同理），再显式做**宿主回环**映射，并注意 ADR-001：
     控制面**无鉴权**，切勿直接暴露到公网。
  2. 容器内 `bufferSize=2048` + `maxConnections=10000` 的 RSS 预算见 [12](12-m4-performance.md)；
     若宿主内核限制容器内端口，需 `--network host` 或调整 `net.ipv4.ip_local_port_range`。

---

## 5. 从 C++ 版迁移

### 5.1 配置字段兼容

Rust 版**直接兼容** C++ `config.json` 的主要字段（camelCase），包含：
`listenHost/listenPort/multiListen`、`upstream[]`（name/host/port/disable/authUser/authPwd；`disable: true` 表示禁用该上游）、
`authClientInfo[]`、`stateServerHost/Port`、`threadNum`、`logLevel`、
`upstreamSelectRule`、`serverChangeTime`、`traditionTcpRelay`、`disableSocks4`、`disableHttpForward`、
`bufferSize`、`maxConnections`、`connectTimeout`、`healthCheck.*`、`dnsTtl`、
`disableConnectionTracker`（映射为 `connectionTracking.enabled=false`）、
`connectionTracking.*`（连接监测，默认明细开启）、`monitorServerHost/Port`（监控端点独立监听）。

**检查时序字段**（默认值与 C++ README "Full configuration" 一致）：
`tcpCheckPeriod`/`tcpCheckStart`（5000/1000ms）、`connectCheckPeriod`/`connectCheckStart`（300000/1000ms）、
`additionCheckPeriod`（10000ms）、`sleepTime`（1800000ms）、`testRemoteHost`/`testRemotePort`，
以及等价的新字段 `healthCheck.tcp.*` / `healthCheck.connect.*` / `healthCheck.target.*`（新字段优先）。

启动时会打印"配置提示"，列出被忽略或语义变化的字段。

### 5.2 行为差异（迁移前必读）

| 主题 | C++ 版 | Rust 版 | 迁移影响 |
|---|---|---|---|
| 入站 SOCKS5 | 只认 `05 01 00` 并整段透传 | **终结解析**（支持 `NMETHODS>1`、可选 0x02 认证） | 更兼容；上游收到的 CONNECT 由本进程重发（域名由上游解析） |
| 错误应答 | 上游失败后下游常无错误码 | 失败回具体 REP / 5B / 502 | 客户端行为更可预期 |
| 健康检查 | 连续失败计数 + 防抖 | **fail-fast**（一次失败即 DOWN）+ 指数退避 | 故障切换更快，但探测抖动更敏感（可调 `healthCheck.tcp.period`） |
| 全上游 DOWN | 继续尝试（黑洞） | 立即回 REP 0x01 | 客户端快速失败 |
| TLS 探测 | `ConnectTestHttps` 默认 `www.google.com:443` | `healthCheck.target` **可选**（不配则只做 TCP）；`expectStatus` 为空时**不校验状态码**（对齐 C++，只看能否完成 HTTP 往返） | 迁移时可显式配置 target |
| `disableConnectTest` | 跳过全部健康检查，只看人工禁用 | **相同语义**：不探测、数据面失败也不降级 | 一致 |
| `disableHttpForward` | 无此开关 | 新增：禁用 HTTP 正向代理（absolute-form 回 `501 + Allow: CONNECT`），CONNECT 不受影响 | 按需开启 |
| 控制面 `set_rule` | 仅查询串改全局规则 | 支持 `scope`(global/client/listen)+`target`+`index` 维度强制上游，`clear=true` 清除 | 更细粒度 |
| `sleepTime` / `additionCheckPeriod` | 存在（`sleepTime` 默认 1800000ms、`additionCheckPeriod` 默认 10000ms） | **默认值与语义均对齐**：`sleepTime` 空闲休眠期**暂停全部自动探测**（TCP/readiness/addition 全停，防规律流量被防火墙拉黑）；`additionCheckPeriod` 在全池不可用**且未休眠**时加速探测；`sleepTime: 0` 可关闭休眠 | 一致；若想保持"始终探测"需显式写 `sleepTime: 0` |
| `tcpCheckStart` / `connectCheckStart` | 1000ms / 1000ms | **相同默认值**：启动后延迟首轮探测；另有新字段 `healthCheck.tcp.startDelay` / `healthCheck.connect.startDelay` | 一致 |
| `disableSocks4` | **默认 `true`**（禁用 SOCKS4/4a） | **相同默认值 `true`**：默认禁用（拦陈旧/实现不正确的客户端）；需要 SOCKS4 时必须显式 `"disableSocks4": false` | 一致；若 C++ 配置里显式写了 `false`，迁移后行为也一致 |
| `disableConnectionTracker` | 关闭连接分析 | 映射为 `connectionTracking.enabled=false`（`connectionTracking.enabled` 优先） | 一致 |
| `upstreamSelectRule` 的 `inherit` | 合法值（继承上层/维度规则） | Rust 无"继承"概念，**按 `random` 处理**并打启动提示；按维度强制上游用控制面 `set_rule(scope=client/listen,…)` | 不会启动失败，但需确认按 random 可接受 |
| 检查时序旧字段缺失时的默认 | `tcpCheckStart`/`connectCheckStart` 默认 1000ms，`sleepTime` 默认 30min | 同 C++ 默认值 | 一致 |
| `serverChangeTime` 默认值 | 5000ms | **60000ms** | 若依赖 `change_by_time` 的切换频率，请显式配置 |
| `stateServerPort` 默认值 | 5010 | **5110** | 与 C++ 共存时需显式错开端口 |
| 控制面 | 十余端点 + 内置前端 | **4 基础端点 + 2 连接监测端点**，无前端 | 原有 `/op?xxx`、`/clientInfo` 等调用需改写 |
| 控制面鉴权 | 无 | 无（同），但响应剔除密码 + 默认回环 | 同 C++ |
| 连接监测 | 会话表 + `/clientInfo`/`/listenInfo`/`/authInfo` | `connectionTracking`（默认明细开启）+ `/api/v1/connections`、`/summary`；`close_conn`/`close_all` 按 `id`/client/listen/upstream 中断；监控端点可独立监听（只读） | 等价的按维度查询与中断，参数契约不同 |
| `slowImpl` | 存在（兼容不标准上游） | **未实现** | 若上游非常规，先验证 |
| 入站认证 | 仅增强模式支持三协议 | **默认即支持**：SOCKS5 0x02 / SOCKS4 USERID / HTTP Basic（失败回 5C / 407） | 配置 `authClientInfo` 即生效 |
| 上游 `disable: true` | 该上游不进池 | 保留在池中（`enabled=false`），`/api/v1/upstreams` 可见，可用 `enable_upstream` 启用；**全部上游都禁用时服务仍可启动**（新连接被拒绝），不再启动失败 | 行为更可控 |
| 每客户端 hint 粘性 | `hintUpstreamIndex` | 已实现：粘性型规则（`one_by_one`/`change_by_time`）按客户端 IP 复用上次成功上游 | 行为接近 |
| `embedWebServer` | 内置静态前端 | 不移植（U2） | 前端需独立部署或不用 |

### 5.3 灰度切换步骤

1. **准备配置**：复制 C++ 配置 → 改 `listenPort`（如 5100→5000）、设置 `healthCheck.target`、
   `stateServerPort`（与旧实例错开，如 5111）、`bufferSize=2048`（100MB 口径）。
2. **并行运行**：Rust 实例监听新端口，用 `scripts/m1-smoke.ps1`/`m2-smoke.ps1` 或人工客户端验证；
   观察 `/api/v1/status` 的 `stats.accepted/active` 与 `health`。
3. **切换客户端**：按客户端/端口逐步切到 Rust 实例（可先切 10% 流量）。
4. **观察**：24h 内关注 fd 数（`ls /proc/<pid>/fd | wc -l`）、RSS、`upstream_handshake_failed`。
5. **下线 C++ 实例**。

### 5.4 回滚

Rust 实例停止（`systemctl stop` 或直接结束进程）→ 恢复 C++ 实例监听原端口即可；
两者配置可共享（Rust 会忽略未知字段并打印提示），回滚无需改配置。

---

## 6. 系统调优（Linux，C10K 场景）

```bash
# 文件描述符
ulimit -n 131072                      # systemd 已由 LimitNOFILE 提供

# 内核网络参数（/etc/sysctl.d/99-socks5balancer.conf）
net.core.somaxconn = 4096             # accept 队列
net.ipv4.tcp_max_syn_backlog = 4096   # SYN 队列
net.ipv4.ip_local_port_range = 10240 65535
net.ipv4.tcp_tw_reuse = 1             # 客户端侧 TIME_WAIT 复用（代理场景常用）
net.ipv4.tcp_fin_timeout = 15
fs.file-max = 1000000
```

`listen` 使用 `SO_REUSEPORT`（Linux）便于多进程/多核扩展；Windows 上不设置 `SO_REUSEADDR`
（避免端口劫持，见 [03](03-protocol.md) 端口陷阱）。

---

## 7. 监控与告警

| 指标 | 来源 | 建议 |
|---|---|---|
| 存活 | `GET /healthz` | systemd `ExecStart`/外部监控探针；若将来部署在容器/K8s，可作 liveness 探针 |
| 连接数/字节/错误 | `GET /api/v1/status` → `stats.*` | 1–5s 采集；`relayErrors/protocolErrors` 突增告警 |
| 上游健康 | `GET /api/v1/upstreams` | `available=false` 持续 >2 周期告警 |
| 全上游不可用 | `stats.rejected` 增长 + `health.available=0` | 立即告警（服务已不可用） |
| 上游切换/降级 | 日志 `标记 DOWN` / `readiness 探测失败` / `liveness 恢复` | 汇总告警 |
| 内存 | `/proc/<pid>/status` `VmRSS` | 按 [12](12-m4-performance.md) 口径设阈值（2K 档 ≈100MB/10K） |

日志为 tracing 文本格式（`RUST_LOG` / `logLevel` 控制），包含 `upstream=`、`peer=`、`rep=` 等字段。

---

## 8. 安全注意

1. **控制面无鉴权**（ADR-001）：默认只监听 `127.0.0.1`；绑定非回环时启动日志告警。
   切勿把 `stateServerPort` 暴露到公网。
2. **上游密码**：控制面响应与日志均不输出密码；但配置文件本身含明文密码 → 建议 `chmod 600`。
3. **客户端认证**：`authClientInfo` 同时覆盖 SOCKS5 0x02 用户名密码、SOCKS4 USERID（只校验用户名）、HTTP Basic（`Proxy-Authorization`/`Authorization`）；不配置则三种入站均免认证。
4. **TLS 探测证书校验不关闭**：默认系统根证书；企业自签用 `healthCheck.target.caFile`。
5. **systemd 加固**：已启用 `NoNewPrivileges`、`ProtectSystem=strict`、`PrivateTmp` 等。

---

## 9. 验证清单（上线前）

- [ ] `cargo test` / `cargo clippy --all-targets -- -D warnings` / `cargo fmt --check` 通过
- [ ] `scripts/m1-smoke.ps1`、`m1-health.ps1`、`m1-tls.ps1`、`m2-smoke.ps1`、`m3-smoke.ps1` 全绿
- [ ] 生产配置通过启动校验（`healthCheck.target` 已填、端口无冲突）
- [ ] `ulimit -n` ≥ 连接数上限 + 余量；`LimitNOFILE` 已设
- [ ] `/healthz` 200；`/api/v1/upstreams` 无密码
- [ ] 杀掉一个上游 → 客户端快速失败 → 重启上游 → 自动恢复（`m1-health.ps1` 同流程）
- [ ] 内存：`VmRSS` 在预算内（2K 档 10K 连接 ≈100MB/Windows、Linux 预计更低）

---

## 10. 下一步

1. Linux 上执行 `scripts/ci-linux.sh` 与 `scripts/loadtest-linux.sh`，回填 [12](12-m4-performance.md) 实测值。
2. 24h 长稳（RSS/fd/内存泄漏/上游切换次数）。
3. 若需要 `close_all`/热重载，按 07 U5 决策后补控制面命令。
4. GitHub Actions 首次运行后回填 CI 结果与耗时（`.github/workflows/ci.yml`）。
