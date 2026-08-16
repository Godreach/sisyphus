//! Server 启动配置：CLI flag > `SISYPHUS_` 前缀环境变量 > config.toml > 内置默认
//! （ADR-0010）；日志级别与格式语义按 ADR-0019。

use std::path::PathBuf;

use serde::Deserialize;

/// REST API 监听地址默认值（ADR-0010：默认端口 8080）。
pub const DEFAULT_REST_ADDR: &str = "0.0.0.0:8080";
/// Agent gRPC 通道监听地址默认值（沿用 B1）。
pub const DEFAULT_GRPC_ADDR: &str = "127.0.0.1:50051";
/// 日志级别默认值（ADR-0019）。
pub const DEFAULT_LOG_LEVEL: &str = "info";
/// 日志格式默认值：stdout JSON（ADR-0019）。
pub const DEFAULT_LOG_FORMAT: &str = "json";
/// 用户自注册开关默认值（ADR-0014：默认关，内网由全局 admin 建号）。
pub const DEFAULT_REGISTRATION_ENABLED: bool = false;

/// 数据目录内的配置文件名。
pub const CONFIG_FILE_NAME: &str = "config.toml";
/// 数据目录内的 SQLite 数据库文件名（ADR-0010）。
pub const DB_FILE_NAME: &str = "sisyphus.db";
/// 数据目录内的产物存储子目录名（ADR-0004）。
pub const ARTIFACTS_DIR: &str = "artifacts";
/// 数据目录内的迁移前备份子目录名（ADR-0010）。
pub const BACKUPS_DIR: &str = "backups";
/// 数据目录内的静态资源本地覆盖子目录名（数据目录布局 ADR-0010；分层
/// 资产 ADR-0005，票 B2a-T5）：放入与内嵌产物同名的文件即压过内嵌版本；
/// 目录不存在即无覆盖。
pub const WEB_DIR: &str = "web";

/// 日志输出格式（ADR-0019：默认 stdout JSON，可切 pretty）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// JSON 行（Docker/收集器友好）。
    Json,
    /// pretty 行式（裸机人读）。
    Pretty,
}

/// 合并后的启动配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// 单一数据落点（ADR-0010）。
    pub data_dir: PathBuf,
    /// REST API 监听地址。
    pub rest_addr: String,
    /// Agent gRPC 通道监听地址。
    pub grpc_addr: String,
    /// 日志级别（trace/debug/info/warn/error）。
    pub log_level: String,
    /// 日志输出格式。
    pub log_format: LogFormat,
    /// 用户自注册开关（register 端点的门；默认关，票 B2b-T4）。
    pub registration_enabled: bool,
}

/// 同一形态的覆盖层：CLI flag 与 `SISYPHUS_` 环境变量都归约为它。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Overrides {
    /// REST 监听地址覆盖。
    pub rest_addr: Option<String>,
    /// gRPC 监听地址覆盖。
    pub grpc_addr: Option<String>,
    /// 日志级别覆盖。
    pub log_level: Option<String>,
    /// 日志格式覆盖。
    pub log_format: Option<String>,
    /// 注册开关覆盖（文本形态与各层统一，布尔语义在 merge 缝收口）。
    pub registration_enabled: Option<String>,
}

/// config.toml 文件层。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// `[server]` 段。
    #[serde(default)]
    pub server: ServerFile,
    /// `[log]` 段。
    #[serde(default)]
    pub log: LogFile,
    /// `[auth]` 段。
    #[serde(default)]
    pub auth: AuthFile,
}

/// `[server]` 段。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerFile {
    /// REST API 监听地址。
    pub rest_addr: Option<String>,
    /// Agent gRPC 通道监听地址。
    pub grpc_addr: Option<String>,
}

/// `[log]` 段。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogFile {
    /// 日志级别。
    pub level: Option<String>,
    /// 日志格式。
    pub format: Option<String>,
}

/// `[auth]` 段（票 B2b-T4 起有了认证相关文件配置）。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthFile {
    /// 用户自注册开关（默认关：register 403，内网由全局 admin 建号）。
    pub registration_enabled: Option<bool>,
}

