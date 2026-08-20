//! 校验对账样本集（ADR-0009，票 B4-T7）。
//!
//! 一组合法 + 各规则破坏样本，每条带声明的 `expected_codes`（规则码 multiset）。
//! 自检 `#[test]`（见 `main.rs`）跑 `validate()` 断言 Rust 标记的码 == 声明码——
//! 抓 model 漂移与脏样本（某样本本应只破一条规则却意外共触发）。
//!
//! 破坏样本遵循「单规则隔离」：除目标规则外其它字段皆合法，避免共触发噪音。
//! 一个 `co_r8_r9` 样本故意双触发，验证 multiset（带计数）比较能抓多码样本。
//!
//! 样本既是 `reconcile.fixtures.json` 的来源（前端对账测试消费其 serde JSON 与
//! 声明码），也供 `pipeline.snapshot.ts` 选用（`snapshot == true` 的合法样本
//! 的 serde 形态作为类型化字面量，vue-tsc 据此对账 TS 类型与 serde 形态）。

use sisyphus_model::pipeline::*;
use sisyphus_model::validate::ValidationCode;

/// 一条对账样本。
pub struct Sample {
    /// 样本 id（fixtures `id` 与快照 `export const` 名）。
    pub id: &'static str,
    /// Pipeline 定义值。
    pub pipeline: Pipeline,
    /// 是否合法（无校验错）。
    pub valid: bool,
    /// 声明的期望规则码（multiset，顺序无关但计数敏感）。
    pub expected_codes: &'static [ValidationCode],
    /// 是否纳入 `pipeline.snapshot.ts` 的类型化字面量（仅合法样本，覆盖全变体）。
    pub snapshot: bool,
}

/// 14 条规则码的规范序（`codes.ts` 的 `VALIDATION_CODES` 与 fixtures `rules` 同序）。
pub const ALL_CODES: [ValidationCode; 14] = [
    ValidationCode::RequiredParameterDefault,
    ValidationCode::EnumChoices,
    ValidationCode::WhenWorkspace,
    ValidationCode::WhenSyntax,
    ValidationCode::ShellCommandEmpty,
    ValidationCode::ContainerImageEmpty,
    ValidationCode::EnvSecretCollision,
    ValidationCode::ArtifactUploadEmpty,
    ValidationCode::ArtifactUploadAbsolute,
    ValidationCode::CacheKeyEmpty,
    ValidationCode::CacheKeyTooLong,
    ValidationCode::CacheKeyWorkspace,
    ValidationCode::CachePathNotRelative,
    ValidationCode::CacheFilesGlob,
];

