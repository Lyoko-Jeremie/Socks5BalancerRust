# 07 · 架构决策记录（ADR）

> 本文件是**移植决策的唯一权威来源**。前六篇是"C++ 现状分析"，本文件是"Rust 实现要怎么做"的决策结果。
> 决策作出人：项目负责人（用户）；记录与影响分析：分析组；日期：2026-10-26。
> 状态标记：**已决策**（可执行）/ **已裁决**（曾有互斥约束，已由用户拍板）。当前无未裁决的阻塞项。
> 所有文档若与本文冲突，**以本文为准**；本文发布后已同步修订 01/02/04/05/06 的对应结论。

---

## 0. 决策摘要

| ID | 决策 | 状态 | 主要影响 |
|---|---|---|---|
| **ADR-001** | 控制面采用**精简实现**，**不做鉴权** | 已决策 | 端点收敛为 4 个；移除 token/CORS/密码回显；风险接受（默认仅绑定回环） |
| **ADR-002** | 健康检查**快速失败、快速切换**：**不做重试/去抖**；对异常后端**降低检查频率**（退避），避免打断自恢复；**检查目标与各段时间全部可配置** | 已决策 | 重写健康状态机；新增 `healthCheck.*` 配置；数据面失败即时降级 |
| **ADR-003** | **最大使用线程数可在配置文件配置** | 已决策 | `threadNum` 语义明确为上限；新增 `workerThreads` 精确指定；总线程预算 = worker + blocking |
| **ADR-004** | SOCKS5 入站**只支持 `0x00` 无认证与 `0x02` 用户名密码认证** | 已决策 | 必须解析方法协商（不再只认 `05 01 00`）；`0x01/0x03+` 一律 `05 FF`；CMD 仅 CONNECT |
| **ADR-005** | 转发缓冲**保持双向 8KB 设计，会话建立时提前分配**；`bufferSize` 可配（2048/4096/8192，默认 8192） | 已决策（冲突已裁决为**方案 B**） | 每连接 16,384 B 缓冲常驻（8K 档）；10K 连接 ≈197–200 MB；**2K 档 ≈77 MB 满足 100MB** |
| **ADR-006** | `ConnectTestHttps` 的 SSL 探测**对齐 C++ 版语义**，TLS 用成熟库实现 | 已决策 | `rustls` + `tokio-rustls` + 系统信任库 + `hyper`；强制证书校验 |
| **ADR-007** | 全工程**尽可能少自研、优先成熟库** | 已决策 | 依赖清单固化；仅协议嗅探等 3 处保留少量自研，并说明理由 |
| **ADR-008** | 代码/配置/文档统一 **UTF-8 无 BOM**，注释中文 | 已决策（沿用项目规范） | 构建与 CI 加编码检查 |

**决策状态：全部已裁决。** 2026-10-26 追加裁决：

- **U1 = 方案 B**：默认保持 `bufferSize=8192`（双向 8K 预分配），同时把 `bufferSize` 做成配置项（允许 2048/4096/8192）；100MB 达标压测使用 2048（≈77 MB）或 4096（≈117 MB）。8K 口径作为性能基准、2K 口径作为内存基准，两组都要跑。
- **U2 = 不复用原 `html/` 前端**：控制面只提供 4 个 `/api/v1/*` 端点，不实现旧 `/` `/op` 兼容层；后续如需页面，另写一个极简静态页。
- U3 随 U1 一并解决（`bufferSize` 可配，默认 8192）。

---

## ADR-001 · 控制面精简、不鉴权

### 背景
C++ 版控制面是 `StateMonitorServer`（1146 行）+ `EmbedWebServer`（394 行）：8 个端点、20 条 `/op` 控制指令、GET-only、CORS 反射任意 Origin、无鉴权，`/` 还明文返回客户端密码。对移植目标（受限资源、2~4 核、100MB）而言这是过重的组件，且安全设计不合格。

### 决策
1. **控制面只保留 4 个端点**（新增端点前缀 `/api/v1`，语义重新设计，**不承诺与原 `/op?xxx` 兼容**）：

| 方法 | 路径 | 作用 | 返回 |
|---|---|---|---|
| `GET` | `/api/v1/status` | 进程/总量/后端/监听/健康摘要/延迟摘要 | 一个 JSON（≤64 KB 上限） |
| `GET` | `/api/v1/upstreams` | 后端逐条详情（host/port/name/state/RTT/conns/enabled） | JSON 数组 |
| `POST` | `/api/v1/control` | 控制命令（JSON body），见下表 | `{"ok":true}` 或错误 |
| `GET` | `/healthz` | 存活探针（容器/K8s 用） | `200 ok` 纯文本 |

2. **控制命令**（`POST /api/v1/control`，body 形如 `{"cmd":"...","args":{...}}`）：