/// 配置加载/合并错误。
#[derive(Debug)]
pub enum ConfigError {
    /// config.toml 语法或字段非法。
    InvalidToml(String),
    /// 监听地址不是合法的 host:port。
    InvalidAddr(String),
    /// 日志级别/格式取值非法。
    InvalidLogValue(String),
    /// 布尔开关取值非法（期望 true/false）。
    InvalidBool(String),
    /// 数据目录或配置文件 IO 失败。
    Io(std::io::Error),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::InvalidToml(e) => write!(f, "config.toml 非法：{e}"),
            ConfigError::InvalidAddr(a) => write!(f, "监听地址非法：{a}"),
            ConfigError::InvalidLogValue(v) => write!(f, "日志取值非法：{v}"),
            ConfigError::InvalidBool(v) => {
                write!(f, "布尔取值非法：{v}（期望 true/false）")
            }
            ConfigError::Io(e) => write!(f, "配置 IO 失败：{e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

/// 首次启动生成的带注释 config.toml 样例（ADR-0010）。
pub fn sample_toml() -> &'static str {
    r#"# sisyphus-server 配置文件（首次启动自动生成，修改后重启生效）
# 优先级：CLI flag > SISYPHUS_ 前缀环境变量 > 本文件 > 内置默认值。

[server]
# REST API 监听地址（默认 0.0.0.0:8080）
rest_addr = "0.0.0.0:8080"
# Agent gRPC 通道监听地址（默认 127.0.0.1:50051）
grpc_addr = "127.0.0.1:50051"

[log]
# 日志级别：trace / debug / info / warn / error（默认 info；RUST_LOG 若设置则整体胜出）
level = "info"
# 日志格式：json（默认，收集器友好）/ pretty（裸机人读）
format = "json"

[auth]
# 用户自注册开关（默认 false = 关闭：POST /auth/register 一律 403，账号由全局管理员建立；
# true 时用户可自注册非管理员账号。修改后重启生效）
registration_enabled = false
"#
}

/// 解析 config.toml 文本（未知字段报错，防手误静默失效）。
pub fn parse_toml(text: &str) -> Result<FileConfig, ConfigError> {
    toml::from_str(text).map_err(|e| ConfigError::InvalidToml(e.to_string()))
}

impl Config {
    /// 从数据目录加载启动配置：确保目录布局、生成/读取 config.toml、
    /// 与环境变量及 CLI 覆盖层合并（ADR-0010）。环境变量层由调用方注入
    /// （生产路径传 [`Overrides::from_env`]，测试传空层保证封闭）。
    pub fn load(
        data_dir: PathBuf,
        cli: Overrides,
        env: Overrides,
    ) -> Result<Config, ConfigError> {
        // 数据目录布局（ADR-0010）：数据库文件落在根，产物与迁移备份各占子目录。
        for sub in [ARTIFACTS_DIR, BACKUPS_DIR] {
            std::fs::create_dir_all(data_dir.join(sub))?;
        }

        // config.toml：不存在则生成带注释样例并立即以生成值继续（ADR-0010）。
        let path = data_dir.join(CONFIG_FILE_NAME);
        let file = if path.is_file() {
            parse_toml(&std::fs::read_to_string(&path)?)?
        } else {
            std::fs::write(&path, sample_toml())?;
            FileConfig::default()
        };

        merge(data_dir, &cli, &env, &file)
    }
}

impl Overrides {
    /// 读 `SISYPHUS_` 前缀环境变量层（ADR-0010；薄适配，语义在 merge 缝上测试）。
    pub fn from_env() -> Overrides {
        let get = |key: &str| {
            std::env::var(key)
                .ok()
                .filter(|v| !v.trim().is_empty())
        };
        Overrides {
            rest_addr: get("SISYPHUS_REST_ADDR"),
            grpc_addr: get("SISYPHUS_GRPC_ADDR"),
            log_level: get("SISYPHUS_LOG_LEVEL"),
            log_format: get("SISYPHUS_LOG_FORMAT"),
            registration_enabled: get("SISYPHUS_REGISTRATION_ENABLED"),
        }
    }
}

/// 按「CLI > 环境变量 > 文件 > 内置默认」合并出启动配置。
pub fn merge(
    data_dir: PathBuf,
    cli: &Overrides,
    env: &Overrides,
    file: &FileConfig,
) -> Result<Config, ConfigError> {
    let pick = |cli: &Option<String>, env: &Option<String>, file: Option<&str>| -> Option<String> {
        cli.clone()
            .or_else(|| env.clone())
            .or_else(|| file.map(str::to_string))
    };

    let rest_addr = pick(&cli.rest_addr, &env.rest_addr, file.server.rest_addr.as_deref())
        .unwrap_or_else(|| DEFAULT_REST_ADDR.to_string());
    let grpc_addr = pick(&cli.grpc_addr, &env.grpc_addr, file.server.grpc_addr.as_deref())
        .unwrap_or_else(|| DEFAULT_GRPC_ADDR.to_string());
    let log_level = pick(&cli.log_level, &env.log_level, file.log.level.as_deref())
        .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_string());
    let log_format =
        pick(&cli.log_format, &env.log_format, file.log.format.as_deref())
            .unwrap_or_else(|| DEFAULT_LOG_FORMAT.to_string());

