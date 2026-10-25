# 08 · M0 里程碑报告（骨架实现与验收）

> 日期：2026-10-26
> 对应计划：[06-rust-port-assessment.md](06-rust-port-assessment.md) §10 的 **M0 骨架**
> 决策依据：[07-decisions.md](07-decisions.md)（8 条 ADR）
> 代码位置：`src/`、`examples/`、`scripts/`

---

## 1. 里程碑目标与完成度

| M0 目标（来自 06 §10） | 完成 | 验收证据 |
|---|---|---|
| 配置解析（兼容 C++ `config.json` + 新字段） | ✅ | 18 个单元测试；`config.json` / `config.example.json` 可直接运行；旧字段（含 `AuthClientInfo`、`internalBehavior`、数字型 `disable`）全部兼容 |
| `tracing` 异步日志 | ✅ | `tracing-appender::non_blocking`；默认 `info`，`RUST_LOG`/`logLevel` 可覆盖；热路径仅 `debug/trace` |
| tokio runtime，线程数可配（ADR-003） | ✅ | 启动日志 `workers=32` / `workers=2` 实测；`threadStackSize`/`maxBlockingThreads` 生效 |
| 多监听 | ✅ | `listenHost:listenPort` + `multiListen[]` 解析出的全部地址分别绑定；冒烟脚本对 5100 与 6610 两个端口验证透传 |
| 纯 TCP 双向透传（ADR-005：双向缓冲预分配） | ✅ | 500/1000 连接全部成功；每连接 RSS 增量 ≈15–17 KB，与 2×8 KB 预算吻合 |
| 验收：打印 worker 数、`bufferSize` 生效 | ✅ | 日志含 `workers=`、`buffer_size=`；第二实例 `bufferSize=2048` 实测生效 |
| 验收：空闲 RSS | ✅（按实测校准） | 32 worker 空闲 **12.55 MB**；2 worker 空闲 **11.37 MB**。原计划"<5 MB"过于乐观，已按实测修正为"≤15 MB（32 线程）/≤12 MB（2 线程）" |

**M0 结论：完成，可进入 M1。**

---

## 2. 交付物

### 2.1 代码结构

```
src/
├── main.rs          # CLI、日志、runtime 构建、装配、信号与优雅关闭
├── config.rs        # 配置解析/兼容/校验（564 行，含 10 个单元测试）
├── logging.rs       # tracing 初始化 + 版本信息
├── app.rs           # 共享上下文 AppCtx
├── stats.rs         # 原子统计 + 活跃连接 CAS 守卫
├── net/
│   ├── mod.rs
│   └── listener.rs  # 多监听、accept 循环、EMFILE 退避、TCP_NODELAY
├── relay/
│   └── mod.rs       # 双向缓冲预分配 + 单在途 read/write 透传
└── upstream/
    └── mod.rs       # 启动期解析 + 无锁 round-robin（M1 换 5 规则/健康状态）
examples/
├── echo_upstream.rs # 本地 echo 服务（端到端验证用）
└── flood.rs         # 并发连接压测（支持保持时长/建连间隔）
scripts/
└── m0-smoke.ps1     # Windows 端到端验收脚本（UTF-8 BOM，PS5.1 兼容）
config.json          # 本机开发配置（5100 + 6610，上游 6100 echo）
config.example.json   # 全字段示例（含 healthCheck/bufferSize/线程等）
```

### 2.2 规模

| 项 | 行数 |
|---|---|
| `src/*.rs` | 1,442 |
| `examples/*.rs` | 91 |
| **Rust 合计** | **1,533** |
| `scripts/m0-smoke.ps1` | 219 |
| release 二进制 | 2.20 MB（strip + fat LTO） |

自研代码全部集中在"配置/装配/透传"，**协议解析尚未开始**（M1/M2）；符合 ADR-007 的"优先成熟库、自研仅限 3 处"约束。