| `cmd` | 参数 | 作用 |
|---|---|---|
| `set_rule` | `rule`，或 `scope`(global/client/listen) + `target`(ip 或 ip:port) + `index`（`clear=true` 清除） | 改选路规则 / 设置维度强制上游（粘性规则生效；C++ `forceNowUseServer` 语义的 Rust 版：强制目标不可用时回落） |
| `enable_upstream` / `disable_upstream` | `index` 或 `name` | 启用/禁用后端（人工） |
| `check_upstream` | `index`（可选） | 立即探测（一次，不重试） |
| `close_conn` | `id` / `client` / `listen` / `upstream`（任一） | 中断匹配连接（需 `connectionTracking.detail=true`，默认开启） |
| `close_all` | — | 中断全部连接（需明细） |
| `reload_config` | — | 热重载（U5：**不做**） |

3. **不实现**：鉴权、TLS、CORS（不设置 CORS 头）、`/clientInfo` 全量明细、`/authInfo`、`/delayInfo` 原始序列、静态文件服务（原 `EmbedWebServer` 整体不移植）。
4. **不返回敏感信息**：响应中**绝不**出现客户端密码、base64 认证串、上游密码。这条与"不鉴权"无关——即使只绑回环也不返回凭据。
5. 默认绑定 `127.0.0.1:5110`（沿用 `stateServerHost/stateServerPort`），文档与启动日志**强警告**：绑定非回环地址等于把无鉴权控制面暴露给网络。
6. 实现用成熟 Web 框架（见 ADR-007），不手写 HTTP 解析。

### 后果
- **风险接受**：无鉴权 → 任何能访问监听地址的人可关连接/禁用后端。缓解：默认回环 + 启动警告 + 文档。
- **U2 已裁决：不复用原 `html/` 前端**（原前端依赖 `/` 的大 JSON 与 `/op?xxx`）。控制面只提供 4 个 `/api/v1/*` 端点；若以后要做页面，另写一个极简静态页（fetch 新 API），不再兼容旧端点。
- 控制面代码量从 C++ 的 ~1500 行降到目标 ≤400 行。
- 控制面与数据面同 runtime（见 ADR-003），但命令处理是纯内存操作（µs 级），不会阻塞转发。

---

## ADR-002 · 健康检查：快速失败、快速切换、异常后端降频、全参数可配置

### 背景
C++ 版健康检查的问题（详见 [04](04-control-monitor.md) §3）：
- 判定链路长且不一致：`isOffline`（TCP 探测）与 `lastConnectFailed`（HTTPS 探测）混用，恢复要求两者都成功；
- **应用层探测周期默认 300 s**，后端坏了要最多 5 分钟才判死，期间连接继续被分到坏后端；
- 探测无超时控制（`async_resolve` 无硬超时）、无并发限制；
- 全池 DOWN 时后端仍在被高频探测（`additionTimer`），可能打断后端自恢复；
- 检查目标硬编码 `www.google.com:443` + 强制 TLS 校验，目标不可达则全池永久 DOWN。

### 决策（逐条，均为硬性要求）

1. **快速失败（fail-fast）**：**单次探测失败即判定该后端不可用**。不做连续失败计数、不做多次采样、不做重试（`retry` 字段在配置中不存在）。
2. **不做去抖/半开**："半开"仅保留为"降频后的一次探测成功即恢复"的语义，不是独立状态机。
3. **快速切换**：数据面在 `connect` 失败 / 首包超时 / 握手失败时，**立即**把该后端标记为"疑似不可用"（`degraded` → 由探测器确认 `down`），当前连接**立即切换到下一个可用后端**（换后端，不是重试同一后端）。这样"探测周期再长"也不会让坏后端继续吃流量。
4. **异常后端降频（关键）**：判定为 `down` 的后端**不做高频探测**，改为**指数退避**（可配置初始值/倍数/上限/抖动），避免打断后端自恢复过程。

   | 状态 | 探测行为 |
   |---|---|
   | `up` | 按常规周期探测（`tcp.period` / `connect.period`） |
   | `degraded`（数据面报错，待确认） | 只由探测器按"失败后首次延迟"探测一次，成功即回 `up` |
   | `down` | 按退避序列 `initial × multiplier^n`（cap `max`，±`jitter`）探测；成功一次即回 `up`，失败继续退避 |

5. **检查目标可配置**：`healthCheck.target.{host,port,sni,httpPath,userAgent,expectStatus}`。默认不指向第三方——**启动校验按实现允许为空**（不配 target 则只做 TCP liveness；旧 `testRemoteHost/testRemotePort` 已映射到 target，见 U4 裁决）。
6. **各段时间全部可配置**：探测周期、首次延迟、单次探测超时、失败退避、抖动、最大并发探测数（见 §5 配置草案）。
7. **判定语义与 C++ 对齐**：
   - `liveness`（TCP 建连）与 `readiness`（经上游 SOCKS5 到目标完成一次 TLS+HTTP 往返）分开记录；
   - 后端可用 = `liveness && readiness && !manual_disabled`（沿用 C++ 的"两者都通过才可用"）；
   - **只看能否完成一次 HTTP 往返**，默认不校验状态码（对齐 C++ `ConnectTestHttps` 的判定）；如需严格可用 `expectStatus` 配置。
