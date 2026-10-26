# Socks5BalancerRust

SOCKS5 / SOCKS4 / HTTP 上游负载均衡器（C++ `Socks5BalancerAsio` 的 Rust 重写）。
一个进程同时提供：多上游选路、健康检查与故障摘除、混合端口代理、精简 HTTP 控制面。

## 快速开始（3 步）

### 1. 构建

```bash
cargo build --release
```

### 2. 最小配置（保存为 `config.json`）

```json
{
  "listenHost": "127.0.0.1",
  "listenPort": 5000,
  "stateServerHost": "127.0.0.1",
  "stateServerPort": 5110,
  "bufferSize": 2048,
  "maxConnections": 10000,
  "upstream": [
    { "name": "upstream-a", "host": "10.0.0.11", "port": 1080 },
    { "name": "upstream-b", "host": "10.0.0.12", "port": 1080, "authUser": "user", "authPwd": "pass" }
  ]
}
```

> 上游必须是 **SOCKS5 代理**（本程序是负载均衡器，本身不提供翻墙线路）。
> `bufferSize=2048` 对应 100MB 内存口径（10K 连接 ≈ 98MB，见 [docs/12](docs/12-m4-performance.md)）。

### 3. 启动

```powershell
# Windows
.\target\release\Socks5BalancerRust.exe -c config.json

# Linux
./target/release/Socks5BalancerRust -c config.json
```

启动后看到 `监听已就绪 local=127.0.0.1:5000` 即成功。`Ctrl+C` 优雅退出。

## 验证

```bash
# 存活探针（应返回 ok）
curl http://127.0.0.1:5110/healthz

# 运行状态（JSON）
curl http://127.0.0.1:5110/api/v1/status

# 上游健康与最近探测 RTT
curl http://127.0.0.1:5110/api/v1/upstreams

# 用 curl 走代理（socks5h = 域名交给代理解析）
curl --proxy socks5h://127.0.0.1:5000 https://example.com
```

同一端口 5000 同时支持：SOCKS5、SOCKS4/4a、HTTP CONNECT、HTTP 正向代理（按首包自动识别）。
> **SOCKS4/4a 默认是关闭的**（`disableSocks4` 默认 `true`，与 C++ 版一致，用于拦掉实现不正确的
> 陈旧客户端）；需要时在配置里显式写 `"disableSocks4": false`。

## 本地自测（不依赖真实上游）

```powershell
# 终端 1：回显目标
.\target\release\examples\echo_upstream.exe 127.0.0.1:6100
# 终端 2：mock SOCKS5 上游
.\target\release\examples\mock_socks5_upstream.exe 127.0.0.1:6200
# 终端 3：把 config.json 的 upstream 改成 127.0.0.1:6200 后启动 balancer
# 终端 4：客户端验证
.\target\release\examples\socks5_client.exe 127.0.0.1:5000 127.0.0.1:6100 "hello"
```

## 常用操作

```bash
# 查看选路规则与流量
curl http://127.0.0.1:5110/api/v1/status

# 切换选路规则（loop/random/one_by_one/change_by_time/force_only_one）
curl -X POST -H "Content-Type: application/json" \
  -d '{"cmd":"set_rule","args":{"rule":"loop"}}' http://127.0.0.1:5110/api/v1/control

# 对某客户端强制使用上游 1（粘性规则下生效；把 clear 设为 true 可清除）
curl -X POST -H "Content-Type: application/json" \
  -d '{"cmd":"set_rule","args":{"scope":"client","target":"10.0.0.5","index":1}}' http://127.0.0.1:5110/api/v1/control

# 对某个监听地址强制使用上游 0（scope 也可为 global）
curl -X POST -H "Content-Type: application/json" \
  -d '{"cmd":"set_rule","args":{"scope":"listen","target":"127.0.0.1:5000","index":0}}' http://127.0.0.1:5110/api/v1/control

# 人工禁用/启用某个上游（index 或 name）
curl -X POST -H "Content-Type: application/json" \
  -d '{"cmd":"disable_upstream","args":{"index":0}}' http://127.0.0.1:5110/api/v1/control

# 立即探测一次
curl -X POST -H "Content-Type: application/json" \
  -d '{"cmd":"check_upstream","args":{"index":0}}' http://127.0.0.1:5110/api/v1/control

# 连接监测（默认开启明细）：查看当前连接与三维度摘要
curl http://127.0.0.1:5110/api/v1/connections
curl http://127.0.0.1:5110/api/v1/connections/summary

# 中断某客户端 / 某监听 / 全部连接（需明细，默认开启）
curl -X POST -H "Content-Type: application/json" \
  -d '{"cmd":"close_conn","args":{"client":"10.0.0.5"}}' http://127.0.0.1:5110/api/v1/control
curl -X POST -H "Content-Type: application/json" \
  -d '{"cmd":"close_all"}' http://127.0.0.1:5110/api/v1/control
```