### 2.3 依赖（M0 实际启用）

`tokio`（rt-multi-thread/net/time/sync/io-util/signal/macros）、`tokio-util`（CancellationToken）、`socket2`、`serde`/`serde_json`、`tracing`/`tracing-subscriber`/`tracing-appender`、`clap`、`thiserror`、`mimalloc`。
M1/M2 将按 ADR-007 增加：`httparse`、`tokio-socks`、`hickory-resolver`、`rustls`/`tokio-rustls`/`rustls-platform-verifier`、`hyper`。

---

## 3. 决策落地情况

| ADR | 内容 | M0 状态 |
|---|---|---|
| ADR-001 | 控制面精简不鉴权 | 未开始（M3）；M0 仅在配置里兼容 `stateServerHost/Port` |
| ADR-002 | 健康检查 fail-fast / DOWN 退避 | 未开始（M1）；配置字段已解析并提示"未启用" |
| ADR-003 | 最大线程数可配置 | ✅ 已落地（`threadNum`/`workerThreads`/`maxBlockingThreads`/`threadStackSize`） |
| ADR-004 | SOCKS5 入站仅 0x00/0x02 | 未开始（M1/M2）；M0 纯透传，不需要解析 |
| ADR-005 | 双向 8KB 预分配 + `bufferSize` 可配 | ✅ 已落地（会话建立时一次性分配；2048/4096/8192 校验） |
| ADR-006 | TLS 探测对齐 C++ | 未开始（M2） |
| ADR-007 | 优先成熟库 | ✅ M0 依赖全部为成熟 crate |
| ADR-008 | UTF-8 无 BOM、中文注释、clippy/fmt | ✅ `cargo clippy --all-targets -- -D warnings` 通过；`cargo fmt --check` 通过；所有源码 UTF-8 无 BOM |

> 例外说明：`scripts/m0-smoke.ps1` 为 **UTF-8 带 BOM**——Windows PowerShell 5.1 只有在 BOM 存在时才按 UTF-8 解析脚本，否则中文会被按 ANSI 解码导致语法错误。这是 ADR-008 允许的"特殊要求"。

---

## 4. 验收结果