8. **单写者（未完全实现）**：实现中健康探测器任务写健康状态，数据面连接失败直接调用 `mark_degraded`（1s 后快速重探确认），未走"只上报事件"的 mpsc 路径；行为等价（及时摘除），但并发写路径比理想方案多一处（原子操作，无数据竞争）。
9. **全池不可用**：新连接立即拒绝（RST/关闭）并计入 `rejected_total`，**不黑洞**（C++ 会接受连接后永久挂起，见 04 §4.5）。
10. **`sleepTime` 保留且默认生效（1800000ms）**：最近 `sleepTime` 内无新连接就进入空闲休眠，**暂停全部自动探测**（TCP 周期探测、readiness 周期探测、`additionCheckPeriod` 加速探测都停）；新连接或控制面 `check_upstream` 恢复。目的不是省流量，而是防止长时间无人使用时，有规律的周期探测被链路状态检测/防火墙判为异常通信而拉黑后端（C++ README "Full configuration" 与作者说明）。显式 `sleepTime: 0` 关闭休眠。注意：`additionCheckPeriod` 的加速探测**不能**刷新"最近连接时间"，否则全池 DOWN 且无人使用时会自己阻止进入休眠。
11. **`additionCheckPeriod` 保留且默认生效（10000ms）**：仅在全池不可用**且不在 `sleepTime` 休眠中**时加速探测（对齐 C++ `UpstreamPool.cpp:419-423`）。
12. **`tcpCheckStart` / `connectCheckStart` 默认 1000ms**：启动后延迟首轮探测，给"后端与本程序同时启动、后端起得更慢"留窗口；同时启动期乐观判定（liveness/readiness 初值 true），避免一启动就把所有后端判死。

### 后果
- 坏后端在**一个探测周期 + 一次连接失败**内被摘除，不再等 5 分钟。
- `down` 后端探测频率从"每 5 s"降到"30 s 起指数退避"，避免打断自恢复。
- 探测目标可选：配置后做 TLS/HTTP readiness，不配置则只做 TCP liveness；不再依赖 `www.google.com` 的可用性。
- 配置字段增加，需要保持旧字段兼容（旧字段映射见 §5.2）。

---

## ADR-003 · 最大线程数可配置

### 背景
C++ 版 `threadNum` 只在上限意义上生效：`实际线程 = min(hardware_concurrency, threadNum)`，且 `≤2` 时退化为单线程（`src/main.cpp:175-207`）。用户要求线程数可配置，且需要能限制在 2~4 核机器上的总量。

### 决策
1. `threadNum`（沿用字段名）：**最大工作线程数上限**，`0`/缺省 = 自动（= 可用核数）。
   - 实际 worker 数 = `min(available_parallelism, threadNum)`（当 `threadNum>0`）。
2. 新增 `workerThreads`（可选，默认不设）：**精确指定** worker 线程数，覆盖 `threadNum` 与自动推导；允许超过核数，但启动时打印警告（超订阅无收益，仅用于特殊场景）。
3. 新增 `maxBlockingThreads`（可选，默认 2）：`spawn_blocking` 线程上限（DNS、文件、配置读取等）。
4. **总线程预算**（写进文档与验收）：`总线程数 = effective_worker + maxBlockingThreads`；默认 4 核机器 = 4 + 2 = 6 线程（栈按 512 KB 计，虚拟内存约 3 MB，RSS 远小）。
5. 线程栈大小 `threadStackSize`（可选，默认 512 KB）可配置，用于小内存机器。
6. 控制面与健康检查**不单独开 runtime/线程**，作为普通 task 运行在同一个多线程 runtime 中（简化部署、降低线程数）；需要阻塞的操作用 `spawn_blocking` 并受 `maxBlockingThreads` 约束。
7. **运行时配置热重载：M0–M4 不做（U5 裁决）**；仅线程数不热更（改线程数需重启，启动日志打印实际值）。后续如需再实现 `reload_config`。

### 后果
- 线程数完全可控，2~4 核机器可精确限制。
- 控制面/健康检查与数据面共享 worker：极端情况下监控请求会占用 worker，但精简控制面（纯内存操作）影响可控。
- 若后续证明健康探测阻塞明显，可将健康检查移到 1 个独立 `current_thread` runtime（保留为可选优化，不改变配置语义）。

---

## ADR-004 · SOCKS5 入站仅支持 `0x00` / `0x02` 两种认证方法

### 背景
C++ 默认模式（`FirstPackAnalyzer`）只认首包恰好为 `05 01 00`（VER=5、NMETHODS=1、METHOD=0x00），其他一律当"非 SOCKS5"处理；只有 `Need_ProxyHandshakeAuth=ON` 才解析完整协商并支持 RFC1929 用户名密码。用户要求明确：**当前只支持 `0x00` 无认证与 `0x02` 用户名密码认证**。

### 决策
1. **必须解析 SOCKS5 方法协商**（不再只匹配固定字节序列）：
   - 读 `VER(1)=0x05`、`NMETHODS(1)`、`METHODS(NMETHODS)`；
   - 方法选择规则：
     | 条件 | 选择 |
     |---|---|
     | 未配置客户端认证（`AuthClientInfo` 为空）且 METHODS 含 `0x00` | `0x00` |
     | 配置了客户端认证且 METHODS 含 `0x02` | `0x02`（走 RFC1929 校验） |
     | 配置了客户端认证但 METHODS 不含 `0x02` | 回 `05 FF` 并关闭（**不降级为无认证**） |
     | 未配置认证但 METHODS 不含 `0x00`（只含 `0x02` 等） | 回 `05 FF` 并关闭 |
   - `0x01`(GSSAPI)、`0x03`~`0x7F`、`0x80`~`0xFE`、`0xFF`：不支持 → `05 FF` 并关闭。
