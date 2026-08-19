//! 生成器：从 `sisyphus-model` 与样本集产出 4 个提交进 `sisyphus-web/src/model/` 的文件
//! （ADR-0009，票 B4-T7）。`gen` 写盘、`check` 内存重生后与盘上 diff（漂移/手改即 exit 1）。
//!
//! 产出：
//! - `pipeline.ts`：三级结构 + 枚举 + tagged 形态的 TS 类型（serde JSON 形态镜像）。
//! - `codes.ts`：`ValidationCode` 镜像（`as const` 数组 + 字面量联合类型）。
//! - `pipeline.snapshot.ts`：扎根 serde JSON 的类型化字面量——vue-tsc 据此对账
//!   TS 类型与 serde 形态（excess/missing/错 tag/错变体/null-where-required）。
//! - `reconcile.fixtures.json`：`{ rules, samples[] }`，前端对账测试消费。

use std::format;

use serde_json::Value;

use crate::samples::{ALL_CODES, Sample};

/// 生成文件相对路径 → 内容（`gen` 写盘 / `check` diff 用同一份，保证一致）。
pub fn generated() -> Vec<(&'static str, String)> {
    vec![
        ("pipeline.ts", pipeline_ts()),
        ("codes.ts", codes_ts()),
        ("pipeline.snapshot.ts", snapshot_ts()),
        ("reconcile.fixtures.json", fixtures_json()),
    ]
}

