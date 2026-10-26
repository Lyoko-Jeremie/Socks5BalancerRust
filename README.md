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

# 上游健康
curl http://127.0.0.1:5110/api/v1/upstreams

# 用 curl 走代理（socks5h = 域名交给代理解析）
curl --proxy socks5h://127.0.0.1:5000 https://example.com
```

同一端口 5000 同时支持：SOCKS5、SOCKS4/4a、HTTP CONNECT、HTTP 正向代理（按首包自动识别）。

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

# 人工禁用/启用某个上游（index 或 name）
curl -X POST -H "Content-Type: application/json" \
  -d '{"cmd":"disable_upstream","args":{"index":0}}' http://127.0.0.1:5110/api/v1/control

# 立即探测一次
curl -X POST -H "Content-Type: application/json" \
  -d '{"cmd":"check_upstream","args":{"index":0}}' http://127.0.0.1:5110/api/v1/control
```

## 关键配置项

| 字段 | 默认 | 说明 |
|---|---|---|
| `listenHost` / `listenPort` | `127.0.0.1` / `5000` | 代理监听地址与端口 |
| `multiListen` | `[]` | 额外监听（`[{"host":"...","port":...}]`） |
| `upstream[]` | — | 上游 SOCKS5：`name`/`host`/`port`/`disable`/`authUser`/`authPwd`（`disable: true` 禁用该上游） |
| `upstreamSelectRule` | `random` | `loop` / `random` / `one_by_one` / `change_by_time` / `force_only_one` |
| `serverChangeTime` | `60000` | `change_by_time` 的切换周期（毫秒） |
| `bufferSize` | `8192` | 每连接双向缓冲；2048/4096/8192（100MB 口径用 2048） |
| `maxConnections` | `10000` | 最大并发连接数 |
| `workerThreads` | `0` | 工作线程数；0 = 自动（常用 2–4） |
| `firstPacketTimeout` | `15000` | 下游首包超时（毫秒），防哑连接 |
| `handshakeTimeout` | `10000` | 协议握手超时（毫秒） |
| `traditionTcpRelay` | `false` | `true` = 纯 TCP 透传（不解析协议） |
| `disableSocks4` | `false` | 禁用 SOCKS4/4a 入站 |
| `stateServerHost/Port` | `127.0.0.1` / `5110` | 控制面地址；`0` 表示禁用 |
| `healthCheck.tcp` | period 5000 / timeout 2000 | TCP 探活周期与超时（一次失败即 DOWN） |
| `healthCheck.target` | 未配置 | TLS/HTTP readiness 目标（如 `{"host":"www.example.com","port":443}`），不配则只做 TCP |

> 完整字段：`config.example.json`（开发）与 `deploy/config.example.json`（生产）。

## 常见问题

- **端口被占用**：`listenPort` 换一个；Windows 上程序不设置 `SO_REUSEADDR`（避免端口劫持）。
- **连接数上不去 / 报 `EMFILE`**：提高系统限制（Linux `ulimit -n 131072`，systemd 用 `LimitNOFILE=`）。
- **上游全是 DOWN**：检查上游地址/认证；`curl /api/v1/upstreams` 看 `failures`，`check_upstream` 可立即重探。
- **控制面安全**：默认只监听回环且**无鉴权**，不要暴露到公网。
- **想只做转发不解析协议**：`traditionTcpRelay: true`。

## 更多文档

- 部署（Docker / systemd / 从 C++ 迁移）：[docs/13-m5-deployment.md](docs/13-m5-deployment.md)
- 完整文档索引与里程碑报告：[docs/README.md](docs/README.md)
- 内存与压测口径：[docs/12-m4-performance.md](docs/12-m4-performance.md)
