# Socks5BalancerAsio → Rust 移植：功能与架构分析文档集

> 本目录是对 C++ 项目 **Socks5BalancerAsio** 的完整功能与架构分析成果，也是 `Socks5BalancerRust` 移植工程的**唯一事实基线**。
>
> 所有文档：简体中文、UTF-8（无 BOM）、纯 LF；所有结论均可在原始 C++ 源码中按 `src/xxx.cpp:行号` 复核。

---

## 1. 分析对象

| 项 | 值 |
|---|---|
| 项目 | `Socks5BalancerAsio`（C++20 / Boost.Asio / Boost.Beast / Boost.Log / OpenSSL） |
| 仓库 | `H:\Code\CLionProjects\Socks5BalancerAsio` |
| 快照 | commit `5b4e957`（tag 描述 `v1.6.0-7-g5b4e957`，master，2025-08-27） |
| 规模 | `src/` 下 59 个文件（26 `.cpp` / 30 `.h` / 1 `.hpp` / 2 `.in`），**11,308 行** C++ |
| 许可证 | GPL v3 |
| 分析方式 | 全量源码静态阅读 + 调用链追踪 + `sizeof`/堆分配/进程内存**实测探针**；未运行 C++ 程序、未做压测 |
| 源文件编码 | 57 个 UTF-8；**`src/TcpRelaySession.cpp` 与 `src/ProxyHandshakeUtils/HttpServerImpl.cpp` 为 GB2312/GBK**（已转码镜像，见 §7） |

---

## 2. 文档清单

| # | 文档 | 内容 | 规模 |
|---|---|---|---|
| 0 | **README.md**（本文） | 索引、结论摘要、缺陷 Top 榜、阅读指引 | — |
| 1 | [01-overview.md](01-overview.md) | 项目定位、功能清单、总体架构、组件职责、连接生命周期主流程、三种工作模式、所有权模型、架构评价 | ~570 行 |
| 2 | [02-relay-core.md](02-relay-core.md) | **高性能转发架构（数据面）**：accept 层、会话/缓冲策略、relay 循环语义、握手阶段屏障、上游建连路径、每包开销、瓶颈排序、Rust 热路径设计约束、等价性验收清单 | ~575 行 |
| 3 | [03-protocol.md](03-protocol.md) | **协议处理**：两条互斥产品线（`FirstPackAnalyzer` vs `ProxyHandshakeAuth`）、首包嗅探与回放、SOCKS5/4/4a/HTTP 状态机、上游 SOCKS5 客户端握手、认证、异常与生命周期风险、字节级时序图、65 项验收清单、39 条缺陷索引 | ~2,480 行 |
| 4 | [04-control-monitor.md](04-control-monitor.md) | **运行控制与监控**：14 个定时器体系、健康状态机、上游选择算法、统计聚合、连接追踪、延迟采样、状态/控制 HTTP 全端点表、静态服务、日志、优雅关闭、24 条缺陷 | ~1,620 行 |
| 5 | [05-build-config-deploy.md](05-build-config-deploy.md) | **构建、配置、部署与资源开销**：CMake 开关矩阵、配置字段字典（含死字段）、Docker/CI/前端、**每连接内存实测（22,039 B）**、1,000 conn/s 关键路径开销分解 | ~1,040 行 |
| 6 | [06-rust-port-assessment.md](06-rust-port-assessment.md) | **硬指标可行性评估与 Rust 架构映射**：指标口径、内存/CPU 预算模型、crate 选型、模块划分、数据面约束、验收压测方案、里程碑、风险登记表 | ~345 行 |
| 7 | [07-decisions.md](07-decisions.md) | **架构决策记录（ADR）**：8 条已决策 + 配置草案 + 旧字段兼容映射 + 指标重算 + 裁决记录（U1=方案 B、U2=不复用前端已裁决；U4–U6 待确认）。**与其他文档冲突时以此为准** | ~380 行 |
| 8 | [08-m0-milestone.md](08-m0-milestone.md) | **M0 里程碑报告**：骨架实现范围、验收结果、实测 RSS/连接数、踩坑与修正、已知限制、如何运行、M1 计划 | ~240 行 |
| 9 | [09-m1-milestone.md](09-m1-milestone.md) | **M1 里程碑报告**：SOCKS5 0x00/0x02、上游握手、5 条选路规则、健康检查（fail-fast + 退避 + 数据面降级）、TLS readiness、验收结果 | ~200 行 |
| 10 | [10-m2-milestone.md](10-m2-milestone.md) | **M2 里程碑报告**：首包嗅探、SOCKS4/4a、HTTP CONNECT / absolute-form、early data 回放、origin-form 改写、验收结果 | ~180 行 |
| 11 | [11-m3-milestone.md](11-m3-milestone.md) | **M3 里程碑报告**：精简控制面 4 端点、控制命令、安全默认、全量回归 | ~200 行 |
| 12 | [12-m4-performance.md](12-m4-performance.md) | **M4 性能与内存基线**：Windows 实测（2048/4096/8192 × 1000 连接）、每连接开销校准、10K/100MB 推算、Linux 压测脚本 | ~150 行 |
| 13 | [13-m5-deployment.md](13-m5-deployment.md) | **M5 部署与迁移说明**：Dockerfile、systemd、生产配置、从 C++ 迁移的字段兼容/行为差异/灰度回滚、系统调优、监控与安全、上线检查清单 | ~230 行 |

