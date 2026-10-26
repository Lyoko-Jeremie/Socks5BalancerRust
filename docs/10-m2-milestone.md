# 10 · M2 里程碑报告（混合端口协议：SOCKS4/4a + HTTP）

> 日期：2026-10-26
> 对应计划：[06-rust-port-assessment.md](06-rust-port-assessment.md) §10 的 **M2**
> 决策依据：[07-decisions.md](07-decisions.md)（ADR-004 入站协议、ADR-007 优先成熟库）
> 代码位置：`src/handshake/{http,socks4}.rs`、`src/relay/mod.rs`、`configs/m2-*.json`、`scripts/m2-smoke.ps1`

---

## 1. 里程碑目标与完成度

| M2 目标（06 §10） | 完成 | 验收证据 |
|---|---|---|
| 首包嗅探与协议分派（SOCKS5 / SOCKS4 / HTTP） | ✅ | 同一监听端口 5500 交替验证三种协议 |
| SOCKS4 CONNECT（IP 目标） | ✅ | `04 01 ...` → `00 5A` + 回显 |
| SOCKS4a CONNECT（域名目标，DSTIP=0.0.0.x） | ✅ | 域名 `localhost` 交上游解析 + 回显 |
| SOCKS4 非 CONNECT 命令拒绝 | ✅ | BIND → `00 5B` |
| HTTP CONNECT | ✅ | `HTTP/1.1 200 Connection established` + 回显 |
| CONNECT 粘包（early data）回放 | ✅ | 请求头与数据同包发送，数据仍到达目标并回显（修正 C++ 失败） |
| HTTP absolute-form 正向代理 | ✅ | curl 经代理成功；目标收到 origin-form `/m2-forward` |
| 非 absolute-form / 非法请求 | ✅ | 回 `400 Bad Request`（C++ 分支A 会塞进隧道，属缺陷 H-1） |
| `disableSocks4` 配置生效 | ✅ | 禁用后回 `00 5B` |
| `slowImpl` 握手兼容 | ⏳ 未做 | 见 §5 已知限制；不影响标准客户端 |
| HTTP keep-alive 多请求逐条改写 | ⏳ 未做 | 首个请求改写后双向透传（见 §5） |

**M2 结论：完成并通过 9/9 端到端验收；`slowImpl` 与 keep-alive 逐请求改写留待后续。**

---

## 2. 交付物

### 2.1 代码（Rust 合计 4,706 行，M1 为 3,963 行）

| 模块 | 行数 | 职责 |
|---|---|---|
| `src/handshake/http.rs` | 312 | HTTP 头读取（含 early data 保留）、httparse 解析、CONNECT/absolute-form 判定、origin-form 改写、应答写入 |
| `src/handshake/socks4.rs` | 161 | SOCKS4/4a CONNECT、USERID/域名读取、8 字节应答 |
| `src/relay/mod.rs` | 647 | 统一 `Tunnel` 模型：协议分派 + 选上游 + 上游握手 + 前缀回放 + 双向转发 |
| `examples/http_target.rs` | 45 | 本地 HTTP 目标（回显请求行，验证改写） |
| `scripts/m2-smoke.ps1` | ~330 | 9 项混合协议端到端验收 |

### 2.2 配置

- `configs/m2-mixed.json`：5500 端口混合协议（非透传模式，上游 mock 需认证）
- `configs/m2-disable-socks4.json`：5501 端口，`disableSocks4=true`

---

## 3. 验收结果（`scripts/m2-smoke.ps1`，9/9 PASS）

| # | 检查 | 结果 |
|---|---|---|
| 1 | SOCKS5 经混合端口仍可用 | ✅ |
| 2 | SOCKS4 IP 目标 CONNECT | ✅ `00 5A` + 回显 |
| 3 | SOCKS4a 域名目标 CONNECT | ✅ `00 5A` + 回显 |
| 4 | SOCKS4 BIND → `00 5B` | ✅ |
| 5 | HTTP CONNECT 200 + 回显 | ✅ |
| 6 | HTTP CONNECT 粘包 early data 回放 | ✅ |
| 7 | forward 请求被改写为 origin-form | ✅ 目标回 `OK GET /m2-forward HTTP/1.1` |
| 8 | 非 absolute-form 回 400 | ✅ |
| 9 | `disableSocks4` 回 5B | ✅ |

### 质量门禁