2. **快速路径保留**：当选 `0x00` 且上游也无需认证时，允许"解析校验后把原始字节回放给上游"的透传优化（保留 C++ 的零改写优势）。当选 `0x02` 时必须**终结认证**（本程序校验），认证通过后再以自己的身份向上游建连。
3. **命令范围**：仅 `CMD=0x01 CONNECT`。`BIND(0x02)` / `UDP ASSOCIATE(0x03)` → `REP=0x07`（Command not supported）并关闭；其他未知 CMD 同样 `0x07`。
4. **错误应答必须送达**：先在内部完成上游建连（或先确定失败原因），再向客户端写 SOCKS5 应答；避免 C++ 中"上游失败后 `close` 取消了下游错误应答"的问题。
5. 上游侧认证（`upstream[].authUser/authPwd`）继续支持，与入站方法无关。
6. 边界：`NMETHODS=0` → `05 FF`；`NMETHODS` 与实际字节不一致时，等待累计够 `2+NMETHODS` 字节（**正确处理拆包**，不要求"一次读完"）。

### 后果
- 多方法客户端（如 `05 02 00 02`）不再被误判为 HTTP，行为比 C++ 默认模式更正确（这是**有意偏离**，已在 03 的缺陷 A-1 中列为 C++ 缺陷）。
- 认证路径与无认证路径统一到一套状态机，不再需要 `Need_ProxyHandshakeAuth` 编译期宏。
- 必须新增"方法协商/认证"的 fuzz 用例与拆包用例。

---

## ADR-005 · 转发缓冲：双向 8 KB、会话建立时提前分配

### 决策
1. 每连接**两个 `bufferSize` 大小的缓冲**（downstream→upstream、upstream→downstream 各一；默认 `bufferSize=8192`，即 8 KB），在**会话对象创建时（accept 后、开始转发前）一次性分配**；运行期不复用、不增长、不重新分配。
2. 分配方式：`Box<[u8; N]>` × 2（或一个 `Box<[u8; 2N]>` 再切分）；**不允许**在每个包里 `Vec::new()`/`BytesMut::resize`。
3. relay 循环保持"单在途 read + 单在途 write"（背压模型），缓冲区随会话生命周期存在，会话结束随 task drop 释放。
4. 缓冲大小**不做运行期切换**（连接进行中不变、不热更新）；但 `bufferSize` 是**启动时读取的配置项**（默认 8192，允许 2048/4096/8192），启动时校验并打印实际值。若需要把配置也去掉，可用 cargo feature 编译期固定（可选）。

### 理由
- 与 C++ 行为一致，便于字节级对比验证；
- 运行期零分配、零拷贝额外开销、无池化锁/饥饿问题，符合"性能优先"；
- 缓冲区固定大小让 relay 循环简单、无增长路径、无内存碎片。

### 后果（**必须记录：与 100MB@C10K 指标冲突**）

| 项 | 8KB×2 预分配 |
|---|---|
| 每连接缓冲 | 16,384 B |
| 每连接元数据（target） | ≈1.2–1.5 KB（task + 会话状态 + 注册表项） |
| 每连接合计 | **≈17.5–18 KB** |
| 10,000 连接仅缓冲 | **160 MiB**（167.8 MB） |
| 10,000 连接合计（含固定开销 ≈18 MB、统计 ≈4 MB） | **≈197–200 MB** |
| 100 MB 预算可承载连接数 | **≈4,400–4,700**（而非 10,000） |

**结论：ADR-005 与"100MB + C10K 同时成立"在数学上互斥。** 可选方案：

| 方案 | 做法 | 10K 连接内存 | 是否满足 ADR-005 | 状态 |
|---|---|---|---|---|
| **A** | 固定 8KB×2 预分配 | ≈197 MB | ✅（严格） | 备选：若未来不允许改 bufferSize 则回到 A |
| **B（已裁决✅）** | 默认 8KB×2 预分配；`bufferSize` 可配置（2048/4096/8192，默认 8192）；100MB 达标压测用 2048 | 8K→≈197MB；4K→≈117MB；2K→**≈77MB** | ✅（默认仍预分配 8K） | **执行此方案** |
| **C（池化）** | 8KB 缓冲从全局池按需借还 | ≈80–100MB（池上限决定） | ❌（违反"提前分配"） | 已否决 |

**已裁决（2026-10-26，用户二次确认）：采用方案 B。** 具体要求：

1. `bufferSize` 为**配置项**（也可通过 cargo feature 固定），取值 2048/4096/8192，默认 **8192**；启动时校验并打印实际值。
2. 缓冲仍在**会话建立时一次性预分配**，运行期零分配；`bufferSize` 只影响单块大小，不改变"预分配"语义。
3. 验收必须跑两组口径：
   - **性能基准**：`bufferSize=8192` → 10K 连接 RSS ≈197–200 MB（不要求 ≤100MB，用于吞吐/延迟基线）；
   - **内存达标**：`bufferSize=2048` → 10K 连接 RSS ≈77 MB（≤100MB 达标，用于 G1/G3 验收）。