阅读顺序建议：**01 → 02 → 03 → 04 → 05 → 06 → 07 → 08 → 09 → 10 → 11 → 12 → 13**。
只想快速决策：读本文 §3/§4/§5；准备动手写代码：读 **07（决策）+ 02 §8 + 06 §7**。

> **当前进度（2026-10-26）**：**M0 ✅；M1（M1.1–M1.5）✅；M2 ✅；M3 ✅；M4 内存基线 ✅；M5 交付物 ✅（待 Linux 实测回填）**。已实现：SOCKS5 入站 0x00/0x02（RFC1929、CONNECT、错误码回传）、上游 SOCKS5 握手（含上游认证）、5 条选路规则、fail-fast 健康检查（DOWN 指数退避 + 数据面降级 + 快速重探）、TLS + HTTP readiness 探测、混合端口（SOCKS5 / SOCKS4-4a / HTTP CONNECT / HTTP forward）、精简控制面（4 端点、运行期启停/改规则/立即探测、无鉴权仅回环、响应不含密码）。端到端验收：协议 10/10、健康检查 6/6、TLS readiness 9/9、混合协议 9/9、控制面 19/19；单元测试 **90** 个全绿；**内存实测：`bufferSize=2048` 下 10K 空闲连接推算 ≈ 98.5 MB（Windows 口径，临界达标；Linux 预计 ~69 MB）**。交付物：Dockerfile、systemd 单元、生产配置、`.github/workflows/ci.yml`（Linux/Windows 双平台 CI）、Linux C10K 压测脚本。**下一步：Linux C10K/1K 实测、24h 长稳、用实测回填 12 号文档**。详见 [13-m5-deployment.md](13-m5-deployment.md)。

> **2026-10-26 修订**：用户已就 7 项关键取舍作出决策（控制面精简不鉴权、健康检查 fail-fast 不重试且异常后端降频、线程数可配置、SOCKS5 仅 0x00/0x02、双向 8K 预分配、TLS 探测对齐 C++、优先成熟库），已固化为 [07-decisions.md](07-decisions.md)，并同步修订 01/02/04/05/06 的"Rust 建议"部分。C++ 现状分析部分不变。

---

## 3. 一页结论

### 3.1 项目本质

> **一个"多上游 SOCKS5 代理之间的 TCP 负载均衡器 + 健康检查器 + 混合协议入口"**，本身不是代理服务器，而是**带首包识别能力的 TCP 中继**。

两个决定性的设计事实（必须先理解）：

1. **SOCKS5 下游是"透传"**：首包是 `05 01 00` 时，程序只做字节搬运，真正的 SOCKS5 协商发生在"客户端 ↔ 上游代理"之间（热路径极短，这是它能承载多连接的基础）。
2. **HTTP 下游是"协议翻译"**：程序解析出目标 `host:port`，自己向**上游**发起 SOCKS5 客户端握手（含 CONNECT），再把字节双向搬运。

### 3.2 硬指标可行性判定

