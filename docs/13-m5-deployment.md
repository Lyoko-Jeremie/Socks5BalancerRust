# 13 · M5 部署与迁移说明

> 日期：2026-10-26
> 对应计划：[06-rust-port-assessment.md](06-rust-port-assessment.md) §10 的 **M5**
> 交付物：`Dockerfile`、`deploy/socks5balancer.service`、`deploy/config.example.json`、
> `scripts/ci-linux.sh`、`scripts/loadtest-linux.sh`
> 决策依据：[07-decisions.md](07-decisions.md)（ADR-001 控制面、ADR-003 线程、ADR-005 内存）

---

## 1. 交付物清单

| 文件 | 用途 |
|---|---|
| `Dockerfile` | 多阶段构建镜像（rust 1-slim 构建 → debian-slim 运行，非 root，`/healthz` 健康检查） |
| `deploy/socks5balancer.service` | systemd 单元（`LimitNOFILE=131072`、最小权限加固、SIGTERM 优雅关闭） |
| `deploy/config.example.json` | 生产示例配置（多上游 + TCP/TLS 双探测 + 回环控制面 + 2K 内存口径） |
| `scripts/ci-linux.sh` | Linux CI：fmt/clippy/test/release + 控制面冒烟 |
| `scripts/loadtest-linux.sh` | Linux C10K / 1K/s 压测（见 [12](12-m4-performance.md)） |
| `scripts/m4-memory.ps1` | Windows 内存基线实测 |

---

## 2. 二进制部署

```bash
# 构建（Linux x86_64）
cargo build --release --bins

# 安装
install -m 0755 target/release/Socks5BalancerRust /usr/local/bin/
install -d -o balancer -g balancer /etc/socks5balancer /var/log/socks5balancer
install -m 0640 -o root -g balancer deploy/config.example.json /etc/socks5balancer/config.json

# 校验配置并试运行
/usr/local/bin/Socks5BalancerRust -c /etc/socks5balancer/config.json
```

> 仓库根目录的 `config.example.json` 是**开发用**全字段示例；生产请用 `deploy/config.example.json`
> （已按 100MB 口径设置 `bufferSize=2048`、`maxConnections=10000`）。

---

## 3. systemd 部署

```bash
useradd --system --no-create-home --shell /usr/sbin/nologin balancer
install -m 0644 deploy/socks5balancer.service /etc/systemd/system/
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

## 4. Docker 部署

```bash
docker build -t socks5balancer-rust:local .
docker run -d --name balancer \
  -p 5000:5000 \
  -p 127.0.0.1:5110:5110 \
  -v "$PWD/deploy/config.example.json:/etc/socks5balancer/config.json:ro" \
  --ulimit nofile=131072:131072 \
  --restart unless-stopped \
  socks5balancer-rust:local

# 健康检查（镜像内置 HEALTHCHECK 走 /healthz）
docker inspect --format '{{.State.Health.Status}}' balancer
```

- 控制面默认 `127.0.0.1`（容器内），需要外部访问请显式映射并评估风险（ADR-001：无鉴权）；
- 容器内 `bufferSize=2048` + `maxConnections=10000` 时，RSS 预算见 [12](12-m4-performance.md)；
- 若宿主内核限制容器内端口，请 `--network host` 或调整 `net.ipv4.ip_local_port_range`。

---

## 5. 从 C++ 版迁移

### 5.1 配置字段兼容

Rust 版**直接兼容** C++ `config.json` 的主要字段（camelCase），包含：
`listenHost/listenPort/multiListen`、`upstream[]`（name/host/port/disable/authUser/authPwd；`disable: true` 表示禁用该上游）、
`authClientInfo[]`、`stateServerHost/Port`、`threadNum`、`logLevel`、
`upstreamSelectRule`、`serverChangeTime`、`traditionTcpRelay`、`disableSocks4`、
`bufferSize`、`maxConnections`、`connectTimeout`、`healthCheck.*`。

启动时会打印"配置提示"，列出被忽略或语义变化的字段。

### 5.2 行为差异（迁移前必读）

| 主题 | C++ 版 | Rust 版 | 迁移影响 |
|---|---|---|---|
| 入站 SOCKS5 | 只认 `05 01 00` 并整段透传 | **终结解析**（支持 `NMETHODS>1`、可选 0x02 认证） | 更兼容；上游收到的 CONNECT 由本进程重发（域名由上游解析） |
| 错误应答 | 上游失败后下游常无错误码 | 失败回具体 REP / 5B / 502 | 客户端行为更可预期 |
| 健康检查 | 连续失败计数 + 防抖 | **fail-fast**（一次失败即 DOWN）+ 指数退避 | 故障切换更快，但探测抖动更敏感（可调 `healthCheck.tcp.period`） |
| 全上游 DOWN | 继续尝试（黑洞） | 立即回 REP 0x01 | 客户端快速失败 |
| TLS 探测 | `ConnectTestHttps` 默认 `www.google.com:443` | `healthCheck.target` **必配**（不配则只做 TCP）；`expectStatus` 为空时**不校验状态码**（对齐 C++，只看能否完成 HTTP 往返） | 迁移时需显式配置 target |
| `disableConnectTest` | 跳过全部健康检查，只看人工禁用 | **相同语义**：不探测、数据面失败也不降级 | 一致 |
| 控制面 | 十余端点 + 内置前端 | **4 端点**，无前端 | 原有 `/op?xxx`、`/clientInfo` 等调用需改写 |
| 控制面鉴权 | 无 | 无（同），但响应剔除密码 + 默认回环 | 同 C++ |
| `slowImpl` | 存在（兼容不标准上游） | **未实现** | 若上游非常规，先验证 |
| 上游 `disable: true` | 该上游不进池 | 保留在池中（`enabled=false`），`/api/v1/upstreams` 可见，可用 `enable_upstream` 启用 | 行为更可控 |
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

Rust 实例停止（`systemctl stop` / `docker stop`）→ 恢复 C++ 实例监听原端口即可；
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
| 存活 | `GET /healthz` | 容器/K8s liveness 探针 |
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
3. **客户端认证**：`authClientInfo` 支持 0x02 用户名密码；不配置则允许 0x00 无认证（与 C++ 同）。
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
