//! Pipeline 定义保存校验（ADR-0006 + 0011/0012/0015）。
//!
//! 编辑器与 Server 共用同一实现（单一事实源，ADR-0009）。坏定义在保存时
//! 报错而非运行期爆炸。校验错误信息明确、可定位到具体字段/规则。

use serde::{Deserialize, Serialize};

use crate::pipeline::{CacheSpec, ExecutionEnv, Job, ParameterType, Pipeline};
use crate::variables;
use crate::when;

/// 校验规则码：`validate.rs` 全部规则的稳定身份（单一事实源，ADR-0009）。
///
/// 14 条规则各一码。缓存 key 三条（空 / 过长 / 禁 `SISY_WORKSPACE`）共用字段路径
/// `caches[N].key`，按 `path` 无法区分——码是唯一稳定身份，前端实时校验与对账测试据此对账。
/// 序列化为 `snake_case` 字符串（前端生成镜像同形）。server 把 `ValidationError`
/// 重投影成自己的 `ValidationIssue{path,message}`（只拷贝两者，api/error.rs），故此码
/// **不进 wire/OpenAPI**——是 model 内部身份，专为前端实时反馈与对账。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    /// 必填参数必须带默认值（ADR-0006）。
    RequiredParameterDefault,
    /// enum 类型参数必须提供候选项。
    EnumChoices,
    /// when 表达式禁用 `${SISY_WORKSPACE}`（ADR-0009/0011）。
    WhenWorkspace,
    /// when 表达式语法不合法（ADR-0006 受限语法）。
    WhenSyntax,
    /// shell 步骤命令不能为空。
    ShellCommandEmpty,
    /// 容器执行环境必须指定 image（ADR-0018）。
    ContainerImageEmpty,
    /// 任务 env 键与机密名冲突（ADR-0015）。
    EnvSecretCollision,
    /// 产物上传需指定非空 name 与路径。
    ArtifactUploadEmpty,
    /// 产物上传路径必须是 workspace 相对路径。
    ArtifactUploadAbsolute,
    /// 缓存 key 不能为空（ADR-0012）。
    CacheKeyEmpty,
    /// 缓存 key 长度超过上限 255。
    CacheKeyTooLong,
    /// 缓存 key 禁用 `${SISY_WORKSPACE}`（ADR-0012）。
    CacheKeyWorkspace,
    /// 缓存 paths 仅允许 workspace 相对路径。
    CachePathNotRelative,
    /// 缓存 files 仅支持精确路径，不支持 glob。
    CacheFilesGlob,
}

/// 校验错误：`path` 为定位用的字段路径（如 `stages[0].jobs[1].caches[0].key`）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{path}: {message}")]
pub struct ValidationError {
    /// 字段定位路径（如 `stages[0].jobs[1].caches[0].key`）。
    pub path: String,
    /// 人类可读的错误描述。
    pub message: String,
    /// 规则码（稳定身份，前端对账据此，ADR-0009）。
    pub code: ValidationCode,
}

impl ValidationError {
    fn new(
        path: impl Into<String>,
        message: impl Into<String>,
        code: ValidationCode,
    ) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
            code,
        }
    }
}