| 指标 | C++ 现状 | 判定 | Rust 可行性（按 [07](07-decisions.md) 决策） |
|---|---|---|---|
| 运行时内存 ≤100 MB | 实测 **22,039 B/连接** → 10,000 连接 ≈ **210 MiB**（超 110%） | ❌ 不可达 | ✅ **ADR-005 + U1=方案 B**：默认 `bufferSize=8192` 保持双向 8K 预分配（10K ≈197–200 MB，性能基线）；`bufferSize` 可配 2048/4096/8192，**100MB 达标压测用 2048（Windows 10K 实测推算 ≈98.5 MB，临界达标；Linux 预计 ~69 MB）** |
| 持有 C10K 连接 | 仅缓冲就 160 MB；另有"建连不发首包"的**永久泄漏**（无超时 + 引用环） | ❌ 不可达 | ✅ 功能可达；内存按 `bufferSize` 双口径验收（8K 性能基线 / 2K 达 100MB）；全链路超时消除泄漏 |
| 1,000 连接/秒 | 每连接一次**无缓存 DNS**、同步无过滤日志、~6–7 次全局统计锁、accept 串行、无 connect 超时 | ❌ 高风险 | ✅ 预算 2 ms/连接（2 核），改造后实际约 10–30 µs |
| 2~4 core | 单 `io_context`、无分片、统计/日志全局锁；`min(核数, threadNum) ≤ 2` 时退化为单线程 | ⚠️ 多核利用差 | ✅ per-core accept（REUSEPORT）+ 分片指标 + 线程数可配（ADR-003） |
| 健康检查时效 | 应用层探测默认 5 分钟、依赖 Google、恢复慢 | ❌ 不符合"快速切换" | ✅ fail-fast + 数据面失败即时降级 + DOWN 指数退避（ADR-002） |
| Windows + Linux | 双平台已支持 | ✅ | ✅ tokio 双平台 |

> 一句话：**功能可以 1:1 对齐，架构不能 1:1 翻译。**

---

## 4. 最关键发现（Top 14）

| # | 发现 | 证据 | 对移植的意义 |
|---|---|---|---|
| K1 | **协议处理有两条互斥的产品线**，由 `Need_ProxyHandshakeAuth` 编译期宏切换：默认 OFF 走"首包嗅探 + 透传"（不支持认证、不支持 SOCKS4）；ON 才是完整协议栈。作者本人的 `CMakeSettings.json` 用的是 **ON** | `CMakeLists.txt:124,289-292`；`src/TcpRelaySession.h:39-47,87-91` | Rust 应**合并为一条路径**，用配置表达差异 |
| K2 | **协议路径完全没有超时**（`connectTimeout` 被解析但全仓无使用；`internalBehavior` 整节无解析代码），叠加"挂起 handler 持有 `shared_ptr<TcpRelaySession>` + session 持有 `shared_ptr<FirstPackAnalyzer>`"的**引用环** → **"建连不发首包"的连接永久泄漏** | `src/FirstPackAnalyzer.cpp:158-166`；`src/TcpRelaySession.h:88`；`src/ConfigLoader.cpp:223-224` | **100MB@C10K 的头号障碍**；Rust 必须全链路超时 + task 取消 |
| K3 | **每连接固定 2×8KB 缓冲常驻**，实测 `sizeof(TcpRelaySession)`=17,296 B；加上 strand/分析器/统计共 **22,039 B/连接** | `src/TcpRelaySession.h:79-83`；[05](05-build-config-deploy.md) §8 | 必须改为懒分配 + 全局池 |
| K4 | **每包统计要拿 2~4 次全局 `recursive_mutex` + 4 次 `std::map` 查找**（每轮双向转发 6~8 次） | `src/TcpRelaySession.cpp:507-529`；`src/TcpRelayStatisticsInfo.h:187` | 分片原子计数，移出热路径 |
| K5 | **每连接一次 DNS 解析且无缓存**（每连接一个 `resolver`，只取 `results.begin()`，无 connect 超时） | `src/TcpRelaySession.cpp:144-186` | 启动解析 + TTL 缓存 + `SocketAddr` 直连 + 多地址尝试 |
| K6 | **日志是同步 sink 写 stdout、无 severity 过滤**，trace 全量输出；每连接约 8~15 条 | `src/log/Log.cpp:121-181` | `tracing` + 编译期 max level；热路径零格式化 |
| K7 | **协议帧校验普遍用"字节数必须精确相等"**，粘包/拆包/early data/应答与数据同包都会失败 | [03](03-protocol.md) §9.4、附录 B（S-4/C-2/B-2/B-3） | 用"边界解析 + 剩余字节回填" |
| K8 | **`ConnectionTracker` 是占位实现**（`analysisData` 只要 ≥3 字节就 `isEnd`），且 `up_count/down_count` 无读取方 → 死代码层 | `src/ConnectionTracker.cpp:96-128` | 直接删除该层（**不是**性能瓶颈，之前"80KB/包拷贝"的判断已复核修正） |
| K9 | **状态/控制 API 全为 GET、无鉴权、CORS 反射任意 Origin，且 `/` 明文返回客户端密码** | `src/StateMonitorServer.cpp:281-356` | Rust 版默认仅绑定 127.0.0.1；可选 token；不返回密码 |
| K10 | **健康检查单次失败即 DOWN，无去抖；应用层恢复最长 5 分钟**；默认依赖 `www.google.com:443` 且强制 TLS 校验 | `src/UpstreamPool.cpp:337-374,472-521`；`src/ConnectTestHttps.h:162` | 按 ADR-002：保留 fail-fast、**不做重试/去抖**；数据面失败即时降级；DOWN 后指数退避降频；探测目标与时间全部可配置 |
| K11 | **按 client IP 建统计 map 永不淘汰**；每次 `/` 请求做 O(总连接数) 清理 | `src/TcpRelayStatisticsInfo.cpp:25-84` | 固定容量 + 淘汰；明细采样 |
| K12 | **accept 全局串行**（回调里才续接下一个 accept）且 **EMFILE 会自旋**；全仓库**无 `TCP_NODELAY`**、无 socket 缓冲调优 | `src/TcpRelayServer.cpp:66-105`；socket 选项 grep | 多 accept + 退避 + `TCP_NODELAY` + 显式 socket 缓冲 |
| K13 | **远程可触发的未定义行为/紧循环**：HTTP `Proxy-Authorization` 短值触发 `size()-6` 下溢；407 后未消费请求导致单连接烧满一核（仅在"增强模式 + 认证"下可达） | `src/ProxyHandshakeUtils/HttpServerImpl.cpp:224,240,385-422` | fuzz 必须零 panic；先校验后索引 |
| K14 | **构建链红灯**：`CMakeLists.txt` 要求 Boost 1.89，但 CI 装 1.81、Docker Alpine 是 1.82；`azure-pipelines.yml` 是 trojan 项目复制残留 | `CMakeLists.txt:102`；`Dockerfile:4`；`.github/workflows/*.yml:128` | Rust 侧无此问题，但说明"官方构建未必可复现" |

