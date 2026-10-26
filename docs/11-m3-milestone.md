# 11 · M3 里程碑报告（精简控制面）

> 日期：2026-10-26
> 对应计划：[06-rust-port-assessment.md](06-rust-port-assessment.md) §10 的 **M3**
> 决策依据：[07-decisions.md](07-decisions.md) ADR-001（控制面精简、无鉴权、仅回环、响应不含密码）
> 端点契约：[04-control-monitor.md](04-control-monitor.md) §14.4
> 代码位置：`src/control/mod.rs`、`configs/m3-control.json`、`scripts/m3-smoke.ps1`

---

## 1. 里程碑目标与完成度

| M3 目标（06 §10 / 04 §14.4） | 完成 | 验收证据 |
|---|---|---|
| `GET /healthz` 存活探针 | ✅ | 200 `ok`（不读业务状态） |
| `GET /api/v1/status` 状态摘要 | ✅ | JSON：schema/version/uptime/workers/bufferSize/maxConnections/listen/selectRule/stats/health |
| `GET /api/v1/upstreams` 上游详情 | ✅ | JSON 数组；index=解析成功上游的顺序（通常等于配置顺序）；初始 `disable` 的上游也在列表中且可用 `enable_upstream` 启用；**不含密码/用户名/认证字段** |
| `POST /api/v1/control` 唯一写端点 | ✅ | 精简命令集（见 §4.2），审计日志 |
| 无鉴权、无 CORS、默认回环 | ✅ | 默认 `127.0.0.1:5110`；非回环仅告警；响应无 `Access-Control-*` |
| 响应大小上限 | ✅ | `stateMaxResponseBytes`（默认 65536；超限回 500） |
| 请求体上限 | ✅ | ≤64 KB（超出回 413） |
| 不移植 EmbedWebServer / 旧 `/op` | ✅ | 只服务 4 端点，其它 404 |
| `close_conn` / `close_all` / `reload_config` | ⏳ 未实现 | 需要连接表或热重载（07 U5 未决），未知命令回 `ok:false`+说明 |

**M3 结论：完成并通过 19/19 端到端验收；控制面保持"精简 + 无鉴权 + 默认回环"的安全默认。**

---

## 2. 交付物

### 2.1 代码（Rust 合计 5,223 行，M2 为 4,706 行）

| 模块 | 行数 | 职责 |
|---|---|---|
| `src/control/mod.rs` | 398（含测试） | hyper HTTP/1.1 服务、4 端点路由、命令执行、JSON 构造、审计日志 |
| `src/upstream/mod.rs` | 347（+49） | 规则改 `AtomicU8`（运行期切换）、人工启停掩码、`upstreams()` 访问器 |
| `src/health/mod.rs` | 530（+14） | `probe_now`（`Notify` 提前唤醒探测任务） |
| `src/app.rs` | +3 | `started: Instant`（uptime） |
| `src/config.rs` | 1021（+43） | `state_server` / `state_max_response_bytes` 解析、回环检测 |
| `scripts/m3-smoke.ps1` | ~310 | 19 项端到端验收 |

### 2.2 配置

- `configs/m3-control.json`：5600 主服务 + 5601 控制面
- 默认值：`stateServerHost=127.0.0.1`、`stateServerPort=5110`（显式 0 禁用）
- 其余 configs 显式 `stateServerPort: 0`，避免多实例验收端口冲突

---

## 3. 验收结果（`scripts/m3-smoke.ps1`，19/19 PASS）

| 分类 | 检查 | 结果 |
|---|---|---|
| 只读 | `/healthz` 200 ok | ✅ |
| 只读 | status schema=v1 | ✅ |
| 只读 | status 含统计与健康摘要 | ✅ |
| 只读 | status.listen 正确 | ✅ |
| 只读 | upstreams 返回数组 | ✅ |
| 安全 | upstreams 不含密码/用户名/认证字段 | ✅ |
| 数据面 | 控制前主服务转发可用 | ✅ |
| 控制 | `check_upstream` ok | ✅ |
| 控制 | `disable_upstream` ok → 客户端失败（无可用上游） | ✅ |
| 控制 | `enable_upstream` ok → 转发恢复 | ✅ |
| 控制 | `set_rule(loop)` ok → status.selectRule=loop | ✅ |
| 错误 | 未知命令 `close_all` → ok=false | ✅ |
| 错误 | 未知路径 → 404 | ✅ |
| 安全 | 无 CORS 响应头 | ✅ |
| 日志 | 控制面启动信息 | ✅ |
| 日志 | 控制命令审计日志 | ✅ |

