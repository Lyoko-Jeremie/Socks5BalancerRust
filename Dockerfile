# Socks5BalancerRust 容器镜像（多阶段构建）
#
# 构建：
#   docker build -t socks5balancer-rust:local .
# 运行（生产配置挂载到 /etc/socks5balancer/config.json）：
#   docker run -d --name balancer \
#     -p 5000:5000 -p 127.0.0.1:5110:5110 \
#     -v $PWD/deploy/config.example.json:/etc/socks5balancer/config.json:ro \
#     --ulimit nofile=131072:131072 \
#     socks5balancer-rust:local
#
# 说明：
# - 控制面默认只监听 127.0.0.1（容器内），需要外部访问时显式映射端口并自行评估风险（ADR-001）。
# - 健康检查走 /healthz（不读业务状态）。
# - 镜像基于 debian-slim；如需更小可用 distroless（需自行提供 ca-certificates）。

# ---------- 构建阶段 ----------
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# 先只拷贝清单，利用 Docker 层缓存预热依赖
COPY Cargo.toml Cargo.lock ./
# 占位源文件让 `cargo fetch` 能解析依赖（之后会覆盖）
RUN mkdir -p src examples \
    && printf 'fn main() {}\n' > src/main.rs \
    && printf '' > examples/echo_upstream.rs \
    && printf '' > examples/flood.rs \
    && printf '' > examples/flood_socks5.rs \
    && printf '' > examples/mock_socks5_upstream.rs \
    && printf '' > examples/socks5_client.rs \
    && printf '' > examples/tls_echo_server.rs \
    && printf '' > examples/http_target.rs \
    && cargo fetch --locked

COPY src ./src
COPY examples ./examples
RUN cargo build --release --locked --bins

# ---------- 运行阶段 ----------
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --no-create-home --shell /usr/sbin/nologin balancer

COPY --from=builder /build/target/release/Socks5BalancerRust /usr/local/bin/Socks5BalancerRust

# 生产配置由挂载提供；此处仅建目录（只读挂载也允许）
RUN mkdir -p /etc/socks5balancer && chown balancer:balancer /etc/socks5balancer

EXPOSE 5000 5110

USER balancer
ENTRYPOINT ["/usr/local/bin/Socks5BalancerRust"]
CMD ["-c", "/etc/socks5balancer/config.json"]

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:5110/healthz || exit 1