> 完整缺陷清单分散在各篇：03 附录 B（39 条）、04 §13（24 条）、05 §8.6（每连接浪费点）、02 §7（瓶颈排序）。

---

## 5. 给实现者的"必做 / 可不同 / 可砍"

### 5.1 必须等价保留
混合端口（SOCKS5 透传 / SOCKS4/4a / HTTP CONNECT / HTTP forward）、5 条上游选择规则 + hint 粘性、双重健康探测 + 死上游隔离 + 自动恢复 + 全下线保命、`config.json` 字段兼容、状态 JSON + 控制指令、双向转发语义（背压/关闭/统计口径）、Windows + Linux 双平台。

### 5.2 实现必须不同
- 执行模型：tokio task 替代"每连接 strand + 回调链"，线性 `async fn` 持有连接所有权；
- 内存模型：**缓冲懒分配 + 全局池 + 硬上限**，空闲连接近零缓冲；
- 协议实现：两条产品线合并为一条，配置驱动；
- 统计：分片原子 + 离线聚合，禁止热路径加锁；
- DNS：启动解析 + TTL 刷新，热路径零解析；
- 超时：connect / 首包 / 握手 / 空闲**全链路**超时；
- 日志：`tracing` + 编译期裁剪 + 异步 writer。

### 5.3 可以砍或降级
`ConnectionTracker` 整层、`EmbedWebServer` 与静态站点、原 `html/` 前端兼容层（U2 已裁决不做）、`internalBehavior` 死配置，以及 `ConnectTestHttps` 的自建 HTTP 服务/证书设施——**但 TLS 探测本身必须保留并对齐 C++ 语义**（ADR-006），只是改用 `rustls` + `hyper` 实现，代码量大幅下降。

### 5.4 已决策事项（2026-10-26）与仍需裁决项

**已决策**（详见 [07-decisions.md](07-decisions.md)，与本文冲突时以 07 为准）：

| ADR | 决策 |
|---|---|
| ADR-001 | 控制面精简：4 个端点、**不做鉴权**、不返回凭据、默认仅回环 |
| ADR-002 | 健康检查**快速失败、不重试/不去抖**；数据面失败即时降级；**异常后端指数退避降频**；检查目标与所有时间可配置 |
| ADR-003 | **最大线程数可配置**（`threadNum` + `workerThreads` + `maxBlockingThreads` + `threadStackSize`），单 runtime |
| ADR-004 | SOCKS5 入站**仅支持 `0x00` 无认证与 `0x02` 用户名密码认证**；其余方法 `05 FF`；CMD 仅 CONNECT |
| ADR-005 | 转发缓冲**双向 8KB、会话建立时提前分配**、运行期零分配 |
| ADR-006 | `ConnectTestHttps` 的 **TLS 探测对齐 C++ 语义**（强制证书校验、系统信任库），用 `rustls`+`hyper` |
| ADR-007 | **优先成熟库**；自研仅限首包嗅探、SOCKS5 入站协商、CONNECT 接管 3 处 |
| ADR-008 | UTF-8 无 BOM、中文注释、clippy/fmt、fuzz 零 panic |

