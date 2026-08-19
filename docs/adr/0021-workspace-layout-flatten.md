# 0021 - Workspace 布局平铺修订（crate 移根、proto 并入、web 改名）

日期：2026-08-15
状态：已接受

## 背景

ADR-0009 的 workspace 拓扑：4 个 crate 置于 `crates/` 下、`.proto` 源文件独立 `proto/` 目录、前端 `web/`。开工前评审发现三处可简化，一致同意修订。

## 决策

- **crate 平铺到仓库根**：`sisyphus-proto`、`sisyphus-model`、`sisyphus-server`、`sisyphus-agent` 四个 crate 目录放仓库根，与 `sisyphus-web`、`docs/` 平级；取消 `crates/` 中间层。
- **`.proto` 源文件并入 `sisyphus-proto`**：放 `sisyphus-proto/proto/*.proto`，build.rs 指向之；取消仓库根 `proto/` 目录。契约先行、语言中立不变；生成物仍不进 git。
- **前端目录改名 `sisyphus-web`**（替代 `web/`）。

## 理由

- 4 个 crate + 3 个目录时 `crates/` 是冗余嵌套，根目录平铺更直观（`cargo run -p sisyphus-server`）；`crates/` 是为几十个 crate 准备的约定。
- `proto/` 单独一层冗余：proto 源文件收进 `sisyphus-proto` 内，契约与生成物同 crate 同版本演进，语言中立仍成立（目录语言中立、文件即契约）。
- `sisyphus-web` 命名与其余 crate 一致，前端是 monorepo 正式成员而非附属目录。

## 后果

- 未来 crate 增多（如 scm 升 crate、`sisyphus-codegen` 类 dev 工具）时，根目录平铺随之拥挤，届时可再引入 `crates/` 或 `tools/` 顶层目录（本 ADR 允许后续修订）。
- README workspace 表格、ADR-0009 相应段落以本 ADR 为准；`CONTEXT.md` 无布局引用不需改动。
