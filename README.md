# Sisyphus

自托管 CI/CD 平台：单二进制 Server + 全平台 Agent，流水线定义存服务端数据库、经 Web UI 可视化编辑。

设计特点：

- **流水线定义只存 Server 端**，经 Web UI 可视化编辑——repo 内没有任何配置文件。
- **单进程 Server**：Web UI、REST API、编排调度都在一个二进制里；v1 设计规模约 100 台 Agent、日千级构建。
- **Agent 全矩阵**：Linux / macOS / Windows × x86_64 / aarch64；默认宿主机直跑（零依赖），可选容器执行环境（v1 以 Docker 为主）。
- **内置可观测与安全**：构建快照永久可查、日志流式回放；项目级三档角色 + 全局管理员、会话 + PAT 双通道；机密加密落库、安全事件审计记账。

技术栈：Rust（axum：REST + SSE；tonic：gRPC 通道；sqlx + SQLite；rust-embed 内嵌前端）+ Vue 3 + Naive UI；proto 为 Agent/Server 间唯一共享契约。

## Get Started

前置：Rust toolchain（vendored protoc，无需系统 protoc）；前端开发需 Node.js ≥ 18。

### 本地构建并启动 Server

```bash
cargo build --workspace
./target/debug/sisyphus-server --data-dir ./data
```

首次启动自动生成 `config.toml` 与 `master.key`，并前向迁移数据库。打开 http://localhost:8080 按 Web 初始化引导创建首个管理员；无浏览器环境可用 headless CLI：

```bash
./target/debug/sisyphus-server --data-dir ./data admin create --password-stdin admin
```

### 接入一台构建机（Agent）

在 Web UI「构建机」页建条目并签发一次性注册码，在构建机上运行：

```bash
sisyphus-agent --server-url http://<server>:50051 --api-url http://<server>:8080 --reg-key sisa_reg_xxx
```

注册成功后 token 落盘，Agent 常驻领取任务。

### 前端开发

```bash
cd sisyphus-web
npm ci
npm run dev    # http://localhost:5173，/api 代理到本机 Server 8080
```

质量门：`npm run check`（typecheck + vitest + i18n 对账），`npm run build && npm run smoke`（headless 冒烟）。

### Docker 镜像

**从源码构建**（多阶段：前端产物 → server release 编译 → debian-slim runtime，内嵌 git/subversion 供 SCM 探测，非 root）：

```bash
docker build -t sisyphus-server .
```

**运行 + 建首个管理员**：

```bash
# 建管理员（headless，密码经 stdin 读取不进进程列表）
docker run --rm -i -v ./data:/data sisyphus-server \
  admin create --password-stdin admin <<< 'your-admin-password'

# 常驻（8080 = REST + Web UI）
docker run -d --name sisyphus-server -p 8080:8080 -v ./data:/data sisyphus-server
```

官方镜像 `ghcr.io/godreach/sisyphus-server:latest` 为 linux/amd64 + arm64 多架构 manifest（由 release 工作流在各平台原生构建）；本地 `docker build` 产单架构镜像。需要远端 Agent 经 gRPC 接入时，覆盖 `SISYPHUS_GRPC_ADDR=0.0.0.0:50051` 并发布 50051 端口——完整示例见 [examples/docker-compose.yml](examples/docker-compose.yml)。

### 测试

```bash
cargo test --workspace
```

## Contributing

环境前置、质量门、提交规范与 PR 流程见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## License

[MIT](LICENSE)
