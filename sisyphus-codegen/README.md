# sisyphus-codegen

开发工具：`sisyphus-model` → TS 类型/校验生成 + 前端对账（ADR-0009，票 B4-T7）。**非发布产物**。

## 定位

把 `sisyphus-model` 的 Rust 类型与校验规则投影成 TypeScript，喂给 `sisyphus-web` 前端，并用对账测试守住"模型 ↔ 前端"不漂移。`publish = false`，不在 workspace 的 `default-members`（4 产品 crate）里——`cargo build`/`cargo run`（无 `-p`）不动它，但 `--workspace` 仍含本 crate，CI 的 build-and-test 跑 `cargo run -p sisyphus-codegen -- check`。

## 用法

```bash
cargo run -p sisyphus-codegen -- gen     # 生成 4 文件到 sisyphus-web/src/model/
cargo run -p sisyphus-codegen -- check  # 内存重生与盘上逐文件 diff，漂移/手改即 exit 1（CI 挂）
```

## 产物

`gen`/`check` 走同一份 `generated()`（保证一致），写出到 `sisyphus-web/src/model/`（相对本 crate 目录定位）：

| 文件 | 说明 |
| --- | --- |
| `pipeline.ts` | 三级结构 + 枚举 + tagged 形态的 TS 类型——`sisyphus-model` 的 serde JSON 形态镜像 |
| `codes.ts` | `ValidationCode` 镜像（`as const` 数组 + 字面量联合类型） |
| `pipeline.snapshot.ts` | 扎根 serde JSON 的类型化字面量——vue-tsc 据此对账 TS 类型与 serde 形态（excess/missing/错 tag/错变体/null-where-required） |
| `reconcile.fixtures.json` | `{ rules, samples[] }`——前端对账测试消费 |

`pipeline.ts` 的类型文本与 `sisyphus-model/src/pipeline.rs` 的 serde 属性一一对应（Option 的 `null` vs `undefined`、Vec 的必填 vs 可选、tagged enum 的判别联合）。**勿手改产物**——`check` 守漂移。

## 模块结构

| 路径 | 说明 |
| --- | --- |
| `src/main.rs` | CLI 入口（`gen`/`check`）+ 自检 `#[test]` |
| `src/codegen.rs` | 生成器：`generated()` 产出 4 文件 |
| `src/samples.rs` | 对账样本集：合法 + 各规则破坏样本，每条带声明 `expected_codes`；`ALL_CODES` 14 条规则码规范序 |

## 自检（`cargo test -p sisyphus-codegen`）

无需 `gen` 即可由 `cargo test --workspace` 兜底：

- `rust_validate_matches_declared_codes`：每样本跑 model `validate()`，断言 Rust 标记码 multiset == 声明码——抓 model 漂移（某规则开始/停止在某样本触发）与脏样本。
- `every_code_covered_by_a_sample`：每条规则码至少被一个样本覆盖（防漏同步）。
- `all_codes_count_matches_enum`：`ALL_CODES` 与 model `ValidationCode` 变体数一致（防 model 加规则后漏登记）。

破坏样本遵循"单规则隔离"——除目标规则外其它字段皆合法，避免共触发噪音；一条 `co_r8_r9` 样本故意双触发，验证 multiset（带计数）比较。

## 与其它 crate 的关系

- 依赖 `sisyphus-model`（类型 + `ValidationCode`）。
- 产物落 `sisyphus-web/src/model/`，供前端编辑器/实时校验消费；与 `sisyphus-web/src/api/types.ts` 共存不互替（后者是只读页面的窄 DTO 子集，本产物是完整权威模型）。

## 参见

- [ADR-0009](../docs/adr/0009-workspace-structure-and-module-boundaries.md)（单一事实源）、[ADR-0006](../docs/adr/0006-pipeline-data-model-and-execution-semantics.md)（Pipeline 数据模型）
- 顶层 [README](../README.md)「REST 契约与 OpenAPI snapshot 守护」一节（同款 snapshot 纪律）