4. 默认发布配置建议 `bufferSize=8192`（性能优先）；**部署在 100MB 内存机器时必须在配置中设为 2048**，并在文档/启动日志显著提示。
5. `4096` 作为中间档位保留（≈117 MB，只在内存稍宽裕时使用）。

> 注：原 06 §3 的"每连接 ≤2.5 KB + 池化缓冲"方案与 ADR-005 冲突，已作废；06 已按本 ADR 重算。

---

## ADR-006 · ConnectTestHttps：SSL 探测对齐 C++ 语义

### 背景
C++ 版 `ConnectTestHttps`（712 行）是"最能证明上游真的能出网"的探测：经上游 SOCKS5 建立到 `testRemoteHost:testRemotePort` 的隧道 → TLS 握手（**强制校验证书与主机名**，Windows 额外加载系统 `ROOT` store）→ 发 `GET <path>` + `Host` → 读完整响应 → 记录总耗时。成功判定**只看能否完成一次 HTTP 往返，不看状态码**（见 [04](04-control-monitor.md) §3.3）。用户要求 SSL 部分对齐，且尽量用成熟库。

### 决策
1. **探测链路完全对齐**：`TCP connect 上游 → SOCKS5 握手(含认证) → CONNECT 目标 → TLS 握手(SNI + 证书校验) → HTTP/1.1 请求 → 读响应 → 记录 RTT`。
2. **证书校验强制**：不允许配置关闭（与 C++ `need_verify_ssl=true` 硬编码一致）。
3. **信任库用系统根证书**：
   - 首选 `rustls-platform-verifier`（自动使用平台验证器：Windows CryptoAPI / Linux ca-certificates / macOS），**行为最接近 C++ 在 Windows 上加载 `ROOT` store**；
   - 备选 `rustls-native-certs`（读系统 CA 文件）+ `webpki-roots` 兜底；
   - 不使用自签/忽略校验的实现。
4. **失败判定**：TLS 握手失败、证书错误、HTTP 往返失败、超时 → 该次探测失败（fail-fast，不再重试，见 ADR-002）。
5. **判定与展示**：默认**不校验状态码**（对齐 C++）；`healthCheck.target.expectStatus` 可选配置白名单（为空/缺省=不校验）。请求路径默认 `/`（C++ 为 `R"(\)"`，疑似笔误，见 04 Q3；**Rust 版修正为 `/`**，并记录为有意偏离）。
6. **超时**：建连/写/读分别由 `healthCheck.connect.timeout` 控制（默认 11 s，对齐 C++ 的 `maxDelayTime=11` 量级），整体探测用 `tokio::time::timeout` 包裹。
7. **用成熟库实现**：`rustls` + `tokio-rustls` + `rustls-platform-verifier` + `hyper`（HTTP/1.1 客户端），**不手写 TLS、不手写 HTTP 解析**；SOCKS5 握手复用上游客户端库（见 ADR-007）。
8. **不复制 C++ 的本地临时 HTTP 服务/证书逻辑**（那部分是 C++ 为探测自建的辅助设施）；Rust 版直接作为 HTTP 客户端发请求，语义等价、代码量大减。

### 后果
- 依赖增加 `rustls`/`tokio-rustls`/`rustls-platform-verifier`/`hyper`（体积与编译时间上升），但完全避免自研 TLS/HTTP。
- 证书校验失败会正确地把后端判为不可用（与 C++ 一致）。
- 探测目标可选（ADR-002 第 5 条按 U4 裁决放宽）：不配 target 时只做 TCP liveness，启动不失败。

---

## ADR-007 · 优先使用成熟库，最小化自研

### 决策
**默认使用成熟 crate；只有"库的抽象与'透传/嗅探'模型不匹配"时才自研，且必须在此登记原因。**

### 依赖清单（固定）

