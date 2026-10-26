# 14 · 连接统计的并发设计与无锁 / 消息架构评估

> 日期：2026-10-27
> 范围：`src/conn.rs`（`ConnRegistry`/`ConnMeta`/`ConnGuard`）、`src/stats.rs`（`Stats`/`ActiveGuard`）、
> `tokio-util` 的 `CancellationToken` 子令牌、控制面读取路径（`src/control/mod.rs`）。
> 基准工具：`examples/conn_bench.rs`（本次新增，可回归）。

---

## 1. 现状：数据结构与每连接操作

### 1.1 数据结构

| 结构 | 并发原语 | 争用面 | 每连接操作 |
|---|---|---|---|
| `shards[16]` | `Mutex<HashMap<ConnId, Arc<ConnMeta>>>` ×16 | 按 id 取模分散到 16 片 | 注册 1 锁 + 注销 1 锁 |
| `active` | `AtomicU64` | **全局单原子** | +1（注册）/ -1（注销） |
| `per_upstream[i]` | `Vec<AtomicU64>` | 每上游一个 | `set_upstream` 时 -1/+1 |
| `per_listen` | `Mutex<HashMap<SocketAddr, u64>>` | **全局单锁** | 注册 1 锁 + 注销 1 锁 |
| `per_client` | `Mutex<HashMap<IpAddr, u64>>` | **全局单锁**（+ 达到上限时整体 `clear()`） | 注册 1 锁 + 注销 1 锁 |
| `next_id` | `AtomicU64`（`fetch_add`） | 全局单原子 | 1 次 |
| `detail_count` | `AtomicU64` | 全局单原子 | +1/-1 |
| `ConnMeta` 原子字段 | `protocol/upstream/state/bytes_up/bytes_down` | **无争用**（仅所属连接 task 读改写） | 热路径按块累加 | 
| `CancellationToken` 子令牌 | 父节点 `Mutex<Inner>` + 每节点 `Mutex` + `Notify` | **全局单锁**（所有连接共享同一个父 token） | 见 1.2 |
| `Stats`（14 个字段） | 全部 `AtomicU64`（Relaxed） | 全局单原子（每块数据 2 次 `fetch_add`） | 热路径每块 2 次 |

### 1.2 每连接生命周期的锁 / 原子 / 分配

| 阶段 | 锁操作 | 原子操作 | 堆分配 |
|---|---|---|---|
| **register** | `per_listen`(1) + `per_client`(1) + `shards[id%16]`(1) + **父令牌 `Mutex`**(1，`child_token` 里 `children.push`) = **4 次** | `next_id`、`active`、`detail_count`、`per_upstream` ≈ 4 次 | `ConnMeta`(Arc)、`TreeNode`(Arc)+每节点 `Mutex`+`Notify`、HashMap 节点、父 `children: Vec` 扩容 |
| **热路径（每数据块）** | 0 | `ConnMeta.bytes_*` ×1 + `Stats.bytes_*` ×1 | 0 |
| **状态切换**（协议/上游/阶段） | 0 | `ConnMeta.protocol/state/upstream` + `per_upstream` ×1 | 0 |
| **unregister（`Drop`）** | `per_listen`(1) + `per_client`(1) + `shards`(1) + **子令牌销毁**：`remove_child` 需父锁(1)，若该子不在 `children` 末尾还要再锁**被换位的另一个连接的节点**(1) = **4–5 次** | `active`、`detail_count`、`per_upstream` ≈ 3 次 | 释放 |

> **隐藏最深的一处**：`CancellationToken::child_token()`（tokio-util 0.7.20 `tree_node.rs:87`）
> 会 `lock` 父节点并 `children.push`；子令牌 drop 时 `decrease_handle_refcount` →
> `with_locked_node_and_parent` → `remove_child` 再取父锁，且做 `swap_remove` 式删除，
> **可能顺带锁住另一个连接的节点**（`tree_node.rs:220-240`）。
> 也就是说"每连接一个子 token"实际是"每连接 2–3 次全局锁 + 一个 `Mutex`/`Notify` 分配"。

### 1.3 控制面读取路径

