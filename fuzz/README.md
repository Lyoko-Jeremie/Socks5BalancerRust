# fuzz 目标（cargo-fuzz）

对 `src/handshake/` 里**真实**的协议解析代码做随机输入测试（ADR-008：解析路径零 panic）。

fuzz 是一个**独立 workspace**，通过 path 依赖主 crate 的 lib target
（`Cargo.toml` 中 `[lib] name = "socks5_balancer_rust"`），因此跑的是 `src/` 里的真实实现，
不存在"复制一份解析逻辑导致与线上分叉"的问题。

## 前置

fuzz 需要 nightly（libFuzzer + sanitizer 依赖 `-Zsanitizer`）和 cargo-fuzz：

```bash
rustup toolchain install nightly
cargo install cargo-fuzz
```

## 目标一览

| target | 覆盖的解析代码 | 关注点 |
|---|---|---|
| `socks5_negotiate` | `handshake::socks5::negotiate` | 版本/方法列表、RFC1929 认证、CONNECT 的 IPv4/域名/IPv6、错误应答 |
| `http_head` | `handshake::http::{parse, read_head, check_proxy_auth}` | CONNECT authority、absolute-form 改写、Proxy-Authorization、头部超限/截断 |
| `socks4_request` | `handshake::socks4::negotiate` | 8 字节定长头、USERID 增强认证、SOCKS4a 域名、非法命令 |

每个 target 内部用 `tokio::io::duplex` + 共享 current-thread runtime 驱动 async 解析函数：
随机字节写入内存管道后立刻关闭写端，解析器读到 EOF 即可返回错误，
既覆盖"输入不完整"分支，也不会因为等字节而挂死。

## 运行

在仓库根目录执行：

```bash
# 列出所有 target
cargo +nightly fuzz list

# 逐个跑（默认持续 fuzz，Ctrl-C 结束）
cargo +nightly fuzz run socks5_negotiate
cargo +nightly fuzz run http_head
cargo +nightly fuzz run socks4_request

# 只跑固定时间（例如 60 秒）
cargo +nightly fuzz run socks5_negotiate -- -max_total_time=60

# 并行 fuzz（多核）
cargo +nightly fuzz run socks5_negotiate -- -jobs=4 -workers=4
```

语料与崩溃产物：

- `fuzz/corpus/<target>/`：累积语料（建议保留，命中率会随语料增长）；
- `fuzz/artifacts/<target>/`：崩溃/超时输入，复现方式：
  `cargo +nightly fuzz run <target> artifacts/<target>/<crash-...>`。

## 与 CI 的关系

本目录**不参与** `.github/workflows/ci.yml`：CI 只用 stable 工具链，而 fuzz 需要 nightly +
sanitizer。建议在本地或独立的长跑机器上定期执行，发现崩溃后把输入放进 `corpus/` 做回归。

## 新增 target

1. 在 `fuzz/fuzz_targets/` 下新建 `xxx.rs`（`#![no_main]` + `fuzz_target!`）；
2. 在 `fuzz/Cargo.toml` 增加对应的 `[[bin]]` 段（`test = false`、`doc = false`）；
3. 优先调用 `socks5_balancer_rust` 的公开 API；若需要放宽 `src/` 可见性，先讨论再改。