| 领域 | crate | 用途 | 备注 |
|---|---|---|---|
| 异步运行时 | `tokio`（rt-multi-thread/net/time/sync/io-util/signal/macros） | 全部异步 I/O、任务、定时器 | 双平台唯一合理选择 |
| 网络工具 | `tokio-util`、`socket2` | 取消令牌、`SO_*` 选项、`TcpStream` 精细控制 | `SO_REUSEPORT`(Linux)/`TCP_NODELAY`/缓冲 |
| 配置 | `serde`、`serde_json` | `config.json` 解析 | 保持字段兼容 |
| 日志 | `tracing`、`tracing-subscriber`、`tracing-appender` | 结构化日志、级别过滤、异步写 | 编译期 `max_level_info` |
| 控制面 HTTP 服务 | `hyper` 1.x + `hyper-util` + `http` + `http-body-util`（或 `axum`） | 4 个端点的 HTTP 服务 | 选 `hyper` 以控制依赖树；`axum` 备选 |
| HTTP 解析（入站嗅探） | `httparse` | HTTP CONNECT / absolute-URI 请求行与头解析 | 零拷贝、成熟、无 async |
| 出站 TLS | `rustls`、`tokio-rustls`、`rustls-platform-verifier` | 健康探测的 TLS | 见 ADR-006 |
| HTTP 客户端（探测） | `hyper`（client http1 handshake） | 发送探测请求、读响应 | 不引入 `reqwest`（其 connector 定制成本高） |
| SOCKS5 上游客户端 | `tokio-socks` | 上游握手（含用户名密码认证）、返回已建连 `TcpStream` | **需验证**：自定义 resolver/超时/`slowImpl` 兼容；若不满足则退回"A 类例外"自研 |
| DNS | `tokio::net::lookup_host` + 自管刷新（`dnsTtl`） | 上游域名解析、按 TTL 原地刷新；IP 字面量跳过 | 未引入 hickory-resolver（依赖与复杂度权衡；`dnsTtl` 默认不启用） |
| 分配器 | `mimalloc`（`libmimalloc-sys`/`mimalloc` crate） | 全局分配器 | 降低多线程竞争与碎片 |
| 锁/快照 | `std::sync::Mutex`（非热路径） | 选择器状态、hint 表、上游地址刷新 | 热路径仍用原子；未引入 parking_lot/arc-swap |
| 随机/抖动 | `fastrand` | 探测错峰、退避抖动 | 线程本地，无全局锁 |
| CLI | `clap`（derive） | `-c/--config`、`--help`、`--version` | 与 C++ 参数兼容 |
| 错误 | `thiserror`（边界）、手写 `enum`（热路径） | 错误类型 | 热路径不分配 |
| 信号 | `tokio::signal` | SIGINT/SIGTERM/Windows Ctrl-C | — |

### 允许自研的例外（仅 3 处，必须登记）
| 例外 | 原因 | 规模上限 | 保障 |
|---|---|---|---|
| 首包嗅探/协议分派（读首包 → 判 SOCKS4/5/HTTP/其他） | 需要"读一点、判定、把字节原样回放"的透传语义，现有库都是"终结式"协议栈，无法满足 | ≤300 行 | 单元测试 + `cargo-fuzz` |
| SOCKS5 **入站**方法协商与 RFC1929 认证校验 | 同上（终结式库会自己建连目标，不适用于"连上游代理"）；且 ADR-004 要求 0x00/0x02 精确行为 | ≤400 行 | 字节级测试 + fuzz |
| HTTP CONNECT 的"解析后接管连接"（提取 host:port 后转交 relay） | `hyper` 的 upgrade 语义可做但更绕；`httparse` 解析 + 接管字节流更直接 | ≤150 行 | 测试 + fuzz |

其余（TLS、HTTP 客户端/服务端、DNS、SOCKS5 上游客户端、HTTP 头解析、JSON、日志、配置、CLI、分配器）**一律用库**。

### 后果
- 依赖数量上升，但自研代码集中在"协议嗅探"这一真正特殊的地方，总量目标 ≤1,000 行。
- 需要一次 `tokio-socks` 的能力验证（见 §6 任务），若不满足 ADR-004 的上游认证/slowImpl 兼容，则把该处升级为例外并记录。

---

## ADR-008 · 编码与工程规范

1. 源码、配置、文档统一 **UTF-8（无 BOM）**、LF 换行；注释中文、标识符英文。
2. CI 增加编码检查（`grep -rP '\r'`、BOM 检测、`file -i` 断言）。
3. 所有涉及网络输入的解析必须"先校验长度后索引"，禁止 `unwrap()`/`panic!`（fuzz 目标：随机字节流零 panic）。
4. 提交前 `cargo clippy --all-targets -- -D warnings`、`cargo fmt --check`。
5. 跨平台：Windows + Linux 双 CI 必须通过。

---

## 5. 配置草案（新增/变更字段）

### 5.1 新增字段（全部可选，缺省=保持 C++ 行为）

