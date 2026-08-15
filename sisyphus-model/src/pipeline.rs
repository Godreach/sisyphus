//! Pipeline 定义数据模型（ADR-0006 + 下游 ADR-0008/0011/0012/0015/0016/0018）。
//!
//! 三级结构：Pipeline（参数/环境变量/通知）→ Stage（when）→ Job（执行环境/
//! 标签/when/env 覆盖/失败语义/产物/缓存/机密）→ Step（仅 shell 与 checkout scm）。
//! 纯类型 + serde，作为编辑器保存校验、构建快照存储与未来 TS 类型生成锚点（ADR-0009）。

use serde::{Deserialize, Serialize};

/// Pipeline 定义。v1 只存 Server 端数据库（ADR-0001），通过 web UI 编辑。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    /// 名称：用户可读、也是工作区/缓存的归组键（ADR-0011/0012）。
    pub name: String,
    /// 参数定义（ADR-0006：四种类型、单值、必填带默认值）。
    #[serde(default)]
    pub parameters: Vec<Parameter>,
    /// Pipeline 级环境变量（ADR-0006：与 `${}` 替换是两个机制）。
    #[serde(default)]
    pub env: Vec<EnvVar>,
    /// 通知配置（ADR-0006：pipeline 完成时发送，失败必发、成功可配）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification: Option<Notification>,
    /// 阶段：按序执行（ADR-0006，不做任务级 DAG）。
    #[serde(default)]
    pub stages: Vec<Stage>,
    /// 修订版本：每次保存递增（ADR-0006）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
}

/// Pipeline 参数（ADR-0006：string/number/bool/enum 四种、单值、必填带默认值）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Parameter {
    /// 参数名（`${name}` 引用，须为合法变量名）。
    pub name: String,
    /// 参数类型（string/number/bool/enum 四种，ADR-0006）。
    pub r#type: ParameterType,
    /// 必填参数必须带默认值（保存校验，ADR-0006）。
    #[serde(default)]
    pub required: bool,
    /// 默认值。任何触发方式一律「默认值、手动触发可覆盖」（ADR-0006）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<ParameterValue>,
    /// 描述（UI 展示用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// enum 类型的候选项（仅 enum 使用）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

/// 参数类型（ADR-0006：四种，无密码型）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParameterType {
    /// 字符串。
    String,
    /// 数字。
    Number,
    /// 布尔。
    Bool,
    /// 枚举（从 `choices` 中取一）。
    Enum,
}

/// 参数值：单值（ADR-0006）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParameterValue {
    /// 字符串值。
    String(String),
    /// 数值。
    Number(f64),
    /// 布尔值。
    Bool(bool),
}

impl ParameterValue {
    /// 参数值的字符串形式（用于 `${}` 替换；与「机密值不参与插值」无冲突）。
    pub fn as_str(&self) -> String {
        match self {
            Self::String(s) => s.clone(),
            Self::Number(n) => n.to_string(),
            Self::Bool(b) => b.to_string(),
        }
    }
}

/// 环境变量键值对（ADR-0006：任务级覆盖 Pipeline 级同名项）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    /// 环境变量名。
    pub name: String,
    /// 环境变量值（可含 `${}` 引用，与 env 注入是两套机制，ADR-0006）。
    pub value: String,
}

/// 通知配置（ADR-0006/0008：pipeline 完成时发送；v1 仅邮件）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    /// 成功时是否发送（失败必发，不可配）。
    #[serde(default)]
    pub on_success: bool,
}

/// 阶段：按序执行、阶段内任务并行（ADR-0006）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stage {
    /// 阶段名。
    pub name: String,
    /// 阶段级 when 条件（ADR-0006：不满足则整个阶段跳过、其内任务全不发）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// 阶段内任务（并行执行）。
    #[serde(default)]
    pub jobs: Vec<Job>,
}

