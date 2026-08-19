# sisyphus-model

Pipeline 定义 JSON 模型、when 表达式 AST、`${}` 变量解析与保存校验（纯逻辑叶子 crate，ADR-0006/0009）。

## 定位

纯类型 + 纯逻辑的**叶子 crate**：不依赖 tokio/axum/sqlx，只依赖 serde/serde_json/thiserror。承载 Pipeline 三级结构、when 表达式 AST 与求值、`${}` 变量解析、保存校验规则，是三处消费方的**单一事实源**（ADR-0009）：

- 编辑器保存校验（坏定义在保存时报错而非运行期爆炸）
- 构建快照存储（整份定义 JSON 入库，"某次构建当时跑了什么"永远可查）
- TS 类型生成锚点（经 `sisyphus-codegen` 投影到前端）

## 模块结构

| 模块 | 说明 |
| --- | --- |
| `pipeline` | Pipeline 三级结构类型：Pipeline（参数/env/通知）→ Stage（when）→ Job（执行环境/标签/when/env 覆盖/失败语义/产物/缓存/机密）→ Step（仅 shell 与 checkout scm）。纯类型 + serde |
| `when` | when 受限表达式：解析为 AST + 求值。语言不图灵完备——比较、`&&`/`||`、字符串相等、存在性判断；越界语法解析期拒绝 |
| `variables` | `${name}` 变量引用解析：`${name}` 展开、`$${name}` 转义；8 个 `SISY_` 内置变量与用户参数同一套语法 |
| `validate` | 保存校验：14 条规则各一稳定码（`ValidationCode`，snake_case 序列化）+ 可定位的 `ValidationError{path, message, code}` |

## 关键设计

- **求值器 Server 独享**（ADR-0009）：when 求值只在 Server 端跑，Agent 拿到的 `JobSpec` 已是只含待执行节点的解析后规格，对 Pipeline 定义本身一无所知。
- **变量解析分工**：7 个内置变量与用户参数由 Server 端解析完毕；`SISY_WORKSPACE` 以占位符随规格下发、Agent 执行前替换（ADR-0011）。when 表达式与缓存 key 禁用 `SISY_WORKSPACE`——由保存校验拒绝。
- **校验码是稳定身份**：`ValidationCode` 序列化为 `snake_case` 字符串，前端实时校验与对账测试据此对账。server 把 `ValidationError` 重投影成自己的 `ValidationIssue{path, message}`（只拷贝两者），故此码**不进 wire/OpenAPI**——是 model 内部身份。

14 条规则覆盖：必填参数默认值、enum 候选、when 语法/`SISY_WORKSPACE`、shell 命令空、容器 image 空、env 与机密名冲突、产物上传 name/路径、缓存 key 空/过长/`SISY_WORKSPACE`、缓存 paths 相对性、缓存 files glob（ADR-0006/0011/0012/0015）。

## 构建

```bash
cargo build -p sisyphus-model
cargo test -p sisyphus-model           # 类型 round-trip、when 求值、校验规则单测
```

## 与其它 crate 的关系

- `sisyphus-server` 依赖本 crate：定义以 JSON 形态往返原样落库读回（schema 不解析定义内部）、保存前先过 `validate`。
- `sisyphus-codegen` 依赖本 crate：把类型与校验码投影成 TS。本 crate 不依赖任何其它 sisyphus crate（叶子）。

## 参见

- [ADR-0006](../docs/adr/0006-pipeline-data-model-and-execution-semantics.md)（Pipeline 数据模型与执行语义）、[ADR-0009](../docs/adr/0009-workspace-structure-and-module-boundaries.md)（模块边界与单一事实源）、[ADR-0011](../docs/adr/0011-agent-workspace-isolation-and-lifecycle.md)（工作区）、[ADR-0015](../docs/adr/0015-secrets-and-audit-log.md)（机密）
- [CONTEXT.md](../CONTEXT.md)（Pipeline / when / 参数 / 内置变量 / 机密 等词条）
