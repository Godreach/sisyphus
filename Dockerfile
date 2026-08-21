# syntax=docker/dockerfile:1
# 官方 Docker 镜像（仅 Server，票 B5-T10 / #82，ADR-0010）。
#
# 多阶段：rust builder（release 编译 server，内嵌已提交的 sisyphus-web/dist/
# ——rust-embed 编译期内嵌，无需 npm 步骤）→ debian-slim runtime（捆绑 git+svn
# 供 SCM 探测零配置 ADR-0016、非 root、/data 卷、EXPOSE 8080 50051、HEALTHCHECK
# 打 /healthz）。
#
# 双架构由 release.yml 的双原生 runner（ubuntu-latest + ubuntu-24.04-arm）各
# 自原生构建单架构镜像再 manifest 合并——不走 buildx QEMU 仿真（aws-lc-sys +
# ring + libsqlite3-sys 的 C 编译在 QEMU 下 20-40 分钟）。故本 Dockerfile 不
# 含 CROSS 指令，每条 `docker build --platform <p>` 在与该平台一致的 runner
# 上原生跑。

# ---- builder：编译 server release 二进制 ----
FROM rust:1-bookworm AS builder
WORKDIR /app
# 先拷依赖清单层，装 cargo 缓存（依赖未变时复用）。
COPY Cargo.toml Cargo.lock ./
COPY sisyphus-proto/ ./sisyphus-proto/
COPY sisyphus-model/ ./sisyphus-model/
COPY sisyphus-server/ ./sisyphus-server/
COPY sisyphus-agent/ ./sisyphus-agent/
COPY sisyphus-codegen/ ./sisyphus-codegen/
# sisyphus-web/dist/ 已提交 git（rust-embed 编译期内嵌目录）。
COPY sisyphus-web/ ./sisyphus-web/
RUN cargo build --release -p sisyphus-server

# ---- runtime：debian-slim + git + subversion + 非 root ----
FROM debian:bookworm-slim AS runtime
# ADR-0016：官方 Docker 镜像捆绑 git+subversion，Docker 用户零配置（Server 端
# poll 探测 shell 出 git ls-remote / svn info；Agent 不在此镜像）。ca-certificates
# 供 HTTPS SCM 探测；tini 作 PID 1 正确转发信号（docker stop 的 SIGTERM）。
# wget 供 HEALTHCHECK 探 /healthz（debian-slim 不含 curl）。
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git subversion ca-certificates tini wget \
    && rm -rf /var/lib/apt/lists/*

# 非 root 用户（home 即数据目录 /data，--data-dir /data）。
RUN useradd --system --create-home --home-dir /data --shell /usr/sbin/nologin sisyphus

COPY --from=builder /app/target/release/sisyphus-server /usr/local/bin/sisyphus-server

USER sisyphus
VOLUME /data
# 8080 = REST API（ADR-0010 默认）；50051 = Agent gRPC 通道——注意 Server 默认
# bind 127.0.0.1:50051（loopback），容器内对 Agent 不可达，需经 SISYPHUS_GRPC_ADDR
# 覆盖为 0.0.0.0:50051 并在 compose 发布端口（见 examples/docker-compose.yml）。
# EXPOSE 仅文档，不自动发布。
EXPOSE 8080 50051
# 健康：REST /healthz（sisyphus-server/src/api/health.rs）。30s 间隔、3s 超时、
# 连续 3 次失败判 unhealthy。
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD wget --quiet --tries=1 --spider http://localhost:8080/healthz || exit 1

ENTRYPOINT ["/usr/bin/tini", "--", "sisyphus-server", "--data-dir", "/data"]