## 关键配置项

| 字段 | 默认 | 说明 |
|---|---|---|
| `listenHost` / `listenPort` | `127.0.0.1` / `5000` | 代理监听地址与端口 |
| `multiListen` | `[]` | 额外监听（`[{"host":"...","port":...}]`） |
| `upstream[]` | — | 上游 SOCKS5：`name`/`host`/`port`/`disable`/`authUser`/`authPwd`（`disable: true` 禁用该上游） |
| `authClientInfo` | `[]` | 客户端认证列表（`[{user,pwd}]`）；启用后 SOCKS5 走 0x02、SOCKS4 校验 USERID、HTTP 校验 Basic |
| `upstreamSelectRule` | `random` | `loop` / `random` / `one_by_one` / `change_by_time` / `force_only_one` |
| `serverChangeTime` | `60000` | `change_by_time` 的切换周期（毫秒） |
| `dnsTtl` | 未配置 | 上游域名 DNS 刷新周期（毫秒）；不配则沿用启动期解析结果 |
| `bufferSize` | `8192` | 每连接双向缓冲；2048/4096/8192（100MB 口径用 2048） |
| `maxConnections` | `10000` | 最大并发连接数 |
| `workerThreads` | `0` | 工作线程数；0 = 自动（常用 2–4） |
| `firstPacketTimeout` | `15000` | 下游首包超时（毫秒），防哑连接 |
| `handshakeTimeout` | `10000` | 协议握手超时（毫秒） |
| `traditionTcpRelay` | `false` | `true` = 纯 TCP 透传（不解析协议） |
| `disableSocks4` | `true` | 禁用 SOCKS4/4a 入站。**默认 `true`（对齐 C++）**：拦掉实现不正确的陈旧客户端；需要 SOCKS4 时显式写 `false` |
| `disableHttpForward` | `false` | 禁用 HTTP 正向代理（absolute-form）；此时回 `501 + Allow: CONNECT`，CONNECT 不受影响 |
| `stateServerHost/Port` | `127.0.0.1` / `5110` | 控制面地址；`0` 表示禁用 |
| `monitorServerHost/Port` | 未配置（复用控制面） | 监控端点独立监听（只读，不含 `/control` 写命令）；不配或 `monitorServerPort=0` 时与控制面共用监听 |
| `connectionTracking` | 启用且明细开启 | 连接监测：`enabled`/`detail`/`detailLimit`/`clientTopLimit`；`detail=false` 只保留三维度计数（`/connections` 为空） |
| `disableConnectionTracker` | `false` | 旧字段：`true` 等价 `connectionTracking.enabled=false` |
| `healthCheck.tcp` | period 5000 / timeout 2000 / startDelay 1000 | TCP 探活周期、超时与**首轮延迟**（一次失败即 DOWN） |
| `healthCheck.connect` | period 300000 / timeout 11000 / startDelay 1000 | TLS/HTTP readiness 探测（需配 `healthCheck.target`） |
| `healthCheck.connectFailMarkDegraded` | `true` | 数据面 connect/握手失败是否立即标记该上游不可用（`false` = 只计数，交给周期探测判定） |
| `healthCheck.target` | 未配置 | TLS/HTTP readiness 目标（如 `{"host":"www.example.com","port":443}`），不配则只做 TCP |
| `sleepTime` | `1800000` | **30 分钟内无新连接 → 暂停全部自动探测**（TCP/readiness/全池加速探测都停），防止长时间无人使用时后端链路上出现规律探测流量被防火墙拉黑；新连接或 `check_upstream` 立即恢复；**写 `0` 关闭休眠** |
| `additionCheckPeriod` | `10000` | 全池不可用且未休眠时按该周期加速探测（把恢复窗口压到一个周期内）；`0` 关闭 |
| `tcpCheckStart` / `connectCheckStart` | `1000` / `1000` | 旧字段：启动后首轮探测延迟（等价 `healthCheck.tcp.startDelay` / `healthCheck.connect.startDelay`） |