```json5
{
  // ===== 线程 =====
  "threadNum": 0,                 // 最大 worker 线程数；0=自动(核数)。沿用 C++ 字段
  "workerThreads": 0,             // 精确指定 worker 数；0=不指定。新增
  "maxBlockingThreads": 2,        // spawn_blocking 线程上限。新增
  "threadStackSize": 524288,      // 线程栈字节数，默认 512KB。新增

  // ===== 转发缓冲（ADR-005 / U1=方案 B）=====
  "bufferSize": 8192,             // 单方向缓冲字节数：2048/4096/8192，默认 8192；100MB 达标填 2048。会话建立时预分配

  // ===== 控制面 =====
  "stateServerHost": "127.0.0.1",
  "stateServerPort": 5110,
  // 无鉴权；默认仅回环。新增：响应体上限（防意外大 JSON）
  "stateMaxResponseBytes": 65536,

  // ===== 健康检查（新增；旧字段仍兼容，见 5.2）=====
  "healthCheck": {
    "tcp":     { "period": 5000,  "timeout": 2000,  "startDelay": 1000 },
    "connect": { "period": 300000,"timeout": 11000, "startDelay": 1000 },
    "target": {
      "host": "www.google.com",   // 可选：不配 target 则只做 TCP liveness
      "port": 443,
      "sni": "",                  // 空=用 host
      "httpPath": "/",            // C++ 为 "\"（笔误），Rust 修正
      "userAgent": "Socks5BalancerRust/0.1",
      "expectStatus": []          // 空=不校验状态码（对齐 C++）
    },
    "failFast": true,             // 固定 true；不存在 retry/失败阈值字段
    "downBackoff": { "initial": 30000, "multiplier": 2.0, "max": 600000, "jitter": 0.2 },
    "probeJitter": 0.2,           // 常规探测错峰
    "maxProbeConcurrency": 4,
    "connectFailMarkDegraded": true // 数据面 connect 失败即降级（快速切换）
  },

  // ===== 检查时序的旧字段（保留；默认值与 C++ README "Full configuration" 一致）=====
  // 它们是 healthCheck.tcp/connect.* 的同义旧字段，新字段优先。
  "tcpCheckStart": 1000,          // = healthCheck.tcp.startDelay（启动后首轮探测延迟）
  "connectCheckStart": 1000,      // = healthCheck.connect.startDelay
  "additionCheckPeriod": 10000,   // 全池不可用且未休眠时加速探测
  "sleepTime": 1800000,           // 30min 无新连接 → 暂停全部自动探测（0 = 关闭休眠）
  "disableConnectionTracker": false, // 旧字段：true = 关闭连接监测

  // ===== SOCKS5 入站 =====
  // 注意：这里曾计划 `socks5: { enableNoAuth, enableUserPass, allowCommand }` 三开关，
  // **实际未落地**——入站行为固定按 ADR-004：方法只接受 0x00/0x02，CMD 只接 CONNECT
  // （BIND/UDP 回 0x07）。若将来要开放，再引入该节并同步迁移说明。
  "disableSocks4": true,          // 默认 true（对齐 C++）：默认禁用 SOCKS4/4a，
                                  // 避免实现不正确的陈旧客户端；需要兼容时显式写 false
  "disableHttpForward": false     // 禁用 HTTP absolute-form 正向代理；CONNECT 不受影响
}
```

### 5.2 旧字段兼容映射

| 旧字段（C++ config.json） | 新语义 |
|---|---|
| `threadNum` | 最大 worker 线程数（`min(核数, threadNum)`） |
| `tcpCheckPeriod` / `tcpCheckStart` | `healthCheck.tcp.period/startDelay`（`startDelay` 默认 1000ms，启动后首轮探测延迟） |
| `connectCheckPeriod` / `connectCheckStart` | `healthCheck.connect.period/startDelay`（`startDelay` 默认 1000ms） |
| `additionCheckPeriod` | 保留并**默认生效（10000ms）**：全池不可用**且不在 `sleepTime` 休眠中**时加速探测 |
| `sleepTime` | 保留并**默认生效（1800000ms）**：最近该时长内无新连接即**暂停全部自动探测**（防规律流量被防火墙拉黑）；显式 `0` 关闭 |
| `serverChangeTime` | 保留（`change_by_time` 周期）；Rust 默认 **60000ms**（C++ 为 5000ms，见迁移差异） |
| `stateServerPort` | 保留；Rust 默认 **5110**（C++ 为 5010，避免与本机其它实例冲突） |
| `disableConnectionTracker` | 映射为 `connectionTracking.enabled=false`（`connectionTracking.enabled` 优先） |
| `disableSocks4` | 保留；**默认 true**（对齐 C++，默认禁用 SOCKS4/4a） |
| `testRemoteHost` / `testRemotePort` | `healthCheck.target.host/port`（若新旧同时存在，新字段优先） |
| `disableConnectTest` | `true` 时等价 `healthCheck=false`（仅保留手动禁用判定） |
| `connectTimeout` | 上游 connect 超时（**Rust 必须真正使用**） |
| `internalBehavior.*` | 仅其 `connectTimeout` 在顶层缺省时作兜底；其余字段忽略并提示 |

> **兼容策略**：旧字段全部可读；新字段优先；两者冲突时打 warning 日志。

---

## 6. 决策对硬指标的重新评估

| 指标 | 决策后 | 判定 |
|---|---|---|
| 10K 连接内存 | **方案 B**：`bufferSize=8192` → ≈197 MB；`4096` → ≈117 MB；`2048` → **≈77 MB** | 8K 档 ❌ / 2K 档 ✅ |
| 1,000 conn/s | 健康检查改为 fail-fast 后不增加建连路径成本；DNS 缓存 + 无锁选路 + 精简控制面 | ✅ 可达 |
| 2~4 core | 线程数可配置 + 单 runtime | ✅ |
| 健康检查时效 | 坏后端：数据面失败即时降级 + 一次探测确认（≤ 一个 `tcp.period`） | ✅ 明显优于 C++ 的 5 分钟 |
| 恢复正常后端的探测频率 | 退避 30s 起、上限 10min，可配置 | ✅ 不打断自恢复 |

**已裁决（用户二次确认）**：采用**方案 B**——默认 8K 保持性能；`bufferSize` 做成配置项（2048/4096/8192）；100MB 达标压测使用 2048，性能基准使用 8192；两组口径都写入验收脚本。

---

## 7. 裁决状态 / 待确认

**已裁决（2026-10-26 第二轮）**：