### 3.1 全量回归（M3 改动后）

| 脚本 | 结果 |
|---|---|
| `m1-smoke.ps1` | ✅ 10/10 |
| `m1-health.ps1` | ✅ 6/6 |
| `m1-tls.ps1` | ✅ 9/9 |
| `m2-smoke.ps1` | ✅ 9/9 |
| `m3-smoke.ps1` | ✅ 19/19 |
| `cargo test` | ✅ 75 passed |
| `cargo clippy --all-targets -- -D warnings` | ✅ |
| `cargo fmt --check` | ✅ |

---

## 4. 实现要点

### 4.1 技术选型

- **hyper 1.x**（`server` + `http1`）直接实现 4 端点：不引入 axum/tower 全栈（04 §14.4 允许 hyper 直写，属于"成熟库"而非自研解析）。
- `serde_json` 输出真 bool/数字，并带 `"schema":"v1"`（替代 C++ property_tree 的全字符串化）。
- 每连接 10s 超时；请求体与响应体都有上限。

### 4.2 控制命令（精简集）

| `cmd` | 参数 | 实现 |
|---|---|---|
| `check_upstream` | `index`（可选；缺省=全部） | `HealthRegistry::probe_now` 通过 `Notify` 唤醒探测任务，立即探测一次（不重试） |
| `enable_upstream` / `disable_upstream` | `index` 或 `name` | `UpstreamSelector` 人工启停掩码；被禁用上游不会被选择（即使健康） |
| `set_rule` | `rule` | `SelectRule` 原子切换（loop/random/one_by_one/change_by_time/force_only_one） |

未实现：`close_conn` / `close_all`（需要连接表）、`reload_config`（热重载未裁决）→ 返回 `{"ok":false,"detail":"不支持的命令…"}`。

### 4.3 安全默认（ADR-001）

- **无鉴权**：控制面不做认证；因此默认仅 `127.0.0.1`，绑定非回环时启动告警。
- **响应绝不返回**：上游密码、用户名、base64 认证串（由单测 `upstreams_不含密码与认证字段` 守护）。
- **无 CORS 头**；写操作只走 `POST` JSON（非简单请求）。
- **审计**：每个控制命令记录 1 条日志（来源 IP + cmd + args + 结果）。

### 4.4 与 C++ 版差异

| 主题 | C++ 版 | Rust M3 |
|---|---|---|
| 端点数量 | 十余个（clientInfo/listenInfo/authInfo/delayInfo/op…）+ 静态前端 | 4 个（ADR-001） |
| JSON | property_tree 全字符串化 | serde 真类型 + `schema:"v1"` |
| 控制操作 | `/op?xxx` 查询串 | `POST /api/v1/control` JSON |
| 鉴权 | 无 | 无（同），但响应剔除敏感字段 + 默认回环 |
| 前端 | 内置 html/ | 不移植（U2 已裁决） |

---

## 5. 已知限制

1. **无连接表**：`close_conn` / `close_all` 未实现；M4 若需要可按连接注册表补（会增加每连接一点内存）。
2. **无热重载**：`reload_config` 未实现（07 U5 未决）。
3. **RTT 已提供**：`upstreams.rttMs` 是最近一次探测的耗时（TCP 连接 / HTTP 往返）；暂无延迟直方图与按客户端延迟采样。
4. **状态非快照缓存**：`/status` 每次实时聚合（读原子 + 构造 JSON）；10K 连接下开销可忽略，M4 压测时验证。
5. **控制面与主服务共享 runtime**：慢请求会占用一个 worker 任务（每连接 10s 超时兜底）。

---

## 6. 如何运行

```powershell
cargo build --release

# M3 控制面端到端（19 项）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m3-smoke.ps1

# 手工
.\target\release\Socks5BalancerRust.exe -c configs\m3-control.json
curl http://127.0.0.1:5601/healthz
curl http://127.0.0.1:5601/api/v1/status
curl http://127.0.0.1:5601/api/v1/upstreams
curl -X POST -H "Content-Type: application/json" -d '{"cmd":"set_rule","args":{"rule":"loop"}}' http://127.0.0.1:5601/api/v1/control
```

---

## 7. 下一步

1. **C10K / 1K 建连压测**（Linux 优先）：连接数上限、RSS、握手 CPU、`bufferSize` 对比、控制面开销。
2. **M4：性能调优与 24h 长稳**（延迟采样/RTT、keep-alive 逐请求改写评估、`close_all` 是否需要）。
3. **M5：交付**（Dockerfile/systemd/迁移说明）。
4. **Linux CI**：双平台构建/单测/冒烟脚本（bash 版）。