| 端点 | 实现 | 成本 |
|---|---|---|
| `/status` | `stats.snapshot()`（14 次原子读）+ `conns.active()` | 极低 |
| `/api/v1/connections` | 逐分片 `lock` → 遍历 → `snapshot()` → 全局 `sort` | O(明细数 log N)，每片短暂持锁 |
| `/api/v1/connections/summary` | `per_upstream` 原子读 + `per_listen`/`per_client` 加锁遍历 + 排序 | 低频，可接受 |

---

## 2. 并发表现实测（`examples/conn_bench.rs`）

Windows 32 vCPU、release、每 op = 完整 `register + set_* + add_bytes×2 + drop`：

| 场景 | ops/s | ns/op |
|---|---|---|
| A) 完整路径 `detail=true` | 849,569 | 1,177 |
| B) `detail=false` | 931,379 | 1,074 |
| C) `enabled=false`（只建 meta + token） | 1,100,820 | 908 |
| D) 仅 `child_token + drop` | 1,796,997 | 556 |
| E) `Stats` 全局原子 ×4（对照） | 17,800,627 | 56 |

**多核缩放（A 路径）**：

| 线程数 | ops/s | ns/op | 相对单线程 |
|---|---|---|---|
| 1 | 3,771,042 | 265 | 1.00x |
| 4 | 3,152,913 | 317 | 0.84x |
| 16 | 999,611 | 1,000 | 0.27x |
| 32 | 843,462 | 1,186 | **0.22x** |

### 2.1 解读

1. **绝对量级不是瓶颈**：真实负载 8K 连接/秒，而完整统计路径上限 843K/s（32 线程最坏争用）——
   余量约 **100 倍**。这与此前压测中"`detail=true/false/enabled=false` 吞吐无差异"的结论一致。
2. **但多核扩展性很差**：32 线程只有单线程的 22%（并行效率 0.7%）。根因是 4–5 把**进程级单锁**
   串行化：`per_listen`、`per_client`、令牌父节点、以及 `active`/`detail_count` 的单一缓存行。
3. **令牌树是最大单项**：纯 `child_token+drop` 556 ns/op（32 线程），约占完整路径的 47%。
4. **原子计数不是主要问题**：`Stats` 路径 56 ns/op（1,780 万/s），远高于真实需求。
5. 结论：一旦把其它串行点（选路 `Mutex`、上游 `addr` `Mutex`）优化掉、吞吐冲到 50K+/s，
   **统计路径会成为下一个硬瓶颈**（843K/s 上限，且争用随核数恶化）。

---

## 3. 方案 A：无锁化（Lock-free）改造

> 目标：保留现有接口（`ConnRegistry` 对外不变），把内部 4–5 把单锁换成"分片/原子/无锁读"。

| # | 现状 | 改造 | 收益 |
|---|---|---|---|
| 1 | `per_listen: Mutex<HashMap>` | **启动期固定** `Vec<(SocketAddr, AtomicU64)>`（监听数 ≤ 4），注册/注销线性查找 + 原子加减 | 去掉每连接 2 次全局锁；完全无锁 |
| 2 | `per_client: Mutex<HashMap>` | 按 IP 哈希分 **64 片** `Mutex<HashMap>`（或 `dashmap`），上限清理按片做 | 争用降 ~64x；仍是锁分片但接近线性扩展 |
| 3 | `active: AtomicU64` | 分片计数器（每 worker/每核一个 `AtomicU64`，快照求和）或保留（56ns 可接受） | 消除跨核同一缓存行写 |
| 4 | `shards[16]: Mutex<HashMap>` | `dashmap::DashMap`（读写分片，快照读不阻塞写）或 `flurry`（真无锁，但维护弱） | 快照/关闭遍历不再阻塞注册 |
| 5 | `child_token()`（父锁 + 每节点 Mutex/Notify） | 取消派生：每连接 `CancellationToken::new()`，连接 task 内 `select! { local.cancelled(), ctx.cancel.cancelled() }` | 去掉每连接 2–3 次全局锁与 `Mutex`/`Notify` 分配 |
| 6 | `next_id: AtomicU64` | 每线程块预取（如 1024 个 id / 次） | 高频 `fetch_add` 的缓存行争用 |
| 7 | `SHARDS = 16` | 提高到 `min(64, cores*2)` | 分片锁冲突进一步下降 |