| # | 事项 | 裁决结果 |
|---|---|---|
| U1 | ADR-005 的 A/B/C 方案 | **方案 B**：默认 8K×2 预分配；`bufferSize` 可配 2048/4096/8192；100MB 达标用 2048（≈77MB），性能基准用 8192（≈197MB） |
| U2 | 是否复用原 `html/` 前端 | **不复用**：控制面仅 4 个 `/api/v1/*` 端点，不实现旧 `/` `/op` 兼容层；后续如需页面另写极简静态页 |
| U3 | `bufferSize` 是否可配 | **可配**（随 U1 方案 B 生效，默认 8192） |

**已裁决（2026-10-26 第三轮）**：

| # | 事项 | 裁决结果 |
|---|---|---|
| U7 | 是否交付 Docker / 容器化部署 | **暂不实现**：M5 只交付"二进制 + systemd 单元 + 迁移说明"；仓库保留 `Dockerfile` / `.dockerignore`，但**不维护、未验证、不纳入验收**，文档不再提供容器部署步骤（见 [13 §4](13-m5-deployment.md)）。将来启用需重新验证构建（`--locked` 依赖、运行镜像精简、非 root）、控制面/监控面绑定与端口发布、`HEALTHCHECK`，并补回部署步骤 |

**仍待确认（不阻塞开工，可在 M0/M1 期间按默认执行）**：

| # | 事项 | 默认取值 | 影响 |
|---|---|---|---|
| U4 | 健康探测 `target.host` 是否允许为空（只做 TCP 探活） | **实现取"允许为空"**：不配 target 则只做 TCP（ADR-002 #5 的"必填"未强制执行，部署便利性优先；若需强制可后续加校验开关） | 部署便利性 |
| U5 | 是否需要 `reload_config` 热重载 | **M0–M4 不做**，后续按需 | 可控性 vs 复杂度 |
| U6 | 是否保留 SOCKS4/4a | **保留**（ADR-004 只约束 SOCKS5 方法） | 兼容性 |

---

## 8. 对其它文档的同步记录

| 文档 | 已按本 ADR 修订的内容 |
|---|---|
| [01-overview.md](01-overview.md) | §9.4 指标差距表按 ADR-005 重算；新增"决策后状态"注记 |
| [02-relay-core.md](02-relay-core.md) | §3.2 缓冲策略改为"8K×2 预分配"；§7/§8 的"降到 2–6 KB"结论作废并重写 |
| [03-protocol.md](03-protocol.md) | §11.2.2 内存模型按 ADR-005 重写（作废原"池化/4KB"方案）；§11.5 验收清单加 ADR-004 注记（多方法用例是必做项）；C++ 现状分析不变 |
| [04-control-monitor.md](04-control-monitor.md) | §14.4 控制面改为精简无鉴权；§14.6 健康检查改为 fail-fast+降频；§14.8 内存预算按 8K 重算；§14.9 验收标准同步 |
| [05-build-config-deploy.md](05-build-config-deploy.md) | §10.4 运行时旋钮改为 8K×2 + 线程配置；§10.2 依赖清单补 rustls/hyper/httparse 等 |
| [06-rust-port-assessment.md](06-rust-port-assessment.md) | §3 预算按 8K 重算；§5 选型按 ADR-007；§7.4 健康检查按 ADR-002；§8 控制面按 ADR-001；§9.2 双口径验收；§12 决策状态更新（U1=B、U2=不复用、U3=可配） |
| [README.md](README.md) | 索引新增本文；§3.2 可行性表与 §5.4 决策点同步 |

### 2026-10-26 第二轮（U1/U2/U3 裁决后的二次同步）

| 文档 | 二次同步内容 |
|---|---|
| 本文 | ADR-005 状态改为"方案 B 已裁决"；§7 改为"裁决状态"；ADR-001 的 U2 由待确认改为已裁决；§5.1 配置草案新增 `bufferSize` |
| [01-overview.md](01-overview.md) | §9.4 指标行改为"默认 8K 性能基线 / 2K 达标 100MB" |
| [02-relay-core.md](02-relay-core.md) | §3.2 方案 B 标记为已裁决；`bufferSize` 语义由"编译期常量"改为"启动配置项"（§3.3/§8.2/§8.4） |
| [03-protocol.md](03-protocol.md) | §11.2.2 方案表标记 B 已裁决 |
| [04-control-monitor.md](04-control-monitor.md) | §14.8 冲突处置改为"已裁决方案 B" |
| [05-build-config-deploy.md](05-build-config-deploy.md) | §8.4 末行、§10.4 缓冲语义改为"启动配置项 `bufferSize`（2048/4096/8192）" |
| [06-rust-port-assessment.md](06-rust-port-assessment.md) | §3.3/§7.2/§9.2/§12/§14 全部改为方案 B 口径；U2 明确不复用前端 |

> 一致性口径（全文统一）：`bufferSize` ∈ {2048, 4096, 8192}，默认 **8192**；**启动时读取**（非热更新、非每连接切换）；会话建立时一次性预分配 2 块。10K 连接内存：8192→≈197–200 MB（性能基线），4096→≈117 MB，2048→**≈77 MB（100MB 达标）**。