    // 注册开关：CLI/env 覆盖层是文本（布尔语义在此收口），文件层是原生
    // toml 布尔，内置默认关（ADR-0014：内网由全局 admin 建号）。
    let registration_enabled =
        match pick(&cli.registration_enabled, &env.registration_enabled, None) {
            Some(value) => parse_bool(&value)?,
            None => file
                .auth
                .registration_enabled
                .unwrap_or(DEFAULT_REGISTRATION_ENABLED),
        };

    let log_format = match log_format.as_str() {
        "json" => LogFormat::Json,
        "pretty" => LogFormat::Pretty,
        other => return Err(ConfigError::InvalidLogValue(other.to_string())),
    };

    for addr in [&rest_addr, &grpc_addr] {
        addr.parse::<std::net::SocketAddr>()
            .map_err(|_| ConfigError::InvalidAddr(addr.clone()))?;
    }

    if !["trace", "debug", "info", "warn", "error"].contains(&log_level.as_str()) {
        return Err(ConfigError::InvalidLogValue(log_level));
    }

    Ok(Config {
        data_dir,
        rest_addr,
        grpc_addr,
        log_level,
        log_format,
        registration_enabled,
    })
}

/// 覆盖层文本的布尔解析（注册开关等）：true/false 大小写不敏感，另收
/// 1/0、yes/no、on/off 常见拼写；其余取值报错（不静默当 false——开关类
/// 配置拼错必须启动失败）。
fn parse_bool(value: &str) -> Result<bool, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(ConfigError::InvalidBool(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_returns_defaults_when_no_layer_overrides() {
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &Overrides::default(),
            &FileConfig::default(),
        )
        .expect("空覆盖层合并应成功");

        assert_eq!(cfg.data_dir, PathBuf::from("/tmp/data"));
        assert_eq!(cfg.rest_addr, DEFAULT_REST_ADDR);
        assert_eq!(cfg.grpc_addr, DEFAULT_GRPC_ADDR);
        assert_eq!(cfg.log_level, DEFAULT_LOG_LEVEL);
        assert_eq!(cfg.log_format, LogFormat::Json);
        assert!(!cfg.registration_enabled, "注册开关默认关（ADR-0014）");
    }

    #[test]
    fn cli_beats_env_beats_file_beats_default() {
        let cli = Overrides {
            grpc_addr: Some("127.0.0.1:60001".into()),
            registration_enabled: Some("false".into()),
            ..Overrides::default()
        };
        let env = Overrides {
            grpc_addr: Some("127.0.0.1:60002".into()),
            rest_addr: Some("127.0.0.1:60003".into()),
            registration_enabled: Some("true".into()),
            ..Overrides::default()
        };
        let file = FileConfig {
            server: ServerFile {
                rest_addr: Some("127.0.0.1:60004".into()),
                grpc_addr: Some("127.0.0.1:60005".into()),
            },
            log: LogFile {
                level: Some("debug".into()),
                format: Some("pretty".into()),
            },
            auth: AuthFile {
                registration_enabled: Some(true),
            },
        };

        let cfg = merge(PathBuf::from("/tmp/data"), &cli, &env, &file)
            .expect("各层齐全时合并应成功");

        // CLI 压过环境变量与文件。
        assert_eq!(cfg.grpc_addr, "127.0.0.1:60001");
        // 环境变量压过文件。
        assert_eq!(cfg.rest_addr, "127.0.0.1:60003");
        // 文件压过内置默认。
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.log_format, LogFormat::Pretty);
        // CLI 的 false 压过 env 与文件的 true（关得掉，不只是开得开）。
        assert!(!cfg.registration_enabled);
    }

    #[test]
    fn registration_switch_merges_across_layers_and_parses_bool_spellings() {
        let file_on = FileConfig {
            auth: AuthFile {
                registration_enabled: Some(true),
            },
            ..FileConfig::default()
        };
        let cfg = merge(PathBuf::from("/tmp/data"), &Overrides::default(), &Overrides::default(), &file_on)
            .expect("文件层开启应生效");
        assert!(cfg.registration_enabled, "文件层压过内置默认关");

        // env 层常见拼写全收（CLI/env 是文本，布尔语义在 merge 收口）。
        for (text, expected) in [
            ("true", true),
            ("TRUE", true),
            ("1", true),
            ("yes", true),
            ("on", true),
            ("false", false),
            ("0", false),
            ("no", false),
            ("off", false),
        ] {
            let env = Overrides {
                registration_enabled: Some(text.into()),
                ..Overrides::default()
            };
            let cfg = merge(
                PathBuf::from("/tmp/data"),
                &Overrides::default(),
                &env,
                &file_on,
            )
            .unwrap_or_else(|e| panic!("{text} 应解析成功：{e}"));
            assert_eq!(cfg.registration_enabled, expected, "env 层 {text}");
        }

        // 拼错的开关值：启动失败（不静默当 false）。
        let env = Overrides {
            registration_enabled: Some("tru".into()),
            ..Overrides::default()
        };
        assert!(matches!(
            merge(PathBuf::from("/tmp/data"), &Overrides::default(), &env, &file_on),
            Err(ConfigError::InvalidBool(_))
        ));
    }

    #[test]
    fn sample_toml_parses_to_built_in_defaults() {
        let file = parse_toml(sample_toml()).expect("样例必须是合法 toml");

        // 样例值与内置默认一致——「生成即用」不改变启动行为的前提。
        let cfg = merge(PathBuf::from("/tmp/data"), &Overrides::default(), &Overrides::default(), &file)
            .expect("样例合并应成功");
        assert_eq!(cfg.rest_addr, DEFAULT_REST_ADDR);
        assert_eq!(cfg.grpc_addr, DEFAULT_GRPC_ADDR);
        assert_eq!(cfg.log_level, DEFAULT_LOG_LEVEL);
        assert_eq!(cfg.log_format, LogFormat::Json);
        assert!(!cfg.registration_enabled, "样例值与内置默认一致");

        // 带注释：样例要能当作文档读。
        assert!(sample_toml().lines().any(|l| l.trim_start().starts_with('#')));
    }

    #[test]
    fn invalid_toml_and_unknown_field_are_rejected() {
        assert!(matches!(
            parse_toml("==== not toml"),
            Err(ConfigError::InvalidToml(_))
        ));
        assert!(matches!(
            parse_toml("[server]\nrest_addrs = \"127.0.0.1:1\"\n"),
            Err(ConfigError::InvalidToml(_))
        ));
    }

    #[test]
    fn invalid_addr_and_log_values_are_rejected() {
        let bad_addr = Overrides {
            rest_addr: Some("not-an-addr".into()),
            ..Overrides::default()
        };
        assert!(matches!(
            merge(PathBuf::from("/tmp/data"), &bad_addr, &Overrides::default(), &FileConfig::default()),
            Err(ConfigError::InvalidAddr(_))
        ));

        let bad_level = Overrides {
            log_level: Some("loud".into()),
            ..Overrides::default()
        };
        assert!(matches!(
            merge(PathBuf::from("/tmp/data"), &bad_level, &Overrides::default(), &FileConfig::default()),
            Err(ConfigError::InvalidLogValue(_))
        ));

        let bad_format = Overrides {
            log_format: Some("xml".into()),
            ..Overrides::default()
        };
        assert!(matches!(
            merge(PathBuf::from("/tmp/data"), &bad_format, &Overrides::default(), &FileConfig::default()),
            Err(ConfigError::InvalidLogValue(_))
        ));
    }

    #[test]
    fn load_on_empty_dir_creates_layout_and_uses_generated_values() {
        let dir = tempfile::tempdir().expect("临时目录");
        let cfg = Config::load(
            dir.path().to_path_buf(),
            Overrides::default(),
            Overrides::default(),
        )
        .expect("空数据目录首次加载应成功");

        // 目录布局（ADR-0010）：db 位 + artifacts/ + backups/。
        assert!(dir.path().join(CONFIG_FILE_NAME).is_file());
        assert!(dir.path().join(ARTIFACTS_DIR).is_dir());
        assert!(dir.path().join(BACKUPS_DIR).is_dir());

        // 生成即用：以生成值（与内置默认一致）继续，不要求二次启动。
        assert_eq!(cfg.data_dir, dir.path());
        assert_eq!(cfg.rest_addr, DEFAULT_REST_ADDR);
        assert_eq!(cfg.grpc_addr, DEFAULT_GRPC_ADDR);

        // 样例带注释，可当文档读。
        let written = std::fs::read_to_string(dir.path().join(CONFIG_FILE_NAME)).unwrap();
        assert_eq!(written, sample_toml());
    }

    #[test]
    fn load_reads_existing_toml() {
        let dir = tempfile::tempdir().expect("临时目录");
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[server]\ngrpc_addr = \"127.0.0.1:60010\"\n",
        )
        .unwrap();

        let cfg = Config::load(
            dir.path().to_path_buf(),
            Overrides::default(),
            Overrides::default(),
        )
        .expect("既有 config.toml 应被读取");

        assert_eq!(cfg.grpc_addr, "127.0.0.1:60010");
        assert_eq!(cfg.rest_addr, DEFAULT_REST_ADDR);
    }
}