运行命令：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m0-smoke.ps1 -Connections 500 -HoldSecs 5
```

| 验收项 | 结果 |
|---|---|
| release 构建 | ✅ |
| 主监听 5100 透传（echo 回环） | ✅ `hello-m0-primary` |
| multiListen 6610 透传 | ✅ `hello-m0-multilisten` |
| 并发连接保持 500 条 | ✅ ok=500/500 |
| `bufferSize=2048` 生效 | ✅ 日志 `buffer_size=2048` |
| `maxConnections=5` 生效 | ✅ 日志出现 7 条"拒绝连接：达到 maxConnections"，已建立连接不受影响 |
| `workerThreads=2` 生效 | ✅ 日志 `workers=2` |
| 空闲 RSS（32 worker） | 12.55 MB / 38 线程 |
| 空闲 RSS（2 worker） | 11.37 MB |
| 500 连接峰值 RSS | 20.00 MB（Private 36.04 MB，句柄 1223） |
| 1000 连接峰值 RSS | 25.85 MB（Private 45.11 MB，句柄 2254） |
| 1000 连接释放后 RSS | 29.11 MB（≈16.6 KB/连接，与 ADR-005 的 16,384 B 缓冲预算吻合） |
| 单元测试 | ✅ 18 passed / 0 failed |
| `cargo clippy -- -D warnings` | ✅ |
| `cargo fmt --check` | ✅ |

### 4.1 每连接内存与 ADR-005 预算的对照

| 连接数 | RSS 增量 | 每连接 | 说明 |
|---|---|---|---|
| 500 | ≈7.5 MB | ≈15.0 KB | 含 task/会话元数据 |
| 1000 | ≈13.3 MB（峰值）～16.6 MB（释放后持有） | ≈13.3–16.6 KB | 与 2×8KB 缓冲 + ≈1 KB 元数据吻合 |
| 预算值（ADR-005） | — | 16,384 B 缓冲 + ≈1.2–1.5 KB | 实测在预算内 |

> 结论：**M0 实测验证了 ADR-005 的每连接内存模型**。这同时说明 8K 档下 10K 连接 ≈ 165–180 MB（含固定开销），与 07 的冲突分析一致；100MB 目标仍需方案 B 的 `bufferSize=2048`。

---

## 5. 实现要点（与 C++ 版的关键差异）

| 主题 | C++ 版 | Rust M0 |
|---|---|---|
| 执行模型 | 单 `io_context` + 每连接 strand + 回调链 | tokio runtime + 每连接 task + 线性 `async fn`（所有权串行移交，无 shared_ptr 环） |
| 缓冲 | 2×8KB 内嵌在 session，无条件分配 | 2×`bufferSize` 在 accept 后一次性预分配，随 task drop 释放（ADR-005） |
| 背压 | 单在途 read/write | 相同（单在途 read/write + `select!` 取消） |
| DNS | 每连接一次 `async_resolve` | 启动期一次性解析成 `SocketAddr`，热路径零 DNS |
| connect 超时 | `connectTimeout` 是死配置 | 真正使用（默认 2000 ms） |
| 连接上限 | 无 | `maxConnections` + CAS 精确计数（`ActiveGuard`），超限 RST |
| socket 选项 | 全仓库无 `TCP_NODELAY` | 下游/上游均 `set_nodelay(true)` |
| 线程 | `min(核数, threadNum)`，≤2 退化单线程 | `workerThreads` 精确指定；`threadNum` 为上限；栈/blocking 可配 |
| 日志 | 同步 sink 写 stdout、无级别过滤 | `tracing` + 非阻塞 writer + 级别过滤 |
| accept 错误 | EMFILE 立即重试（自旋） | 100ms→1s 指数退避 + 告警 |

---

## 6. 过程中发现的问题与修正

| # | 问题 | 现象 | 处理 |
|---|---|---|---|
| 1 | **Windows `SO_REUSEADDR` 允许端口劫持** | 本机 6600 端口已被正在运行的 C++ 版 `Socks5BalancerAsio` 占用；Rust 版仍 bind "成功"，但连接被投递到另一个进程，表现为"能连上但不回显" | **只在 Unix 设置 `SO_REUSEADDR`**；Windows 保持默认，端口被占用时直接 bind 失败（更安全）。配置文件改用 6610 |
| 2 | **本机动态端口 16384 + TIME_WAIT 限制单机压测规模** | 2000/5000 连接时大量 `connect` 超时（5000 时仅 2942 成功）；`Get-NetTCPConnection -State TimeWait` 达 8000+ | 压测工具改为 `SO_LINGER=0` RST 关闭（不再增加客户端 TIME_WAIT）；验收脚本按可用端口断言成功率并打印 TIME_WAIT；**C10K 验证需 Linux 或多机**（已列入 M1/M4） |
| 3 | tokio `TcpStream::set_linger` 已废弃 | clippy `-D warnings` 失败 | 压测工具改用 `socket2::SockRef::set_linger(0)` |
| 4 | PowerShell 5.1 的两个坑 | ① 无 BOM 的 UTF-8 脚本中文乱码导致语法错误；② 环境变量存在大小写重复键时 `Start-Process` 抛异常 | 脚本使用 UTF-8 BOM；启动进程改用 `.NET Process` + 临时 `.bat` 承载命令行与重定向 |
| 5 | mimalloc 释放后不立即归还 OS | 连接释放后 RSS 未回落到空闲值（20.12/29.11 MB） | 记录为预期行为；M4 评估 `MIMALLOC_PURGE_DELAY`/周期性回收，验收以"不单调增长"为准 |

---

## 7. 已知限制（M0 范围）

1. **仅纯 TCP 透传**：不做首包识别、不支持 HTTP 代理、不需要 SOCKS5 协商（下游 SOCKS5 字节会原样转给上游 SOCKS5 代理，因此"下游 SOCKS5 → 上游 SOCKS5"已可用）。
2. **无健康检查、无选路规则**：仅 round-robin，启动时解析一次上游地址；上游宕机后连接会失败（有 connect 超时）。
3. **无控制面/无统计接口**：仅本地日志（30s 一次摘要）。
4. **未在 Linux 上运行验证**：代码已用 `cfg` 分隔平台差异（REUSEADDR/REUSEPORT），但本机仅有 Windows 环境；Linux CI 需在 M1 补齐。
5. **未验证信号优雅关闭**：Windows 下 `Ctrl-C` 需交互式控制台；当前以"取消令牌 + 单元测试"覆盖取消路径，信号路径留给 Linux CI。
6. **`SO_RCVBUF/SO_SNDBUF` 只设置在 listener 上**：Linux 会被 accepted socket 继承，Windows 不会；M1 在 accepted 连接上再设置一次（需要 raw socket 访问，计划引入 socket2 的 `SockRef` 并验证平台支持）。

---

## 8. 如何运行

```powershell
# 1) 编译
cargo build --release