**已裁决（2026-10-26 第二轮）**：U1 = 方案 B（默认 8K；`bufferSize` 可配，100MB 达标用 2048）；U2 = 不复用原 `html/` 前端；U3 = `bufferSize` 可配。**仍待确认（按默认执行，不阻塞）**：U4 探测目标是否允许为空（默认不允许）；U5 配置热重载（默认不做）；U6 是否保留 SOCKS4/4a（默认保留）。

---

## 6. 建议的移植路线（详见 06 §10）

| 阶段 | 目标 | 关键验收 |
|---|---|---|
| M0 骨架 | 配置（含新字段兼容）/日志/runtime（线程可配）/多监听/纯 TCP 透传 | RSS < 5 MB；`sizeof` 实测 task 开销；打印实际 worker 数 |
| M1 核心 | SOCKS5 0x00/0x02 协商 + 5 条规则 + TCP/TLS 健康检查（fail-fast + DOWN 退避）+ 超时/背压/连接上限 | 杀上游 ≤1 个探测周期摘除；10K 空闲 RSS 按 `bufferSize` 口径达标；1K/s ≥99% |
| M2 协议 | SOCKS4/4a + HTTP CONNECT/forward + 上游认证 + TLS 探测对齐（ADR-006） | 协议用例全绿；与 C++ 字节级对比 |
| M3 控制面 | 精简 4 端点（无鉴权）+ 统计快照 | 端点契约测试；响应不含密码 |
| M4 达标 | 压测调优 + 24h 长稳 | 06 §9.2 全部达标（方案 B 双口径：8K 性能基线 + 2K 内存达标） |
| M5 交付 | 文档/部署脚本/迁移说明 | 可替换原版 |

> **强烈建议先做 M0+M1 的"薄垂直切片"**：尽早用 10K/1K 压测暴露内存与调度问题，再补协议与控制面。

---

## 7. 分析资产与可复现性

| 资产 | 路径 | 说明 |
|---|---|---|
| UTF-8 源码镜像 | `.analysis/utf8-src/` | C++ 源码的完整 UTF-8 副本（仅把 2 个 GBK 文件转码，行号与原文件一致）。**只用于阅读与引用，不是移植产物** |
| 内存探针 | `.analysis/sizeof_probe*.cpp` | build-analyst 用 MSVC 14.51 + Boost 1.89 编译的 `sizeof`/堆分配计数/进程内存探针源码；[05](05-build-config-deploy.md) §8 的实测数字由它产生 |
| 行数统计命令 | [05](05-build-config-deploy.md) §12.1 | 可复现源文件规模与宏作用点的命令 |

**复核口径**：所有 `src/xxx.cpp:行号` 均指向 **C++ 原仓库**；GBK 文件的行号取自转码镜像（与原文件逐行一致）。本次分析**未修改任何 C++ 源码**（已用 `git status` + mtime 校验）。

---

## 8. 编码与工程约定（Rust 侧）

- 全部源码、配置、文档 **UTF-8（无 BOM）**，换行 LF；
- 注释用中文，标识符/API 名保留英文；
- 跨平台：Windows + Linux 必须同时通过 CI（已提供 `.github/workflows/ci.yml`：双平台 `fmt`/`clippy`/`test`，Linux 追加 release 构建与 `scripts/ci-linux.sh` 冒烟，另有独立的 UTF-8 无 BOM / LF 编码检查 job）；
- 优先使用成熟 crate（见 [06](06-rust-port-assessment.md) §5.2）；
- 性能敏感函数避免锁与分配，遵循 [02](02-relay-core.md) §8 的约束与 [06](06-rust-port-assessment.md) §7 的 10 条数据面规则。

---

## 9. 已知局限

- 本文集是**静态分析 + 局部实测**，未运行原 C++ 程序、未做端到端压测，也未在 Linux 上复测 `sizeof`（数字来自 Windows/MSVC，Linux/GCC 需复跑探针）。
- 性能结论中的"Rust 预估"是预算推算，**必须在 M0/M4 用真实压测校准**。
- 各篇末尾的「待确认」条目（03 共 10 条、04 共 16 条、05 共 12 条）需通过抓包、实测或与原作者确认后收敛。
- 一处已在复核中纠正的误判：`Socks5ServerImpl.cpp:226` 的"长度校验前索引"**不会崩溃**（缓冲区预分配 8,196 B），已从"极高/远程 DoS"降级为"低/移植陷阱"（见 [03](03-protocol.md) 摘要第 6 条与 U-4）。