/// 任务：绑定到某个执行环境的一个执行单元（ADR-0006）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    /// 任务名。
    pub name: String,
    /// 执行环境：宿主机直跑（默认）或容器（ADR-0002/0018）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_env: Option<ExecutionEnv>,
    /// Agent 标签要求（AND 全集匹配，ADR-0008）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// 任务级 when（ADR-0006）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// 任务级环境变量：覆盖 Pipeline 级同名项（ADR-0006）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    /// 任务失败是否豁免 fail-fast（ADR-0006）。
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_failure: bool,
    /// 自动重试次数（ADR-0006，默认 0；重试 N 次后仍失败才算失败）。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub retry_count: u32,
    /// 超时分钟（ADR-0008，默认 0 = 无限）。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub timeout_minutes: u32,
    /// 产物上传声明（任务级、声明式，完成后上传，ADR-0006）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_uploads: Vec<ArtifactUpload>,
    /// 产物下载依赖（开始前拉取，仅限本次构建内其它任务，ADR-0006）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_downloads: Vec<ArtifactDownload>,
    /// 缓存声明（ADR-0012）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caches: Vec<CacheSpec>,
    /// 机密引用（ADR-0015：按名注入 env；快照只存名不存值）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// 步骤：按序执行，仅两种类型（ADR-0006）。
    #[serde(default)]
    pub steps: Vec<Step>,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// 执行环境（ADR-0002/0018）：宿主机直跑（默认）或容器（仅 image 字段）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "config")]
pub enum ExecutionEnv {
    /// 宿主机直跑（默认，零依赖）。
    #[default]
    Host,
    /// 容器后端：仅 image 一个字段（ADR-0018）。
    Container {
        /// 容器镜像名（如 `rust:1.97`）。
        image: String,
    },
}

/// 产物上传声明（任务级，完成后上传，ADR-0006）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactUpload {
    /// 产物名（下载时引用）。
    pub name: String,
    /// workspace 相对路径（声明式校验，ADR-0006）。
    pub path: String,
}

/// 产物下载依赖（开始前拉取，ADR-0006）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDownload {
    /// 来源任务（同 pipeline 内、本次构建内）。
    pub job: String,
    /// 产物名。
    pub name: String,
    /// 落盘 workspace 相对路径。
    pub path: String,
}

/// 缓存声明（ADR-0012）：key 模板 + workspace 相对路径 + 可选 files 哈希分量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSpec {
    /// key 模板，支持 `${}` 插值；禁用 `SISY_WORKSPACE`（ADR-0012）。
    pub key: String,
    /// workspace 相对路径列表（仅相对路径，ADR-0012）。
    #[serde(default)]
    pub paths: Vec<String>,
    /// 参与哈希的锁文件列表（仅精确路径，按声明顺序拼内容 sha256 前 12 位）。
    #[serde(default)]
    pub files: Vec<String>,
}

/// 步骤：任务内最小执行单元，仅两种类型（ADR-0006）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "config")]
pub enum Step {
    /// shell 步骤（ADR-0006：解释器默认 Unix→sh、Windows→pwsh 无则 cmd）。
    Shell {
        /// 要执行的命令。
        command: String,
        /// 解释器偏好（默认 Unix→sh、Windows→pwsh 无则 cmd）。
        shell: Option<Shell>,
        /// 步骤级 when（ADR-0006：三级统一挂 when；引用 SISY_WORKSPACE 保存校验报错，ADR-0009）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        when: Option<String>,
    },
    /// checkout scm（ADR-0016：检出项目绑定仓库到工作区根并钉到确切提交）。
    Checkout {
        /// git 子模块开关（默认开，ADR-0016）。
        #[serde(default, skip_serializing_if = "is_false")]
        submodules: bool,
        /// 步骤级 when（ADR-0006：三级统一挂 when；引用 SISY_WORKSPACE 保存校验报错，ADR-0009）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        when: Option<String>,
    },
}

impl Step {
    /// 步骤级 when 条件（ADR-0006：三级 when 统一挂载）。
    pub fn when(&self) -> Option<&str> {
        match self {
            Self::Shell { when, .. } | Self::Checkout { when, .. } => when.as_deref(),
        }
    }
}

/// shell 解释器偏好（ADR-0006：默认值语义，v1 不做严格约束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    /// `/bin/sh`（POSIX）。
    Sh,
    /// `bash`。
    Bash,
    /// `pwsh`（PowerShell Core）。
    Pwsh,
    /// `cmd`（Windows 传统）。
    Cmd,
}

/// 修订版本（ADR-0006：每次保存递增、记录操作人与时间）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    /// 递增的版本号（从 1 起）。
    pub number: u32,
    /// 操作人（永久保留，账号禁用不删除，ADR-0014）。
    pub operator: String,
    /// 操作时间（Unix 毫秒时间戳）。
    pub at_ms: i64,
}