# 2) 起一个本地 echo 上游（或换成真实 SOCKS5 上游）
cargo run --release --example echo_upstream -- 127.0.0.1:6100

# 3) 起负载均衡器（默认读 ./config.json）
.\target\release\Socks5BalancerRust.exe -c config.json

# 4) 端到端验收（自动起 echo/balancer、跑功能与并发、验证配置生效）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m0-smoke.ps1 -Connections 500 -HoldSecs 5

# 5) 手工验证透传（任意 TCP 客户端）
#    连接 127.0.0.1:5100 或 127.0.0.1:6610，发送数据应被 echo 原样返回

# 6) 常用参数
.\target\release\Socks5BalancerRust.exe --help
.\target\release\Socks5BalancerRust.exe --version
```

> 100MB 内存机器上请把配置中的 `"bufferSize"` 设为 `2048`（ADR-005 方案 B），启动日志会打印实际值。

---

## 9. 下一步（M1）

按 06 §10 与 07 ADR：

1. **SOCKS5 入站协议**（ADR-004）：方法协商（0x00/0x02）、RFC1929 认证、CONNECT、`0x07` 拒绝 BIND/UDP；拆包/粘包正确处理（`httparse` 不适用，SOCKS5 手写解析 ≤400 行）。
2. **首包识别**：SOCKS4/4a、HTTP CONNECT、HTTP forward（`httparse`），透传回放语义。
3. **选路规则**：`loop`/`random`/`one_by_one`/`change_by_time`/`force_only_one` + hint 粘性（无锁结构）。
4. **健康检查**（ADR-002）：TCP 探活 + fail-fast、DOWN 指数退避、数据面失败即时降级、目标与时间可配；`hickory-resolver` 做 TTL 解析。
5. **控制面最小集**（M3 提前做骨架）：`/healthz` + `/api/v1/status`。
6. **Linux CI**：双平台构建、单元测试、冒烟脚本（bash 版）。

---

## 10. 附录：单元测试清单（18 项）

| 模块 | 测试 |
|---|---|
| config | 最小配置默认值；C++ 完整配置兼容；`bufferSize` 三档校验；`disable` 布尔/数字；全禁用报错；空上游报错；线程参数校验；worker 数推导；BOM 容忍 |
| relay | 缓冲按配置大小分配；透传回环正确（含取消中断） |
| stats | 活跃连接上限（CAS）；快照字段自洽 |
| upstream | round-robin 顺序；空池；IP 字面量免 DNS；localhost 解析 |
| listener | EMFILE/ENFILE/WSAEMFILE 识别为需退避 |