> **关于自动检查的时序（迁移/上线前必读）**：默认节奏是"启动后 1s 首探、之后每 5s 一轮 TCP 探活、
> 每 5min 一轮 TLS/HTTP 探测；**30 分钟没有任何新连接就全部安静下来**"，与 C++ 版一致。
> 如果后端与本程序同时启动、后端起得更慢，`tcpCheckStart`/`connectCheckStart` 与启动期乐观判定
> （启动瞬间先视为可用）会避免"一启动就全判死"；万一全池被判死，`additionCheckPeriod` 会每 10s
> 加速重探，后端起来后自动恢复。

> 示例配置：`config.example.json`（开发，含常用字段）与 `deploy/config.example.json`（生产，含 TLS 探测/连接监测）；
> **完整字段字典**见 [docs/13 §5](docs/13-m5-deployment.md) 与 `src/config.rs`（未知字段会在启动时告警）。

## 连接监测（简要）

- **默认开启每连接明细**（与 C++ 的会话表行为接近）：`GET /api/v1/connections` 支持按 `client` / `listen` / `upstream` 过滤，`cursor` + `limit` 分页；`GET /api/v1/connections/summary` 返回按上游/客户端/监听三维度的连接数。
- **内存代价**：明细约 100–150 B/连接（10K 连接约 1.5–2 MB）；100MB 口径压测可设 `connectionTracking.detail: false`，只保留三维度计数（约几十 KB），`/connections` 返回空列表。
- **中断连接**：`close_conn`（按 `id` / `client` / `listen` / `upstream`）与 `close_all` 需要 `detail: true`；只关闭命中的连接，其它连接不受影响。
- **独立监控监听**：配置 `monitorServerHost` / `monitorServerPort` 后，监控端点使用单独端口（只读，**不提供** `/api/v1/control`）；不配置则挂在控制面监听上。
- **安全**：监控端点同样**无鉴权**、默认回环；暴露到网络前请自行加访问控制（防火墙/反代鉴权）。

## 常见问题

- **端口被占用**：`listenPort` 换一个；Windows 上程序不设置 `SO_REUSEADDR`（避免端口劫持）。
- **连接数上不去 / 报 `EMFILE`**：提高系统限制（Linux `ulimit -n 131072`，systemd 用 `LimitNOFILE=`）。
- **上游全是 DOWN**：检查上游地址/认证；`curl /api/v1/upstreams` 看 `failures`，`check_upstream` 可立即重探。
- **控制面安全**：默认只监听回环且**无鉴权**，不要暴露到公网。
- **想只做转发不解析协议**：`traditionTcpRelay: true`。
- **内存比预期高 / 10K 连接超 100MB**：确认 `bufferSize=2048` 且需要严格口径时设
  `connectionTracking.detail=false`；程序默认已启用 mimalloc 低 RSS 策略
  （`MIMALLOC_PURGE_DELAY=0` + `MIMALLOC_ARENA_EAGER_COMMIT=0`，可用同名环境变量覆盖）。
  Linux 实测 10K 空闲连接峰值 **≈93 MB**（默认策略 114.7 MB）；Windows 上这两个选项对工作集几乎无影响，
  详见 [docs/12](docs/12-m4-performance.md)。

## 更多文档

- 部署（systemd / 二进制 / 从 C++ 迁移；**Docker 暂不实现**，见 [13 §4](docs/13-m5-deployment.md)）：[docs/13-m5-deployment.md](docs/13-m5-deployment.md)
- 完整文档索引与里程碑报告：[docs/README.md](docs/README.md)
- 内存与压测口径：[docs/12-m4-performance.md](docs/12-m4-performance.md)