impl Revision {
    /// 下一个修订版本号（number + 1）。
    pub fn next(&self, operator: String, at_ms: i64) -> Self {
        Self {
            number: self.number + 1,
            operator,
            at_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pipeline() -> Pipeline {
        Pipeline {
            name: "demo".into(),
            parameters: vec![Parameter {
                name: "target".into(),
                r#type: ParameterType::Enum,
                required: true,
                default: Some(ParameterValue::String("x86_64".into())),
                description: Some("构建目标".into()),
                choices: vec!["x86_64".into(), "aarch64".into()],
            }],
            env: vec![EnvVar {
                name: "CARGO_HOME".into(),
                value: "${SISY_WORKSPACE}/.cargo".into(),
            }],
            notification: Some(Notification { on_success: true }),
            stages: vec![Stage {
                name: "build".into(),
                when: None,
                jobs: vec![Job {
                    name: "compile".into(),
                    exec_env: Some(ExecutionEnv::Container {
                        image: "rust:1.97".into(),
                    }),
                    labels: vec!["sisyphus/os=linux".into()],
                    when: None,
                    env: vec![],
                    allow_failure: false,
                    retry_count: 2,
                    timeout_minutes: 30,
                    artifact_uploads: vec![ArtifactUpload {
                        name: "bin".into(),
                        path: "target/release/sisyphus".into(),
                    }],
                    artifact_downloads: vec![],
                    caches: vec![CacheSpec {
                        key: "cargo-${SISY_BRANCH}".into(),
                        paths: vec![".cargo".into()],
                        files: vec!["Cargo.lock".into()],
                    }],
                    secrets: vec!["DOCKER_TOKEN".into()],
                    steps: vec![Step::Shell {
                        command: "cargo build --release".into(),
                        shell: Some(Shell::Bash),
                        when: None,
                    }],
                }],
            }],
            revision: Some(Revision {
                number: 3,
                operator: "alice".into(),
                at_ms: 1_700_000_000_000,
            }),
        }
    }

    #[test]
    fn serde_json_round_trip_preserves_pipeline() {
        let pipeline = sample_pipeline();
        let json = serde_json::to_string(&pipeline).expect("serialize");
        let back: Pipeline = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(pipeline, back);
    }

    #[test]
    fn serde_round_trip_defaults_for_missing_fields() {
        // 阶段/任务等缺省字段反序列化为空集合/默认值（JSON 精简）。
        let json = r#"{"name":"min","stages":[]}"#;
        let pipeline: Pipeline = serde_json::from_str(json).expect("deserialize");
        assert!(pipeline.parameters.is_empty());
        assert!(pipeline.env.is_empty());
        assert_eq!(pipeline.revision, None);
    }

    #[test]
    fn execution_env_serializes_tagged() {
        let host = ExecutionEnv::Host;
        let json = serde_json::to_string(&host).expect("serialize");
        assert_eq!(json, r#"{"type":"host"}"#);

        let container = ExecutionEnv::Container {
            image: "rust:1.97".into(),
        };
        let json = serde_json::to_string(&container).expect("serialize");
        let back: ExecutionEnv = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(container, back);
    }

    #[test]
    fn step_serializes_tagged() {
        let shell = Step::Shell {
            command: "echo hi".into(),
            shell: None,
            when: None,
        };
        let json = serde_json::to_string(&shell).expect("serialize");
        let back: Step = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(shell, back);

        // 步骤级 when 随 config 内容序列化。
        let checkout = Step::Checkout {
            submodules: true,
            when: Some("${SISY_BRANCH} == \"main\"".into()),
        };
        let json = serde_json::to_string(&checkout).expect("serialize");
        let back: Step = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(checkout, back);
        assert_eq!(back.when(), Some("${SISY_BRANCH} == \"main\""));
    }

    #[test]
    fn revision_next_increments() {
        let rev = Revision {
            number: 3,
            operator: "alice".into(),
            at_ms: 1,
        };
        let next = rev.next("bob".into(), 2);
        assert_eq!(next.number, 4);
        assert_eq!(next.operator, "bob");
    }

    #[test]
    fn parameter_value_as_str() {
        assert_eq!(ParameterValue::String("x".into()).as_str(), "x");
        assert_eq!(ParameterValue::Number(42.0).as_str(), "42");
        assert_eq!(ParameterValue::Bool(true).as_str(), "true");
    }
}