/// `pipeline.ts`：TS 类型定义（serde JSON 形态镜像）。
///
/// 类型文本与 `sisyphus-model/src/pipeline.rs` 的 serde 属性一一对应——
/// Option 的 `null`（无 skip）vs `undefined`（skip）、Vec 的必填（default 无 skip）
/// vs 可选（skip_if_empty）、bool/u32 的条件省略、tagged enum 的判别联合。
/// 此映射是「TS 标可选但 serde 永发」方向（如 `CacheSpec.paths`）的唯一事实源——
/// 该方向无法靠扎根 serde 的快照字面量对账（present 总满足 optional），靠人工评审。
fn pipeline_ts() -> String {
    // 用真实换行（非 `\` 续行）保缩进——`\` 续行会吞下一行前导空白。
    format!(
        r#"{header}
/// Pipeline 三级结构 + 枚举 + tagged 形态——sisyphus-model 的 serde JSON 形态镜像
/// （ADR-0006/0009，票 B4-T7）。单一事实源在 `sisyphus-model/src/pipeline.rs`；
/// 本文件由 sisyphus-codegen 生成，勿手改。`cargo run -p sisyphus-codegen -- check` 校验漂移。
///
/// 与 `src/api/types.ts` 的关系：types.ts 的 `ModelPipeline` 等是只读页面的窄 DTO 子集；
/// 本文件是完整权威模型，供编辑器/前端校验消费。两者共存、不互替。

/** 参数类型（serde `rename_all=lowercase`）。 */
export type ParameterType = 'string' | 'number' | 'bool' | 'enum'

/** 参数值（serde `untagged`：单值）。 */
export type ParameterValue = string | number | boolean

/** shell 解释器偏好（serde `rename_all=lowercase`）。 */
export type Shell = 'sh' | 'bash' | 'pwsh' | 'cmd'

/** 环境变量键值对。 */
export interface EnvVar {{
  name: string
  value: string
}}

/** 通知配置（`on_success` serde `default` 无 skip → 永发）。 */
export interface Notification {{
  on_success: boolean
}}

/** 修订版本。 */
export interface Revision {{
  number: number
  operator: string
  at_ms: number
}}

/** 产物上传声明。 */
export interface ArtifactUpload {{
  name: string
  path: string
}}

/** 产物下载依赖。 */
export interface ArtifactDownload {{
  job: string
  name: string
  path: string
}}

/** 缓存声明（`paths`/`files` serde `default` 无 skip → 永发，必填）。 */
export interface CacheSpec {{
  key: string
  paths: string[]
  files: string[]
}}

/** Pipeline 参数。 */
export interface Parameter {{
  name: string
  type: ParameterType
  /** serde `default` 无 skip → 永发。 */
  required: boolean
  /** Option `skip_none` → 缺省时省略。 */
  default?: ParameterValue
  /** Option `skip_none` → 缺省时省略。 */
  description?: string
  /** Vec `skip_if_empty` → 空时省略。 */
  choices?: string[]
}}

/** 执行环境（serde `tag=type, content=config`；`Host` 无 config）。 */
export type ExecutionEnv =
  | {{ type: 'host' }}
  | {{ type: 'container'; config: {{ image: string }} }}

/** 步骤（serde `tag=type, content=config`）。 */
export type Step =
  | {{ type: 'shell'; config: {{ command: string; shell: Shell | null; when?: string }} }}
  | {{ type: 'checkout'; config: {{ submodules?: boolean; when?: string }} }}

/** 任务。 */
export interface Job {{
  name: string
  /** Option `skip_none` → 缺省时省略。 */
  exec_env?: ExecutionEnv
  /** Vec `skip_if_empty` → 空时省略。 */
  labels?: string[]
  /** Option `skip_none` → 缺省时省略。 */
  when?: string
  /** Vec `skip_if_empty` → 空时省略。 */
  env?: EnvVar[]
  /** bool `skip_if_false` → 仅真时出现。 */
  allow_failure?: boolean
  /** u32 `skip_if_zero` → 仅非零时出现。 */
  retry_count?: number
  /** u32 `skip_if_zero` → 仅非零时出现。 */
  timeout_minutes?: number
  /** Vec `skip_if_empty` → 空时省略。 */
  artifact_uploads?: ArtifactUpload[]
  /** Vec `skip_if_empty` → 空时省略。 */
  artifact_downloads?: ArtifactDownload[]
  /** Vec `skip_if_empty` → 空时省略。 */
  caches?: CacheSpec[]
  /** Vec `skip_if_empty` → 空时省略。 */
  secrets?: string[]
  /** serde `default` 无 skip → 永发（空为 `[]`），必填。 */
  steps: Step[]
}}

/** 阶段。 */
export interface Stage {{
  name: string
  /** Option `skip_none` → 缺省时省略。 */
  when?: string
  /** serde `default` 无 skip → 永发（空为 `[]`），必填。 */
  jobs: Job[]
}}

/** Pipeline 定义（三级结构）。 */
export interface Pipeline {{
  name: string
  /** serde `default` 无 skip → 永发（空为 `[]`），必填。 */
  parameters: Parameter[]
  /** serde `default` 无 skip → 永发（空为 `[]`），必填。 */
  env: EnvVar[]
  /** Option `skip_none` → 缺省时省略。 */
  notification?: Notification
  /** serde `default` 无 skip → 永发（空为 `[]`），必填。 */
  stages: Stage[]
  /** Option `skip_none` → 缺省时省略。 */
  revision?: Revision
}}
"#,
        header = HEADER
    )
}