/// 最小合法 Pipeline（一个阶段、一个全默认任务）——作为各破坏样本的底座。
fn base() -> Pipeline {
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

/// 全变体合法 Pipeline——快照用，覆盖每个枚举/Option 的 present 分支。
fn full() -> Pipeline {
    Pipeline {
        name: "demo".into(),
        parameters: vec![
            Parameter {
                name: "p_str".into(),
                r#type: ParameterType::String,
                required: false,
                default: Some(ParameterValue::String("x".into())),
                description: Some("字符串参数".into()),
                choices: vec![],
            },
            Parameter {
                name: "p_num".into(),
                r#type: ParameterType::Number,
                required: true,
                default: Some(ParameterValue::Number(2.5)),
                description: None,
                choices: vec![],
            },
            Parameter {
                name: "p_bool".into(),
                r#type: ParameterType::Bool,
                required: false,
                default: Some(ParameterValue::Bool(true)),
                description: None,
                choices: vec![],
            },
            Parameter {
                name: "p_enum".into(),
                r#type: ParameterType::Enum,
                required: false,
                default: Some(ParameterValue::String("a".into())),
                description: None,
                choices: vec!["a".into(), "b".into()],
            },
            // default: None → serde 省略；快照据此对账 `default?:` 缺省方向。
            Parameter {
                name: "p_nodefault".into(),
                r#type: ParameterType::String,
                required: false,
                default: None,
                description: None,
                choices: vec![],
            },
        ],
        env: vec![EnvVar {
            name: "CARGO_HOME".into(),
            value: "${SISY_WORKSPACE}/.cargo".into(),
        }],
        notification: Some(Notification {
            on_success: true,
            recipients: vec!["dev@example.com".into(), "ops@example.com".into()],
        }),
        stages: vec![Stage {
            name: "build".into(),
            when: Some("${SISY_BRANCH} == \"main\"".into()),
            jobs: vec![
                Job {
                    name: "compile".into(),
                    exec_env: Some(ExecutionEnv::Container {
                        image: "rust:1.97".into(),
                    }),
                    labels: vec!["sisyphus/os=linux".into()],
                    when: Some("exists SISY_COMMIT_ID".into()),
                    env: vec![EnvVar {
                        name: "RUSTFLAGS".into(),
                        value: "-D warnings".into(),
                    }],
                    allow_failure: true,
                    retry_count: 2,
                    timeout_minutes: 30,
                    artifact_uploads: vec![ArtifactUpload {
                        name: "bin".into(),
                        path: "target/release/sisyphus".into(),
                    }],
                    artifact_downloads: vec![ArtifactDownload {
                        job: "prep".into(),
                        name: "vendor".into(),
                        path: "vendor".into(),
                    }],
                    caches: vec![CacheSpec {
                        key: "cargo-${SISY_BRANCH}".into(),
                        paths: vec![".cargo".into()],
                        files: vec!["Cargo.lock".into()],
                    }],
                    secrets: vec!["DOCKER_TOKEN".into()],
                    steps: vec![
                        Step::Shell {
                            command: "cargo build --release".into(),
                            shell: Some(Shell::Bash),
                            when: None,
                        },
                        Step::Shell {
                            command: "cargo test".into(),
                            shell: None,
                            when: Some("${SISY_BRANCH} == \"main\"".into()),
                        },
                        Step::Checkout {
                            submodules: true,
                            when: None,
                        },
                        Step::Checkout {
                            submodules: false,
                            when: None,
                        },
                    ],
                },
                Job {
                    name: "prep".into(),
                    // 显式 Host 覆盖 `{ type: 'host' }` tagged 形态（无 config）。
                    exec_env: Some(ExecutionEnv::Host),
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
                },
                Job {
                    name: "lint".into(),
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
                    // 覆盖剩余 Shell 变体（sh/pwsh/cmd）——snapshot 据此对账枚举形态。
                    steps: vec![
                        Step::Shell {
                            command: "sh -c 'echo sh'".into(),
                            shell: Some(Shell::Sh),
                            when: None,
                        },
                        Step::Shell {
                            command: "pwsh -c 'echo pwsh'".into(),
                            shell: Some(Shell::Pwsh),
                            when: None,
                        },
                        Step::Shell {
                            command: "cmd /c echo cmd".into(),
                            shell: Some(Shell::Cmd),
                            when: None,
                        },
                    ],
                },
            ],
        }],
        revision: Some(Revision {
            number: 3,
            operator: "alice".into(),
            at_ms: 1_700_000_000_000,
        }),
    }
}

/// 全默认 Pipeline——快照用，覆盖 Option 的 absent 分支（notification/revision 与
/// 任务级 exec_env/when/allow_failure/retry_count/timeout_minutes 全缺省）。
fn minimal() -> Pipeline {
    Pipeline {
        name: "min".into(),
        parameters: vec![],
        env: vec![],
        notification: None,
        stages: vec![Stage {
            name: "s".into(),
            when: None,
            jobs: vec![Job {
                name: "j".into(),
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

/// 全部对账样本（合法 + 各规则破坏 + 一个共触发）。
pub fn samples() -> Vec<Sample> {
    let mut out: Vec<Sample> = Vec::new();

    // 合法样本（也作快照）。
    out.push(Sample {
        id: "valid_minimal",
        pipeline: minimal(),
        valid: true,
        expected_codes: &[],
        snapshot: true,
    });
    out.push(Sample {
        id: "valid_full",
        pipeline: full(),
        valid: true,
        expected_codes: &[],
        snapshot: true,
    });

    // R1：必填参数无默认值。
    let mut p = base();
    p.parameters.push(Parameter {
        name: "target".into(),
        r#type: ParameterType::String,
        required: true,
        default: None,
        description: None,
        choices: vec![],
    });
    out.push(Sample {
        id: "r1_required_no_default",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::RequiredParameterDefault],
        snapshot: false,
    });

    // R2：enum 参数无候选项。
    let mut p = base();
    p.parameters.push(Parameter {
        name: "os".into(),
        r#type: ParameterType::Enum,
        required: false,
        default: Some(ParameterValue::String("linux".into())),
        description: None,
        choices: vec![],
    });
    out.push(Sample {
        id: "r2_enum_no_choices",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::EnumChoices],
        snapshot: false,
    });

    // R3：when 含 SISY_WORKSPACE（语法合法 → 仅触 R3，不触 R4）。
    let mut p = base();
    p.stages[0].when = Some("${SISY_WORKSPACE} == \"/x\"".into());
    out.push(Sample {
        id: "r3_when_workspace",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::WhenWorkspace],
        snapshot: false,
    });

    // R4：when 语法错（无 SISY_WORKSPACE → 仅触 R4，不触 R3）。
    let mut p = base();
    p.stages[0].when = Some("(a == \"b\"".into());
    out.push(Sample {
        id: "r4_when_syntax",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::WhenSyntax],
        snapshot: false,
    });

    // R5：shell 步骤命令为空（仅空白）。
    let mut p = base();
    p.stages[0].jobs[0].steps.push(Step::Shell {
        command: "   ".into(),
        shell: None,
        when: None,
    });
    out.push(Sample {
        id: "r5_shell_empty",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::ShellCommandEmpty],
        snapshot: false,
    });

    // R6：容器执行环境 image 为空。
    let mut p = base();
    p.stages[0].jobs[0].exec_env = Some(ExecutionEnv::Container {
        image: "  ".into(),
    });
    out.push(Sample {
        id: "r6_container_no_image",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::ContainerImageEmpty],
        snapshot: false,
    });

    // R7：env 键与机密名冲突。
    let mut p = base();
    p.stages[0].jobs[0].secrets = vec!["DOCKER_TOKEN".into()];
    p.stages[0].jobs[0].env = vec![EnvVar {
        name: "DOCKER_TOKEN".into(),
        value: "x".into(),
    }];
    out.push(Sample {
        id: "r7_env_secret_collision",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::EnvSecretCollision],
        snapshot: false,
    });

    // R8：产物上传 name 为空（path 合法相对路径 → 仅触 R8）。
    let mut p = base();
    p.stages[0].jobs[0].artifact_uploads.push(ArtifactUpload {
        name: "  ".into(),
        path: "rel/path".into(),
    });
    out.push(Sample {
        id: "r8_artifact_empty",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::ArtifactUploadEmpty],
        snapshot: false,
    });

    // R9：产物上传绝对路径（name 非空 → 不触 R8，仅触 R9）。
    let mut p = base();
    p.stages[0].jobs[0].artifact_uploads.push(ArtifactUpload {
        name: "bin".into(),
        path: "/absolute/path".into(),
    });
    out.push(Sample {
        id: "r9_artifact_absolute",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::ArtifactUploadAbsolute],
        snapshot: false,
    });

    // R10：缓存 key 为空。
    let mut p = base();
    p.stages[0].jobs[0].caches.push(CacheSpec {
        key: "  ".into(),
        paths: vec![],
        files: vec![],
    });
    out.push(Sample {
        id: "r10_cache_key_empty",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::CacheKeyEmpty],
        snapshot: false,
    });

    // R11：缓存 key 超长（>255 UTF-8 字节）。用多字节字符：128 个「中」= 128 UTF-16 码元
    // （≤255，旧 TS `.length` 误判合法）但 384 UTF-8 字节（>255，Rust `str::len` 拒绝）。
    // 锁定 byte-length 对账——若 TS 退回 `.length` 会漏判，与 Rust 分叉。
    let mut p = base();
    p.stages[0].jobs[0].caches.push(CacheSpec {
        key: "中".repeat(128),
        paths: vec![],
        files: vec![],
    });
    out.push(Sample {
        id: "r11_cache_key_too_long",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::CacheKeyTooLong],
        snapshot: false,
    });

    // R12：缓存 key 含 SISY_WORKSPACE。
    let mut p = base();
    p.stages[0].jobs[0].caches.push(CacheSpec {
        key: "${SISY_WORKSPACE}/cache".into(),
        paths: vec![],
        files: vec![],
    });
    out.push(Sample {
        id: "r12_cache_key_workspace",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::CacheKeyWorkspace],
        snapshot: false,
    });

    // R13：缓存 paths 绝对路径。
    let mut p = base();
    p.stages[0].jobs[0].caches.push(CacheSpec {
        key: "k".into(),
        paths: vec!["/home/me/.cargo".into()],
        files: vec![],
    });
    out.push(Sample {
        id: "r13_cache_path_absolute",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::CachePathNotRelative],
        snapshot: false,
    });

    // R13 第二分支：缓存 paths 以 `..` 起首（亦非 workspace 相对路径）。覆盖 TS
    // `startsWith('..')` 分支——前一样本只走 `isAbsolute` 分支，对账缝会漏掉 `..`。
    let mut p = base();
    p.stages[0].jobs[0].caches.push(CacheSpec {
        key: "k".into(),
        paths: vec!["../escape".into()],
        files: vec![],
    });
    out.push(Sample {
        id: "r13_cache_path_parent_relative",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::CachePathNotRelative],
        snapshot: false,
    });

    // R14：缓存 files 含 glob。
    let mut p = base();
    p.stages[0].jobs[0].caches.push(CacheSpec {
        key: "k".into(),
        paths: vec![],
        files: vec!["target/*".into()],
    });
    out.push(Sample {
        id: "r14_cache_files_glob",
        pipeline: p,
        valid: false,
        expected_codes: &[ValidationCode::CacheFilesGlob],
        snapshot: false,
    });

    // 共触发样本：R8（name 空）+ R9（绝对路径）——验证 multiset 带计数比较。
    let mut p = base();
    p.stages[0].jobs[0].artifact_uploads.push(ArtifactUpload {
        name: "  ".into(),
        path: "/absolute/path".into(),
    });
    out.push(Sample {
        id: "co_r8_r9",
        pipeline: p,
        valid: false,
        expected_codes: &[
            ValidationCode::ArtifactUploadEmpty,
            ValidationCode::ArtifactUploadAbsolute,
        ],
        snapshot: false,
    });

    out
}