/// 校验 Pipeline 定义；返回全部错误（不短路）。
pub fn validate(pipeline: &Pipeline) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    // Pipeline 级：必填参数必须带默认值（ADR-0006）
    for (i, p) in pipeline.parameters.iter().enumerate() {
        let path = format!("parameters[{i}]");
        if p.required && p.default.is_none() {
            errors.push(ValidationError::new(
                format!("{path}.{}.required", p.name),
                "必填参数必须带默认值（ADR-0006：所有触发方式统一「默认值，手动触发可覆盖」）",
                ValidationCode::RequiredParameterDefault,
            ));
        }
        if p.r#type == ParameterType::Enum && p.choices.is_empty() {
            errors.push(ValidationError::new(
                format!("{path}.{}.choices", p.name),
                "enum 类型参数必须提供候选项",
                ValidationCode::EnumChoices,
            ));
        }
    }

    // 阶段/任务：when 语法 + 禁 SISY_WORKSPACE（ADR-0009/0011）
    for (si, stage) in pipeline.stages.iter().enumerate() {
        validate_when(&mut errors, format!("stages[{si}]"), stage.when.as_deref());
        for (ji, job) in stage.jobs.iter().enumerate() {
            let job_path = format!("stages[{si}].jobs[{ji}]");
            validate_when(&mut errors, format!("{job_path}.when"), job.when.as_deref());
            validate_job(&mut errors, &job_path, job);
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_when(errors: &mut Vec<ValidationError>, path: String, source: Option<&str>) {
    let Some(source) = source else { return };
    // 禁 SISY_WORKSPACE（ADR-0009/0011）
    if source.contains("${SISY_WORKSPACE}") {
        errors.push(ValidationError::new(
            &path,
            "when 表达式禁用 ${SISY_WORKSPACE}（Agent 侧才可知其值，Server 端无法求值）",
            ValidationCode::WhenWorkspace,
        ));
    }
    // when 受限语法（解析失败即拒绝，ADR-0006）
    if let Err(e) = when::parse(source) {
        errors.push(ValidationError::new(
            &path,
            e.to_string(),
            ValidationCode::WhenSyntax,
        ));
    }
}

fn validate_job(errors: &mut Vec<ValidationError>, path: &str, job: &Job) {
    // 步骤级 when（ADR-0006/0009：三级统一挂 when；步骤级 when 引用
    // SISY_WORKSPACE 保存校验报错——Agent 端才可知其值，Server 端无法求值）。
    for (si, step) in job.steps.iter().enumerate() {
        validate_when(errors, format!("{path}.steps[{si}].when"), step.when());
        if let crate::pipeline::Step::Shell { command, .. } = step
            && command.trim().is_empty()
        {
            errors.push(ValidationError::new(
                format!("{path}.steps[{si}].command"),
                "shell 步骤命令不能为空",
                ValidationCode::ShellCommandEmpty,
            ));
        }
    }

    // 执行环境（ADR-0018：容器仅 image 字段，非空）
    if let Some(ExecutionEnv::Container { image }) = &job.exec_env
        && image.trim().is_empty()
    {
        errors.push(ValidationError::new(
            format!("{path}.exec_env.image"),
            "容器执行环境必须指定 image",
            ValidationCode::ContainerImageEmpty,
        ));
    }

    // env 键与机密名冲突（ADR-0015）
    let secret_names: std::collections::HashSet<&String> = job.secrets.iter().collect();
    for e in &job.env {
        if secret_names.contains(&e.name) {
            errors.push(ValidationError::new(
                format!("{path}.env.{}", e.name),
                "任务 env 键与机密名冲突（ADR-0015：机密经 env 注入，键名冲突）",
                ValidationCode::EnvSecretCollision,
            ));
        }
    }

    // 产物上传路径声明式校验（ADR-0006：非空、相对路径）
    for (ui, u) in job.artifact_uploads.iter().enumerate() {
        if u.name.trim().is_empty() || u.path.trim().is_empty() {
            errors.push(ValidationError::new(
                format!("{path}.artifact_uploads[{ui}]"),
                "产物上传需指定非空的 name 与 workspace 相对路径",
                ValidationCode::ArtifactUploadEmpty,
            ));
        }
        if is_absolute(&u.path) {
            errors.push(ValidationError::new(
                format!("{path}.artifact_uploads[{ui}].path"),
                "产物上传路径必须是 workspace 相对路径，不支持绝对路径",
                ValidationCode::ArtifactUploadAbsolute,
            ));
        }
    }

    // 缓存声明（ADR-0012）
    for (ci, cache) in job.caches.iter().enumerate() {
        validate_cache(errors, &format!("{path}.caches[{ci}]"), cache);
    }
}

fn validate_cache(errors: &mut Vec<ValidationError>, path: &str, cache: &CacheSpec) {
    // key 字面部分：非空、长度上限、字符集（ADR-0012）
    if cache.key.trim().is_empty() {
        errors.push(ValidationError::new(
            format!("{path}.key"),
            "缓存 key 不能为空",
            ValidationCode::CacheKeyEmpty,
        ));
    }
    if cache.key.len() > 255 {
        errors.push(ValidationError::new(
            format!("{path}.key"),
            "缓存 key 长度超过上限 255",
            ValidationCode::CacheKeyTooLong,
        ));
    }
    // key 禁 SISY_WORKSPACE（ADR-0012）
    if cache.key.contains("${SISY_WORKSPACE}") {
        errors.push(ValidationError::new(
            format!("{path}.key"),
            "缓存 key 禁用 ${SISY_WORKSPACE}（per-Agent 值会让 key 永不命中）",
            ValidationCode::CacheKeyWorkspace,
        ));
    }
    // paths 仅 workspace 相对路径（ADR-0012）
    for (pi, p) in cache.paths.iter().enumerate() {
        if is_absolute(p) || p.starts_with("..") {
            errors.push(ValidationError::new(
                format!("{path}.paths[{pi}]"),
                "缓存 paths 仅允许 workspace 相对路径",
                ValidationCode::CachePathNotRelative,
            ));
        }
    }
    // files 仅精确路径、不支持 glob（ADR-0012）
    for (fi, f) in cache.files.iter().enumerate() {
        if f.contains('*') || f.contains('?') {
            errors.push(ValidationError::new(
                format!("{path}.files[{fi}]"),
                "缓存 files 仅支持精确路径，不支持 glob",
                ValidationCode::CacheFilesGlob,
            ));
        }
    }
}

fn is_absolute(p: &str) -> bool {
    p.starts_with('/') || p.starts_with('\\')
}

/// 从 Pipeline 提取需要展开的变量引用集合（保存校验用）。
pub fn referenced_variables(input: &str) -> Vec<String> {
    // 简单扫描 ${name}（不含转义处理——校验时只看字面引用）
    let mut out = Vec::new();
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        if let Some(end_rel) = rest[start + 2..].find('}') {
            let name = &rest[start + 2..start + 2 + end_rel];
            if variables::is_valid_name(name) {
                out.push(name.to_string());
            }
            rest = &rest[start + 2 + end_rel..];
        } else {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 修订版本与快照
// ---------------------------------------------------------------------------

/// 构建快照：每次构建入库的整份 Pipeline 定义 JSON（含所用 revision，ADR-0006）。
/// 机密值永不进快照、只存任务声明的机密名列表（ADR-0015）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildSnapshot {
    /// 快照时点的 Pipeline 定义全文。
    pub pipeline: Pipeline,
    /// 所用修订版本。
    pub revision: crate::pipeline::Revision,
}

impl BuildSnapshot {
    /// 由 Pipeline 与当前 revision 构造快照。
    pub fn new(pipeline: Pipeline, revision: crate::pipeline::Revision) -> Self {
        Self { pipeline, revision }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::*;

    fn base_pipeline() -> Pipeline {
        Pipeline {
            name: "demo".into(),
            parameters: vec![],
            env: vec![],
            notification: None,
            stages: vec![Stage {
                name: "build".into(),
                when: None,
                jobs: vec![Job {
                    name: "compile".into(),
                    exec_env: None,
                    labels: vec![],
                    when: None,
                    env: vec![],
                    allow_failure: false,
                    retry_count: 0,
                    timeout_minutes: 0,
                    artifact_uploads: vec![],
                    artifact_downloads: vec![],
                    caches: vec![],
                    secrets: vec![],
                    steps: vec![],
                }],
            }],
            revision: None,
        }
    }

    #[test]
    fn accepts_valid_pipeline() {
        assert!(validate(&base_pipeline()).is_ok());
    }

    #[test]
    fn required_parameter_must_have_default() {
        let mut p = base_pipeline();
        p.parameters.push(Parameter {
            name: "target".into(),
            r#type: ParameterType::String,
            required: true,
            default: None,
            description: None,
            choices: vec![],
        });
        let errs = validate(&p).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.message.contains("必填参数必须带默认值"))
        );
        assert!(errs.iter().any(|e| e.code == ValidationCode::RequiredParameterDefault));
    }

    #[test]
    fn required_parameter_with_default_is_ok() {
        let mut p = base_pipeline();
        p.parameters.push(Parameter {
            name: "target".into(),
            r#type: ParameterType::String,
            required: true,
            default: Some(ParameterValue::String("x86_64".into())),
            description: None,
            choices: vec![],
        });
        assert!(validate(&p).is_ok());
    }

    #[test]
    fn enum_parameter_requires_choices() {
        let mut p = base_pipeline();
        p.parameters.push(Parameter {
            name: "os".into(),
            r#type: ParameterType::Enum,
            required: false,
            default: Some(ParameterValue::String("linux".into())),
            description: None,
            choices: vec![],
        });
        let errs = validate(&p).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("必须提供候选项")));
        assert!(errs.iter().any(|e| e.code == ValidationCode::EnumChoices));
    }

    #[test]
    fn rejects_workspace_in_when() {
        let mut p = base_pipeline();
        p.stages[0].when = Some("${SISY_WORKSPACE} == \"/x\"".into());
        let errs = validate(&p).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.message.contains("禁用 ${SISY_WORKSPACE}"))
        );
        assert!(errs.iter().any(|e| e.code == ValidationCode::WhenWorkspace));
    }

    #[test]
    fn rejects_bad_when_syntax() {
        let mut p = base_pipeline();
        p.stages[0].when = Some("(a == \"b\"".into());
        let errs = validate(&p).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("when 表达式")));
        assert!(errs.iter().any(|e| e.code == ValidationCode::WhenSyntax));
    }

    #[test]
    fn rejects_empty_when() {
        // 空 when 串 tokenize 无 token → parse_primary 缺操作数 → 拒绝（与「无 when」不同）。
        let mut p = base_pipeline();
        p.stages[0].when = Some("".into());
        let errs = validate(&p).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("when 表达式")));
        assert!(errs.iter().any(|e| e.code == ValidationCode::WhenSyntax));
    }

    #[test]
    fn rejects_workspace_in_step_when() {
        let mut p = base_pipeline();
        p.stages[0].jobs[0].steps.push(Step::Shell {
            command: "echo hi".into(),
            shell: None,
            when: Some("${SISY_WORKSPACE} == \"/x\"".into()),
        });
        let errs = validate(&p).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.message.contains("禁用 ${SISY_WORKSPACE}"))
        );
        assert!(errs.iter().any(|e| e.code == ValidationCode::WhenWorkspace));
    }

    #[test]
    fn rejects_empty_shell_command() {
        let mut p = base_pipeline();
        p.stages[0].jobs[0].steps.push(Step::Shell {
            command: "   ".into(),
            shell: None,
            when: None,
        });
        let errs = validate(&p).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("命令不能为空")));
        assert!(errs.iter().any(|e| e.code == ValidationCode::ShellCommandEmpty));
    }

    #[test]
    fn rejects_empty_container_image() {
        let mut p = base_pipeline();
        p.stages[0].jobs[0].exec_env = Some(ExecutionEnv::Container { image: "  ".into() });
        let errs = validate(&p).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("必须指定 image")));
        assert!(errs.iter().any(|e| e.code == ValidationCode::ContainerImageEmpty));
    }

    #[test]
    fn rejects_workspace_in_cache_key() {
        let mut p = base_pipeline();
        p.stages[0].jobs[0].caches.push(CacheSpec {
            key: "${SISY_WORKSPACE}/cache".into(),
            paths: vec![],
            files: vec![],
        });
        let errs = validate(&p).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("缓存 key 禁用")));
        assert!(errs.iter().any(|e| e.code == ValidationCode::CacheKeyWorkspace));
    }

    #[test]
    fn rejects_empty_cache_key() {
        let mut p = base_pipeline();
        p.stages[0].jobs[0].caches.push(CacheSpec {
            key: "  ".into(),
            paths: vec![],
            files: vec![],
        });
        let errs = validate(&p).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("缓存 key 不能为空")));
        assert!(errs.iter().any(|e| e.code == ValidationCode::CacheKeyEmpty));
    }

    #[test]
    fn rejects_too_long_cache_key() {
        // >255 UTF-8 字节即拒；多字节字符（128「中」= 384 字节）显式覆盖字节口径。
        let mut p = base_pipeline();
        p.stages[0].jobs[0].caches.push(CacheSpec {
            key: "中".repeat(128),
            paths: vec![],
            files: vec![],
        });
        let errs = validate(&p).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("长度超过上限 255")));
        assert!(errs.iter().any(|e| e.code == ValidationCode::CacheKeyTooLong));
    }

    #[test]
    fn rejects_absolute_cache_path() {
        let mut p = base_pipeline();
        p.stages[0].jobs[0].caches.push(CacheSpec {
            key: "k".into(),
            paths: vec!["/home/me/.cargo".into()],
            files: vec![],
        });
        let errs = validate(&p).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.message.contains("仅允许 workspace 相对路径"))
        );
        assert!(errs.iter().any(|e| e.code == ValidationCode::CachePathNotRelative));
    }

    #[test]
    fn rejects_parent_relative_cache_path() {
        // `..` 起首亦非 workspace 相对路径（同条规则，覆盖另一分支）。
        let mut p = base_pipeline();
        p.stages[0].jobs[0].caches.push(CacheSpec {
            key: "k".into(),
            paths: vec!["../escape".into()],
            files: vec![],
        });
        let errs = validate(&p).unwrap_err();
        assert!(errs.iter().any(|e| e.code == ValidationCode::CachePathNotRelative));
    }

    #[test]
    fn rejects_glob_in_cache_files() {
        let mut p = base_pipeline();
        p.stages[0].jobs[0].caches.push(CacheSpec {
            key: "k".into(),
            paths: vec![],
            files: vec!["Cargo.lock".into(), "target/*".into()],
        });
        let errs = validate(&p).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("不支持 glob")));
        assert!(errs.iter().any(|e| e.code == ValidationCode::CacheFilesGlob));
    }

    #[test]
    fn rejects_env_secret_name_collision() {
        let mut p = base_pipeline();
        p.stages[0].jobs[0].secrets = vec!["DOCKER_TOKEN".into()];
        p.stages[0].jobs[0].env = vec![EnvVar {
            name: "DOCKER_TOKEN".into(),
            value: "x".into(),
        }];
        let errs = validate(&p).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("机密名冲突")));
        assert!(errs.iter().any(|e| e.code == ValidationCode::EnvSecretCollision));
    }

    #[test]
    fn rejects_empty_artifact_upload_name() {
        // 仅 name 空、path 合法相对路径 → 触 R8（ArtifactUploadEmpty）不触 R9（绝对路径）。
        let mut p = base_pipeline();
        p.stages[0].jobs[0].artifact_uploads.push(ArtifactUpload {
            name: "  ".into(),
            path: "rel/path".into(),
        });
        let errs = validate(&p).unwrap_err();
        assert!(errs.iter().any(|e| e.code == ValidationCode::ArtifactUploadEmpty));
        assert!(!errs.iter().any(|e| e.code == ValidationCode::ArtifactUploadAbsolute));
    }

    #[test]
    fn rejects_absolute_artifact_upload_path() {
        let mut p = base_pipeline();
        p.stages[0].jobs[0].artifact_uploads.push(ArtifactUpload {
            name: "bin".into(),
            path: "/absolute/path".into(),
        });
        let errs = validate(&p).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.message.contains("workspace 相对路径"))
        );
        assert!(errs.iter().any(|e| e.code == ValidationCode::ArtifactUploadAbsolute));
    }

    #[test]
    fn referenced_variables_scans() {
        let v = referenced_variables("a=${SISY_BRANCH} b=${target} c=${MISSING}");
        assert_eq!(v, vec!["SISY_BRANCH", "target", "MISSING"]);
    }
}