**预期**：32 线程完整路径从 843K → **3–5M ops/s**（接近单线程线性的一半），令牌路径从 556ns 降到 ~50ns。

**风险与代价**：
- 接口不变，改造集中在 `conn.rs` 内部（+ 新增 `dashmap` 依赖，成熟）；
- 快照语义从"全局一致"变成"逐片弱一致"（控制面本来就不要求强一致）；
- `close_conn` 遍历改为 DashMap 迭代器，语义不变。

---

## 4. 方案 B：消息架构（Actor）

> 目标：热路径彻底无锁、可集中观测、内存上限可控。

```
连接 task ──try_send(Open{meta})──▶ ┌──────────────────┐
          ──try_send(Close{id})───▶ │  ConnStatsActor  │ ──▶ 无锁读快照（ArcSwap<Snapshot>）
          ──（bytes 本地累加）───▶  │ 单 task / 单线程  │ ◀──控制面 Request/Reply（oneshot）
                                    └──────────────────┘
```

- 事件：`Open`、`Close`、`StateChange`（可选）、`Page/Summary`（带 `oneshot` 回复）、`CloseConn`（过滤取消）；
- 热路径只做 `try_send`（有界 `mpsc`，容量 64K，**满则丢弃统计**——统计可丢）；
- `bytes` 不逐块发消息：`ConnMeta` 的原子仍由连接自己累加，actor 只做聚合；
- 控制面从 actor 的 `ArcSwap` 快照读（或 request/reply），语义变为**最终一致**（≤ 一次 flush 间隔）。

**优点**：热路径 ~20–50ns/op；单消费者无锁竞争；可精确控制内存（明细上限、丢弃策略）。
**缺点**：控制面最终一致；`close_conn` 多一跳延迟；有界队列的丢弃策略需要明确语义；新增模块与调试成本；
单消费者上限约 5–10M ops/s（对 100K 级连接速率仍绰绰有余）。

**适用**：目标吞吐 ≥100K conn/s，或需要集中式观测/采样/限流。

---

## 5. 方案 C：混合（实务推荐）

- 热路径：保留 `ConnMeta` 的每连接原子（无争用，设计正确），`Stats` 原子保留；
- 明细表：`DashMap`（写分片锁、读无锁快照）；
- `per_listen` → `Vec<AtomicU64>`；`per_client` → 64 片；`active` → 分片计数器；
- 取消令牌：去掉 `child_token()`，改用"本地 token + `select!` 全局 token"；
- 控制面：直接读无锁/弱一致快照，不做 actor。

收益接近方案 A，复杂度最低，且可以用 `examples/conn_bench.rs` 做回归（目标 ≥3M ops/s @32 线程）。

---

## 6. 决策建议

| 目标 | 建议 |
|---|---|
| 当前水平（≈8K QPS / 连接速率） | **不需要改造**：余量约 100 倍，且统计不是当前瓶颈（真正的串行点见 [12 §6](12-m4-performance.md)） |
| 目标 50K+ conn/s | 按方案 C 改造统计路径，预计 32 线程 843K → 3–5M ops/s |
| 目标 100K+ conn/s / 集中观测 | 方案 B（actor），但先完成方案 C 的分片/令牌改造，再按需引入 |
| 顺序 | ① 选路 `Mutex` + 上游 `addr` `Mutex`（收益最大）→ ② 统计路径（本方案）→ ③ 视需要再做 actor |

**回归门槛**：每次改动跑 `cargo run --release --example conn_bench -- 32 200000`，
完整路径（A）应不低于 3,000,000 ops/s；同时 `docs/12` 的 8K QPS 压测不应退化。

---

## 7. 附录：原始基准输出

```
conn_bench: threads=32 iters/thread=200000 cores=32
A) register+unregister detail=true     6400000 ops  7.533s   849569 ops/s  1177.1 ns/op
B) register+unregister detail=false    6400000 ops  6.872s   931379 ops/s  1073.7 ns/op
C) register+unregister enabled=false   6400000 ops  5.814s  1100820 ops/s   908.4 ns/op
D) child_token + drop                  6400000 ops  3.561s  1796997 ops/s   556.5 ns/op
E) Stats 全局原子 x4                    6400000 ops  0.360s 17800627 ops/s    56.2 ns/op
```