/// `codes.ts`：`ValidationCode` 镜像。
fn codes_ts() -> String {
    let codes: Vec<String> = ALL_CODES
        .iter()
        .map(|c| {
            // serde rename_all=snake_case 的字符串形态（前端字面量联合同值）。
            serde_json::to_string(c).unwrap()
        })
        .collect();
    format!(
        "{header}\n\
/// `ValidationCode` 镜像——校验规则码单一事实源在 `sisyphus-model/src/validate.rs`；\n\
/// 本文件由 sisyphus-codegen 生成，勿手改。`cargo run -p sisyphus-codegen -- check` 校验漂移。\n\
/// 前端 `validate.ts` 据此 emit `{{ path, code, message }}`，对账测试据此比码。\n\
\n\
export const VALIDATION_CODES = [\n\
{codes}\n\
] as const\n\
\n\
export type ValidationCode = (typeof VALIDATION_CODES)[number]\n",
        header = HEADER,
        codes = codes
            .iter()
            .map(|c| format!("  {c},"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// `pipeline.snapshot.ts`：扎根 serde JSON 的类型化字面量（仅 `snapshot == true` 样本）。
fn snapshot_ts() -> String {
    let mut literals = Vec::new();
    for s in crate::samples::samples().into_iter().filter(|s| s.snapshot) {
        let json = serde_json::to_value(&s.pipeline).expect("serialize pipeline");
        let literal = json_to_ts(&json, 0);
        literals.push(format!("export const {id}: Pipeline = {literal}", id = s.id));
    }
    format!(
        "{header}\n\
/// 扎根 sisyphus-model serde JSON 的类型化字面量——vue-tsc 据此对账 `pipeline.ts`\n\
/// 的 TS 类型与 serde 形态（excess/missing/错 tag/错变体/null-where-required）。\n\
///\n\
/// 护栏非对称：抓不到「TS 标可选但 serde 永发」（如 `CacheSpec.paths`——present 总满足\n\
/// optional）；该方向以 `pipeline.ts` 的 serde→TS 映射为准（人工评审）。\n\
/// 本文件由 sisyphus-codegen 生成，勿手改。`cargo run -p sisyphus-codegen -- check` 校验漂移。\n\
\n\
import type {{ Pipeline }} from './pipeline'\n\
\n\
{literals}\n",
        header = HEADER,
        literals = literals.join("\n\n")
    )
}

/// `reconcile.fixtures.json`：`{ rules, samples[] }`，前端对账测试消费。
fn fixtures_json() -> String {
    let rules: Vec<Value> = ALL_CODES
        .iter()
        .map(|c| serde_json::to_value(c).unwrap())
        .collect();
    let samples: Vec<Value> = crate::samples::samples()
        .into_iter()
        .map(|s| sample_fixture(&s))
        .collect();
    let root = serde_json::json!({
        "rules": rules,
        "samples": samples,
    });
    // 末尾换行与 gen 写盘一致（diff 友好）。
    format!("{}\n", serde_json::to_string_pretty(&root).unwrap())
}

/// 单样本的 fixtures 条目：`{ id, valid, expectedCodes, json }`。
fn sample_fixture(s: &Sample) -> Value {
    let expected: Vec<Value> = s
        .expected_codes
        .iter()
        .map(|c| serde_json::to_value(c).unwrap())
        .collect();
    serde_json::json!({
        "id": s.id,
        "valid": s.valid,
        "expectedCodes": expected,
        "json": serde_json::to_value(&s.pipeline).expect("serialize pipeline"),
    })
}

/// `@generated` 文件头（4 个产物共用）。
const HEADER: &str = "// @generated by sisyphus-codegen (sisyphus-model → TS)，勿手改——改 model 后跑 `cargo run -p sisyphus-codegen -- gen`。";

/// JSON 值 → TS 对象字面量文本（扎根 serde 输出，保形）。
fn json_to_ts(v: &Value, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    let pad_inner = "  ".repeat(indent + 1);
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap(),
        Value::Array(arr) => {
            if arr.is_empty() {
                return "[]".into();
            }
            let items: Vec<String> = arr
                .iter()
                .map(|e| format!("{pad_inner}{}", json_to_ts(e, indent + 1)))
                .collect();
            format!("[\n{}\n{pad}]", items.join(",\n"))
        }
        Value::Object(obj) => {
            if obj.is_empty() {
                return "{}".into();
            }
            let items: Vec<String> = obj
                .iter()
                .map(|(k, e)| format!("{pad_inner}{key}: {val}", key = ts_key(k), val = json_to_ts(e, indent + 1)))
                .collect();
            format!("{{\n{}\n{pad}}}", items.join(",\n"))
        }
    }
}

/// TS 对象字面量键：合法标识符裸写，否则引号（serde 键均为合法标识符，但稳健起见判定）。
fn ts_key(k: &str) -> String {
    let valid = !k.is_empty()
        && k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        k.to_string()
    } else {
        serde_json::to_string(k).unwrap()
    }
}
