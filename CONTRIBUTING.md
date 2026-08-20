# 贡献指南

欢迎给 sisyphus 提交贡献。本文件是贡献者的单一入口——环境前置、构建/测试/lint、质量门、提交规范、PR 流程在此收口；架构决策与领域词汇另有权威来源（见文末）。

## 项目定位

sisyphus 是自托管 CI 平台（单二进制 Server + 全平台 Agent），完整定位与三大根本取舍见 [README](README.md) 的「定位」节。动手前先确认改动与既有取舍一致——架构级变更应落在 [docs/adr/](docs/adr/)。

## 环境前置

| 组件 | 版本 | 用途 |
| --- | --- | --- |
| Rust | 见 `Cargo.toml` 的 `rust-version`（当前 1.97+，edition 2024） | 全 workspace（proto/model/server/agent/codegen） |
| sqlx-cli | 任意稳定版 | 仅改编译期 `sqlx::query!` 查询时生成 `.sqlx/` 离线校验 |
| Node + npm | 见 `sisyphus-web/package.json` 的 `engines.node`（Node 20+） | 仅改 `sisyphus-web/` 前端 |

Rust 工具链用 vendored protoc，无需系统 protoc（ADR-0009）。Node 仅 `sisyphus-web/` 子目录用到，仓库根无 node 环境。

## 构建 / 测试 / lint

```bash
cargo build --workspace                        # 构建
cargo test --workspace                         # 测试
cargo clippy --workspace -- -D warnings        # lint（警告即错）
```

数据库迁移工作流（`sqlx::migrate!` 编译期嵌入、`.sqlx/` 离线校验）与 REST 契约 / OpenAPI snapshot 守护的完整步骤见 [README](README.md) 的「开发」节——改动涉及迁移或 REST 契约时按那两节操作。

## 质量门（本地过一遍 = CI 会跑的）

CI（`.github/workflows/ci.yml`）跑这些检查；本地先过一遍省一轮 CI 红灯：

- `cargo test --workspace` —— Rust 全量测试
- `cargo clippy --workspace -- -D warnings` —— lint
- `cargo run -p sisyphus-codegen -- check` —— 生成产物未漂移（model 改后未 gen 或被手改即红；需重生成跑 `cargo run -p sisyphus-codegen`）
- 改前端时在 `sisyphus-web/` 跑 `npm ci && npm run check`（= typecheck + vitest + i18n 对账），再 `npm run build` 与 `npm run smoke`（headless 12 页冒烟）

格式统一用 `cargo fmt --all`（仓库有 `style: cargo fmt` 提交记录）；fmt 非 CI 强制，提交前自行跑。Agent 矩阵额外在 Windows 上构建/测试 `sisyphus-agent`——agent 只依赖 proto，跨平台独立可测。

## 提交信息

提交 subject 遵循 Conventional Commits——`<type>[(<scope>)][!]: <中文描述>`，type ∈ {feat,fix,docs,chore,style,refactor,perf,test,build,ci,revert}。feat/fix 末尾惯例带票号 `（票 #NN）`，body 末 `Closes #NN`。

```bash
feat: 产物链路——存储/端点/Agent 传输/前端产物区（票 #74）
fix(web): headless 冒烟钉中文 locale——修 CI en-US 红灯
```

本地 commit-msg hook 与 CI 会拦下缺 `type:` 前缀的 subject。启用本地 hook（一次性，本地配置不入 git）：

```bash
git config core.hooksPath .githooks
```

完整规则、正例反例、正则见 [docs/agents/commit-messages.md](docs/agents/commit-messages.md)。

## 贡献流程

1. **开 issue**：GitHub Issues 是本仓的请求与 triage 面（PR 不作请求面，只作合并载体）。用 `gh` CLI 建议题、说明动机与方案草稿；`gh` 用法见 [docs/agents/issue-tracker.md](docs/agents/issue-tracker.md)。triage 标签见 [docs/agents/triage-labels.md](docs/agents/triage-labels.md)（`needs-triage` / `needs-info` / `ready-for-agent` / `ready-for-human` / `wontfix`）。
2. **建分支**：从 `main` 切出，命名建议 `<type>-<简述>` 或带票号（如 `feat-artifacts`、`fix/smoke-locale`）。
3. **小步提交**：每个提交一个聚焦改动，subject 合规（见上）。大改动拆成多个小提交便于评审。
4. **开 PR**：标题同提交规范；描述关联 issue（`Closes #NN`），列改动要点与验证情况。PR 应小而聚焦，一次解决一个问题。
5. **评审**：过完质量门、CI 全绿后请求评审；评审意见按新提交响应（不 squash 评审轮次，保留可追溯的修复历史）。

## 架构变更

涉及架构取舍的改动先在 [docs/adr/](docs/adr/) 起一篇 ADR（格式见既有 ADR：背景 / 决策 / 后果，必要时加「理由」），或在 PR 里讨论对既有 ADR 的修正。领域术语统一用 [CONTEXT.md](CONTEXT.md) 词汇表。

## License

贡献依 [MIT](LICENSE) 许可随项目发布。