---

## 8. 附：上游 `addr` 字段的存储方案对比（`examples/addr_bench.rs`）

`Upstream.addr: Arc<Mutex<SocketAddr>>` 与 `health::Entry.addr` 共享同一个锁。
实际读写频率：

| 操作 | 频率 | 说明 |
|---|---|---|
| 读 `addr()` | **每连接 1 次**（8K QPS ≈ 8K 次/s） | `relay/mod.rs:597` 建上游时；`trace!` 字段在日志级别关闭时不求值 |
| 写 | **默认 0 次/s** | 只有配置了 `dnsTtl` 才有 `spawn_dns_refresher`，最小 1s/上游 |
| 读（health） | 每上游每 `tcpCheckPeriod`（默认 5s）1 次 | 探测任务里 `entry.addr()`，低频 |

Windows 32 vCPU 微基准（`addr_bench`，只读场景 + 999 读 : 1 写混合）：

| 方案 | 单线程读 | 32 线程同读（最坏争用） | 写成本 | IPv6 |
|---|---|---|---|---|
| `Arc<Mutex<SocketAddr>>`（现状） | **3.0 ns** | 34.3 ns | 加锁（极低频） | ✅ |
| `Arc<RwLock<SocketAddr>>`（std） | 3.4 ns | **69.5 ns**（读计数原子更争用） | 写锁 | ✅ |
| `AtomicU64`（IPv4 打包 `addr<<16 \| port`） | **0.3 ns** | 0.1 ns（纯读无写） | 单 store | ❌（IPv6 放不下） |
| `ArcSwap<SocketAddr>`（arc-swap，未入依赖） | ~1–2 ns（无锁 load） | ~1–2 ns（无锁） | 一次 `Arc` 分配 + swap | ✅ |

**结论**：

1. **现状 `Mutex` 在真实负载下不是问题**：8K 次/s × 3 ns ≈ **0.02 ms/s** CPU（0.002% 单核）；
   即使 100K conn/s 也仅 ~0.3 ms/s。`addr` 改造优先级应在选路/统计之后。
2. **原子量（`AtomicU64`）并不"更好"**：只快 ~2.7 ns/连接（≈0.002% CPU），却要求
   **IPv4-only 打包**（上游域名可能解析出 IPv6，需要双路径/编码正确性测试），
   为可忽略的收益引入复杂度，不划算；仅当"每包都读地址"这类极高频场景才值得。
3. **`std::sync::RwLock` 不推荐**：32 线程同读实测比 `Mutex` 更慢（reader 计数是共享缓存行原子写）。
4. **若决定去锁，选 `ArcSwap<SocketAddr>`**：读写都无锁、支持 IPv6、改动局部（`upstream` 字段 +
   `health::Entry` 同步 + DNS 刷新写入），读成本与 `AtomicU64` 同量级。
5. **更值得的长期方向**：把"上游池"作为**不可变快照**整体发布（`ArcSwap<Vec<UpstreamCfg>>`），
   读一次拿到整池，顺带消除选路 `Mutex<SelectState>`——收益远大于单独改 `addr`。

方案对比小结（若要实施，推荐顺序）：

| 优先级 | 方案 | 收益 | 成本 |
|---|---|---|---|
| 1 | 保持现状 `Mutex` | — | 0 |
| 2 | `ArcSwap<SocketAddr>` | 每连接省 ~1–3 ns、无锁 | 少量依赖 + 3 个文件改动 |
| 3 | 整池不可变快照（含选路无锁） | 消除选路/addr 两把锁，吞吐上限提升 | 中等，需重排 `selector` 接口 |
| − | `AtomicU64` 打包 IPv4 | 省 ~2.7 ns，但牺牲 IPv6 | 编码/测试复杂度 |
| − | `std::sync::RwLock` | 无（读更慢） | 0 |

---

## 9. 不修改选路时：统计相关方案的量化对比

### 9.1 约束与前提

- 不改 `UpstreamSelector` 的 `Mutex<SelectState>`（每连接一次全局锁）→ 整体吞吐上限仍在
  **约 8K QPS**（见 [12 §3.5](12-m4-performance.md) 实测），所以**统计改造不会提升任何实测吞吐**；