| 项 | 结果 |
|---|---|
| `cargo test` | ✅ 69 passed / 0 failed（M1 53 → M2 69） |
| `cargo clippy --all-targets -- -D warnings` | ✅ |
| `cargo fmt --check` | ✅ |

---

## 4. 实现要点

### 4.1 统一 Tunnel 模型

所有协议最终都归约为：

```text
Tunnel { target: TargetAddr, prefix: Vec<u8>, reply: Reply }
```

| 协议 | target | prefix | reply |
|---|---|---|---|
| SOCKS5 | CONNECT 解析结果 | 空 | `05 00` / REP |
| SOCKS4/4a | IP 或域名 | 空 | `00 5A` / `00 5B` |
| HTTP CONNECT | authority | **early data**（请求头之后字节） | `200 Connection established` |
| HTTP forward | absolute-form authority | **改写后的请求头 + early data** | 无（直接转发请求） |

随后统一走：选上游 → 连上游 TCP → 上游 SOCKS5 握手 → 回成功应答 → 写 prefix → 双向转发。
失败应答按协议映射（SOCKS5 REP / SOCKS4 5B / HTTP 502）。

### 4.2 与 C++ 版的关键差异

| 主题 | C++ 版 | Rust M2 |
|---|---|---|
| 首包判定 | 只认 `05 01 00`；HTTP 用首字母猜测（`P/G/H/...`） | 按首字节分派，HTTP 交给完整解析器，非法请求回 400 |
| CONNECT 粘包 | 严格 `== 10` 字节判断，粘包即断开 | 读到 `\r\n\r\n`，之后字节作为 prefix 回放 |
| CONNECT 拆包 | 一次 read 不完整即失败 | 循环读直到头结束（带超时/上限） |
| absolute-form | 分支A 把明文塞进 SOCKS5 隧道（缺陷 H-1） | 改写为 origin-form 后经上游隧道发送 |
| Proxy-* 头 | 未剥离 | `Proxy-Connection` / `Proxy-Authorization` 剥离 |
| 应答语义 | 上游失败时下游常无错误码 | 失败分别回 REP / 5B / 502 |

### 4.3 HTTP 改写规则

- 请求行：`GET http://host:port/path?q HTTP/1.1` → `GET /path?q HTTP/1.1`；
- 保留除 `Proxy-*` 外的全部头（含 `Host`）；
- 头部之后的 early data 原样附在改写后的请求头之后一起发给目标。

---

## 5. 已知限制

1. **HTTP keep-alive 多请求**：只有首个请求被解析/改写，之后双向透传。若客户端在同一代理连接上复用 keep-alive，第二个请求仍是 absolute-form，目标可能拒绝。C++ 同样不支持（逐请求改写需在每轮响应后重新解析，M4 再评估）。
2. **`slowImpl` 未实现**：C++ 的"慢实现"兼容模式（分片写/等待）针对个别不标准上游；标准客户端不受影响。
3. **HTTP/2 / HTTP/3 不支持**：仅 HTTP/1.x 明文代理；HTTPS 请走 CONNECT。
4. **WebSocket**：`Upgrade` 请求在 forward 路径会被原样转发，之后双向透传（天然支持）；但 keep-alive 限制同样适用。
5. **`disableSocks4` 语义**：禁用时直接回 5B（C++ 会尝试按 HTTP 解析并最终断开）；对客户端更友好。

---

## 6. 如何运行

```powershell
cargo build --release

# M2 混合协议端到端（9 项）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m2-smoke.ps1

# 手工：混合端口 5500 同时支持三种协议
.\target\release\examples\mock_socks5_upstream.exe 127.0.0.1:6200 alice:secret
.\target\release\Socks5BalancerRust.exe -c configs\m2-mixed.json

# SOCKS4 / SOCKS4a（任意 SOCKS4 客户端）
# HTTP CONNECT：浏览器/工具把代理设为 127.0.0.1:5500
# HTTP forward：curl --proxy http://127.0.0.1:5500 http://target/
```

---

## 7. 下一步

1. **M3：精简控制面** ✅*（2026-10-26 完成，见 [11-m3-milestone.md](11-m3-milestone.md)）*
2. **C10K / 1K 建连压测**（Linux 优先）：连接数上限、RSS、握手 CPU、`bufferSize` 对比。
3. **M4：性能调优与 24h 长稳**（含 HTTP keep-alive 逐请求改写评估）。
4. **Linux CI**：双平台构建/单测/冒烟脚本（bash 版）。