- 统计路径当前 32 线程上限 840K ops/s，8K/s 负载下只占 CPU 约 **0.02 ms/s**；
- 因此下面所有方案的收益都是"为将来铺路 / 降每连接 CPU 与分配 / 降争用"，不是"当下提速"。

### 9.2 拓宽基准（`examples/conn_bench`，Windows 32 vCPU）

新增两组：**F) tokio mpsc `try_send`**（有界 64K + 单消费者 drain）、
**G) A1 原型**（per_listen 原子、per_client 64 分片、active 32 分片、明细 16 分片、无子令牌）。

| 分组 | 1 线程 | 32 线程 | 说明 |
|---|---|---|---|
| A) 现状完整路径 | 3.84 M/s（260 ns） | **840 K/s（1,190 ns）** | 现状 |
| B) `detail=false` | 4.42 M/s | 905 K/s | |
| C) `enabled=false` | 7.70 M/s | 1.03 M/s | 仍含 `ConnMeta` + `child_token` |
| D) `child_token + drop` | 15.2 M/s（66 ns） | 1.68 M/s（594 ns） | 令牌树锁 |
| E) `Stats` 原子 ×4 | 143.6 M/s（7 ns） | 17.6 M/s（57 ns） | |
| F) mpsc `try_send` | 79.5 M/s（12.6 ns） | **323.8 M/s（3.1 ns）** | 有界队列、消费者足够快 |
| G) A1 原型 | 18.3 M/s（55 ns） | **8.6 M/s（116 ns）** | 只换数据结构的上限 |

### 9.3 方案对比（不改选路）

| 方案 | 32T 上限（估） | 相对现状 | 改动量 | 新依赖 | 当前 8K/s 收益 | 主要风险 |
|---|---|---|---|---|---|---|
| **0 不改** | 840 K/s | 1x | 0 | 0 | — | 无 |
| **D 工程微调**（`SHARDS` 16→64、`detail` 默认可关、`per_client` 分片） | 1.5–2.5 M/s | 1.8–3x | ~60 行 | 0 | ≈0（省 ~0.02 ms/s CPU） | 语义不变，低 |
| **A1 最小无锁化**（`per_listen`→原子数组、`per_client`→64 分片、`active` 分片、去掉 `child_token`） | 2–3 M/s（仍含 `ConnMeta` 分配） | 2.4–3.6x | ~200 行 | 0 | ≈0 | relay 的 token `select!` 改动点要覆盖全；快照弱一致 |
| **A2 全面无锁**（A1 + `DashMap` 明细 + `next_id` 块分配） | 4–6 M/s | 5–7x | ~300 行 | `dashmap` | ≈0 | 快照/`close` 遍历语义；依赖 |
| **B Actor**（热路径 `try_send`、单消费者聚合、`ArcSwap` 快照） | 5–10 M/s（受单消费者 ~20 M/s 限制） | 6–12x | ~500 行新模块 | 0（tokio mpsc 已有） | ≈0 | 控制面最终一致；`close_conn` 多一跳；背压策略；调试成本 |

> F 的 3.1 ns/消息说明"投递"几乎免费；Actor 的真正瓶颈是**单消费者处理**
> （每消息 HashMap 更新 + 计数约 30–60 ns → 上限 15–30 M/s），以及控制面的 request/reply 语义。

### 9.4 结论

1. **不改选路时，统计路径不是瓶颈**：五条方案对当前吞吐的收益都 ≈0；唯一实在的好处是
   每连接 CPU 与分配减少（A1 去掉 `TreeNode`/`Mutex`/`Notify`，每连接省 ~60–100 ns 与 ~100–150 B）。
2. 若一定要动，选 **D（工程微调）+ A1**：风险低、改动集中在 `conn.rs`，32T 上限提到 2–3 M/s，
   并为后续选路无锁化铺路。
3. **不要单独上 A2 / Actor**：在选路未改的前提下，额外复杂度换不到可测收益；Actor 应等统计
   成为真实瓶颈（连接速率 >50K/s）再考虑。
4. 选路改造完成、吞吐冲到 50K+ 后，按 A1 → A2 推进，并以 `conn_bench` 32T ≥3M ops/s 与
   [12](12-m4-performance.md) 的 8K QPS 压测作为回归门槛。

