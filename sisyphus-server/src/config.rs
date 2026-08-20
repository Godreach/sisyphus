//! Server 启动配置：CLI flag > `SISYPHUS_` 前缀环境变量 > config.toml > 内置默认
//! （ADR-0010）；日志级别与格式语义按 ADR-0019。

use std::path::{Path, PathBuf};

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
/// orphan 宽限默认值（分钟，ADR-0008：Agent 离线后运行中任务 unknown 宽限
/// 超时判失败；默认 10 分钟，config `[scheduler]` 可覆盖）。
pub const DEFAULT_ORPHAN_GRACE_MINUTES: i64 = 10;
/// poll 触发器轮询节奏默认值（分钟，ADR-0016 票 #14 已定：项目级默认 5
/// 分钟）。新建 poll 触发器未显式给节奏时取此默认值，进触发器 spec；config
/// `[triggers]` 可覆盖。
pub const DEFAULT_POLL_INTERVAL_MINUTES: i64 = 5;
/// 日志与产物共享的 per-build 保留期默认值（天，ADR-0013/0004：默认 30
/// 天，Server 全局配置，config `[retention]` 可覆盖；每日低频扫描清理
/// 过期构建的日志 chunk 与产物，构建记录永久保留，B5-T6）。
pub const DEFAULT_RETENTION_DAYS: i64 = 30;
/// `/metrics` 端点鉴权默认值（ADR-0019：默认开——Bearer PAT 任意登录角色，
/// 运维可为 Prometheus 专建 viewer 用户；config `[metrics] auth = false` 可
/// 关，文档注明仅限可信内网。与业务路由同端口，不单开）。
pub const DEFAULT_METRICS_AUTH: bool = true;

/// 数据目录内的配置文件名。
pub const CONFIG_FILE_NAME: &str = "config.toml";
/// 数据目录内的 SQLite 数据库文件名（ADR-0010）。
pub const DB_FILE_NAME: &str = "sisyphus.db";
/// 数据目录内的主密钥文件名（ADR-0015：默认落数据目录，路径可经 config
/// 改到独立卷；首启自动生成 32 字节随机文件）。
pub const MASTER_KEY_FILE_NAME: &str = "master.key";
/// 数据目录内的产物存储子目录名（ADR-0004）。
pub const ARTIFACTS_DIR: &str = "artifacts";
/// 数据目录内的升级包存储子目录名（ADR-0017：管理员上传的 agent 发行包）。
pub const UPGRADE_PACKAGES_DIR: &str = "upgrade-packages";
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
    /// 主密钥文件路径（ADR-0015：默认数据目录内 `master.key`，可经 config
    /// 改到独立卷；首启自动生成、已有文件不重生成，票 B2b-T6）。
    pub master_key_path: PathBuf,
    /// orphan 宽限分钟（ADR-0008：Agent 离线后运行中任务 unknown 的宽限；
    /// 默认 10 分钟）。
    pub orphan_grace_minutes: i64,
    /// poll 触发器轮询节奏默认分钟（ADR-0016：项目级默认 5 分钟；新建 poll
    /// 触发器未显式给节奏时取此值，进触发器 spec）。
    pub poll_interval_minutes: i64,
    /// 日志与产物共享的 per-build 保留期天数（ADR-0013/0004：默认 30 天，
    /// 每日低频扫描清理过期构建的日志 chunk 与产物，构建记录永久保留）。
    pub retention_days: i64,
    /// `/metrics` 端点鉴权开关（ADR-0019：默认开；config `[metrics] auth =
    /// false` 可关——仅限可信内网，文档注明）。
    pub metrics_auth: bool,
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
    /// 主密钥文件路径覆盖（文本形态；相对路径按相对数据目录解析）。
    pub master_key_path: Option<String>,
    /// 保留期天数覆盖（文本形态；整数语义在 merge 缝收口，ADR-0013）。
    pub retention_days: Option<String>,
    /// `/metrics` 鉴权开关覆盖（文本形态；布尔语义在 merge 缝收口，
    /// ADR-0019——开关类配置拼错必须启动失败）。
    pub metrics_auth: Option<String>,
}

/// 覆盖层与文件层的保留期天数统一合并：CLI > env > 文件 > 默认（票 #78，
/// ADR-0013 全链可配——与 orphan/poll 不同，保留期按 Spec 明确进优先级链）。
/// CLI/env 层是文本（整数语义在此收口）、文件层是原生 toml 整数，两层
/// 皆须 >= 1——非法取值启动失败（不静默取默认：运维旋钮拼错必须暴露）。
fn merge_retention_days(
    cli: &Option<String>,
    env: &Option<String>,
    file: Option<i64>,
) -> Result<i64, ConfigError> {
    if let Some(days) = file
        && days < 1
    {
        return Err(ConfigError::InvalidLogValue(format!(
            "保留期天数须 >= 1，得到：{days}"
        )));
    }
    match cli
        .clone()
        .or_else(|| env.clone())
        .map(|s| parse_days(&s))
        .transpose()?
    {
        Some(days) => Ok(days),
        None => Ok(file.unwrap_or(DEFAULT_RETENTION_DAYS)),
    }
}

/// 覆盖层文本的天数解析：正整数（>= 1）；非法取值报错（防手误静默失效）。
fn parse_days(value: &str) -> Result<i64, ConfigError> {
    match value.trim().parse::<i64>() {
        Ok(days) if days >= 1 => Ok(days),
        Ok(_) => Err(ConfigError::InvalidLogValue(format!(
            "保留期天数须 >= 1，得到：{value}"
        ))),
        Err(_) => Err(ConfigError::InvalidLogValue(format!(
            "保留期天数非法：{value}（期望正整数）"
        ))),
    }
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
    /// `[scheduler]` 段（ADR-0008：调度时间语义）。
    #[serde(default)]
    pub scheduler: SchedulerFile,
    /// `[triggers]` 段（ADR-0016：触发器节奏默认）。
    #[serde(default)]
    pub triggers: TriggersFile,
    /// `[retention]` 段（ADR-0013：日志与产物共享 per-build 保留期）。
    #[serde(default)]
    pub retention: RetentionFile,
    /// `[metrics]` 段（ADR-0019：/metrics 鉴权开关）。
    #[serde(default)]
    pub metrics: MetricsFile,
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

/// `[auth]` 段（票 B2b-T4 起有了认证相关文件配置；票 B2b-T6 增机密键）。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthFile {
    /// 用户自注册开关（默认关：register 403，内网由全局 admin 建号）。
    pub registration_enabled: Option<bool>,
    /// 主密钥文件路径（相对路径按相对数据目录解析；默认数据目录内
    /// `master.key`，ADR-0015：可改到独立卷做运维纵深）。
    pub master_key_path: Option<PathBuf>,
}

/// `[scheduler]` 段（ADR-0008：调度时间语义；B2c-T4 起）。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerFile {
    /// orphan 宽限分钟（Agent 离线后运行中任务 unknown 的宽限，超时判失败；
    /// 默认 10 分钟）。
    pub orphan_grace_minutes: Option<i64>,
}

/// `[triggers]` 段（ADR-0016，票 B2c-T6：触发器节奏默认；新建触发器未显式
/// 给值时取此）。无 CLI/env 覆盖层——节奏是触发器级配置的默认底，非
/// 运行时调度旋钮。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggersFile {
    /// poll 触发器轮询节奏默认分钟（项目级默认 5 分钟，ADR-0016 票 #14）。
    pub poll_interval_minutes: Option<i64>,
}

/// `[retention]` 段（ADR-0013，票 #78：日志与产物共享 per-build 保留期）。
/// 优先级链 CLI > env > 文件 > 默认（Spec 明确进链；merge 在
/// [`merge_retention_days`] 收口）。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionFile {
    /// per-build 保留期天数（默认 30 天；每日低频扫描清理过期构建的日志
    /// chunk 与产物文件 + 元数据，构建记录永久保留）。
    pub retention_days: Option<i64>,
}

/// `[metrics]` 段（ADR-0019，票 B5-T7：/metrics 端点鉴权开关）。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsFile {
    /// `/metrics` 是否要求鉴权（默认 true：Bearer PAT 任意登录角色；
    /// false = 公开——仅限可信内网，文档注明）。
    pub auth: Option<bool>,
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
# 主密钥文件路径（默认 <data-dir>/master.key；首启自动生成 32 字节随机文件、已有不重生成。
# 防「DB 文件/备份单独泄露」的最后一环：可改到独立卷做运维纵深——数据目录整体失守
# （含密钥文件）无解。相对路径按相对数据目录解析，修改后重启生效）
# master_key_path = "/secure/volume/master.key"

[scheduler]
# orphan 宽限分钟（Agent 离线后运行中任务 unknown 的宽限，超时未恢复判失败，ADR-0008；
# 默认 10 分钟）
orphan_grace_minutes = 10

[triggers]
# poll 触发器轮询节奏默认分钟（项目级默认 5 分钟，ADR-0016）：新建 poll 触发器
# 未显式给节奏时取此值，进触发器 spec；cron 触发器按各自表达式节奏，不取此值。
poll_interval_minutes = 5

[retention]
# 日志与产物共享的 per-build 保留期天数（默认 30 天，ADR-0013）：每日低频扫描
# 清理过期构建的日志 chunk 与产物文件 + 元数据（含空目录回收）；构建记录
# （状态/号/时长）永久保留。删产物不碰 backups/ 迁移备份目录。
retention_days = 30

[metrics]
# /metrics 端点鉴权开关（默认 true，ADR-0019）：true = 需认证（Bearer PAT 任意
# 登录角色，运维可为 Prometheus 专建 viewer 用户）；false = 公开。仅限可信
# 内网关闭——该端点暴露调度队列深度等运行态，公网裸奔等同泄露运营信息。
auth = true
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
    pub fn load(data_dir: PathBuf, cli: Overrides, env: Overrides) -> Result<Config, ConfigError> {
        // 数据目录布局（ADR-0010）：数据库文件落在根，产物/升级包/迁移备份各占子目录。
        for sub in [ARTIFACTS_DIR, UPGRADE_PACKAGES_DIR, BACKUPS_DIR] {
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
        let get = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
        Overrides {
            rest_addr: get("SISYPHUS_REST_ADDR"),
            grpc_addr: get("SISYPHUS_GRPC_ADDR"),
            log_level: get("SISYPHUS_LOG_LEVEL"),
            log_format: get("SISYPHUS_LOG_FORMAT"),
            registration_enabled: get("SISYPHUS_REGISTRATION_ENABLED"),
            master_key_path: get("SISYPHUS_MASTER_KEY_PATH"),
            retention_days: get("SISYPHUS_RETENTION_DAYS"),
            metrics_auth: get("SISYPHUS_METRICS_AUTH"),
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

    let rest_addr = pick(
        &cli.rest_addr,
        &env.rest_addr,
        file.server.rest_addr.as_deref(),
    )
    .unwrap_or_else(|| DEFAULT_REST_ADDR.to_string());
    let grpc_addr = pick(
        &cli.grpc_addr,
        &env.grpc_addr,
        file.server.grpc_addr.as_deref(),
    )
    .unwrap_or_else(|| DEFAULT_GRPC_ADDR.to_string());
    let log_level = pick(&cli.log_level, &env.log_level, file.log.level.as_deref())
        .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_string());
    let log_format = pick(&cli.log_format, &env.log_format, file.log.format.as_deref())
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

    // 主密钥文件路径：CLI > env > 文件 > 默认（数据目录内 master.key）。
    // CLI/env 层是文本，相对路径一律按相对数据目录解析（与文件层同为
    // 配置语义，覆盖层不引入第二套相对基准）。
    let master_key_path = match pick(&cli.master_key_path, &env.master_key_path, None) {
        Some(path) => resolve_master_key_path(&data_dir, &path),
        None => match &file.auth.master_key_path {
            Some(path) => resolve_master_key_path(&data_dir, &path.to_string_lossy()),
            None => data_dir.join(MASTER_KEY_FILE_NAME),
        },
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

    // orphan 宽限分钟：文件层可配，无 CLI/env 覆盖层（默认 10，ADR-0008）。
    // 负值/0 按 0 处理（宽限即时判败——调用侧 max(0) 收口）。
    let orphan_grace_minutes = file
        .scheduler
        .orphan_grace_minutes
        .unwrap_or(DEFAULT_ORPHAN_GRACE_MINUTES)
        .max(0);

    // poll 节奏默认分钟：文件层可配，无 CLI/env 覆盖层（默认 5，ADR-0016
    // 票 #14）。负值/0 按 1 处理（最少 1 分钟——调用侧 max(1) 收口，
    // 0 分钟轮询会忙轮，无意义）。
    let poll_interval_minutes = file
        .triggers
        .poll_interval_minutes
        .unwrap_or(DEFAULT_POLL_INTERVAL_MINUTES)
        .max(1);

    // 保留期天数：CLI > env > 文件 > 默认（票 #78，Spec 明确进优先级链，
    // ADR-0013 默认 30 天）。CLI/env 层是文本、文件层是原生 toml 整数，
    // 整数语义在 [`merge_retention_days`] 统一收口；非法取值启动失败。
    let retention_days = merge_retention_days(
        &cli.retention_days,
        &env.retention_days,
        file.retention.retention_days,
    )?;

    // /metrics 鉴权开关：CLI > env > 文件 > 默认（ADR-0019 默认开）。
    // CLI/env 层是文本（布尔语义在 merge 收口——复用 parse_bool，开关类
    // 配置拼错必须启动失败）；文件层是原生 toml 布尔。
    let metrics_auth = match pick(&cli.metrics_auth, &env.metrics_auth, None) {
        Some(value) => parse_bool(&value)?,
        None => file.metrics.auth.unwrap_or(DEFAULT_METRICS_AUTH),
    };

    Ok(Config {
        data_dir,
        rest_addr,
        grpc_addr,
        log_level,
        log_format,
        registration_enabled,
        master_key_path,
        orphan_grace_minutes,
        poll_interval_minutes,
        retention_days,
        metrics_auth,
    })
}

/// 覆盖层与文件层的主密钥路径统一解析：绝对路径原样用，相对路径按相对
/// 数据目录解析（[`AuthFile::master_key_path`] 同为相对基准）。
fn resolve_master_key_path(data_dir: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        data_dir.join(path)
    }
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
        assert_eq!(
            cfg.master_key_path,
            PathBuf::from("/tmp/data").join(MASTER_KEY_FILE_NAME),
            "主密钥默认落数据目录内"
        );
        assert_eq!(
            cfg.orphan_grace_minutes, DEFAULT_ORPHAN_GRACE_MINUTES,
            "orphan 宽限默认 10 分钟（ADR-0008）"
        );
        assert_eq!(
            cfg.poll_interval_minutes, DEFAULT_POLL_INTERVAL_MINUTES,
            "poll 节奏默认 5 分钟（ADR-0016）"
        );
        assert_eq!(
            cfg.retention_days, DEFAULT_RETENTION_DAYS,
            "日志/产物保留期默认 30 天（ADR-0013）"
        );
        assert_eq!(
            cfg.metrics_auth, DEFAULT_METRICS_AUTH,
            "/metrics 鉴权默认开（ADR-0019）"
        );
    }

    /// 票 #78：`[retention] retention_days` 文件层可配（与日志/产物共享
    /// per-build 保留期，默认 30 天）；CLI > env > 文件 > 默认全链可配
    /// （Spec 明确进优先级链）；非法取值启动失败；未知字段拒绝。
    #[test]
    fn retention_days_merges_priority_chain_and_validates() {
        let file = FileConfig {
            retention: RetentionFile {
                retention_days: Some(14),
            },
            ..FileConfig::default()
        };
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &Overrides::default(),
            &file,
        )
        .expect("文件层保留期");
        assert_eq!(cfg.retention_days, 14);

        // CLI > env > 文件。
        let cli = Overrides {
            retention_days: Some("60".into()),
            ..Overrides::default()
        };
        let env = Overrides {
            retention_days: Some("45".into()),
            ..Overrides::default()
        };
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &cli,
            &env,
            &file,
        )
        .expect("CLI 层");
        assert_eq!(cfg.retention_days, 60, "CLI 压过 env 与文件");
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &env,
            &file,
        )
        .expect("env 层");
        assert_eq!(cfg.retention_days, 45, "env 压过文件");

        // 文件层非法取值（<=0）→ 报错不静默（运维旋钮拼错必须暴露）。
        let file_zero = FileConfig {
            retention: RetentionFile {
                retention_days: Some(0),
            },
            ..FileConfig::default()
        };
        assert!(matches!(
            merge(
                PathBuf::from("/tmp/data"),
                &Overrides::default(),
                &Overrides::default(),
                &file_zero
            ),
            Err(ConfigError::InvalidLogValue(_))
        ));

        // CLI/env 层非法文本 → 报错。
        let bad = Overrides {
            retention_days: Some("abc".into()),
            ..Overrides::default()
        };
        assert!(matches!(
            merge(
                PathBuf::from("/tmp/data"),
                &bad,
                &Overrides::default(),
                &FileConfig::default()
            ),
            Err(ConfigError::InvalidLogValue(_))
        ));

        // 未知字段拒绝（deny_unknown_fields）。
        assert!(matches!(
            parse_toml("[retention]\nretention_days_x = 5\n"),
            Err(ConfigError::InvalidToml(_))
        ));
    }

    #[test]
    fn cli_beats_env_beats_file_beats_default() {
        let cli = Overrides {
            grpc_addr: Some("127.0.0.1:60001".into()),
            registration_enabled: Some("false".into()),
            master_key_path: Some("/vol/cli.key".into()),
            ..Overrides::default()
        };
        let env = Overrides {
            grpc_addr: Some("127.0.0.1:60002".into()),
            rest_addr: Some("127.0.0.1:60003".into()),
            registration_enabled: Some("true".into()),
            master_key_path: Some("/vol/env.key".into()),
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
                master_key_path: Some("/vol/file.key".into()),
            },
            scheduler: SchedulerFile::default(),
            triggers: TriggersFile::default(),
            retention: RetentionFile::default(),
            metrics: MetricsFile::default(),
        };

        let cfg =
            merge(PathBuf::from("/tmp/data"), &cli, &env, &file).expect("各层齐全时合并应成功");

        // CLI 压过环境变量与文件。
        assert_eq!(cfg.grpc_addr, "127.0.0.1:60001");
        // 环境变量压过文件。
        assert_eq!(cfg.rest_addr, "127.0.0.1:60003");
        // 文件压过内置默认。
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.log_format, LogFormat::Pretty);
        // CLI 的 false 压过 env 与文件的 true（关得掉，不只是开得开）。
        assert!(!cfg.registration_enabled);
        // 主密钥路径同样按 CLI > env > 文件 > 默认合并。
        assert_eq!(cfg.master_key_path, PathBuf::from("/vol/cli.key"));
    }

    #[test]
    fn master_key_path_merges_across_layers_and_resolves_relative() {
        // 文件层绝对路径：直接生效。
        let file_abs = FileConfig {
            auth: AuthFile {
                master_key_path: Some("/secure/volume/master.key".into()),
                ..AuthFile::default()
            },
            ..FileConfig::default()
        };
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &Overrides::default(),
            &file_abs,
        )
        .expect("文件层绝对路径");
        assert_eq!(
            cfg.master_key_path,
            PathBuf::from("/secure/volume/master.key")
        );

        // 文件层相对路径：按相对数据目录解析。
        let file_rel = FileConfig {
            auth: AuthFile {
                master_key_path: Some("keys/master.key".into()),
                ..AuthFile::default()
            },
            ..FileConfig::default()
        };
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &Overrides::default(),
            &file_rel,
        )
        .expect("文件层相对路径");
        assert_eq!(
            cfg.master_key_path,
            PathBuf::from("/tmp/data/keys/master.key")
        );

        // CLI/env 层压过文件层，相对路径同样按数据目录解析。
        let cli = Overrides {
            master_key_path: Some("my.key".into()),
            ..Overrides::default()
        };
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &cli,
            &Overrides::default(),
            &file_abs,
        )
        .expect("CLI 层");
        assert_eq!(cfg.master_key_path, PathBuf::from("/tmp/data/my.key"));

        // 无任何覆盖：数据目录内默认文件名。
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &Overrides::default(),
            &FileConfig::default(),
        )
        .expect("默认");
        assert_eq!(
            cfg.master_key_path,
            PathBuf::from("/tmp/data").join(MASTER_KEY_FILE_NAME)
        );
    }

    #[test]
    fn registration_switch_merges_across_layers_and_parses_bool_spellings() {
        let file_on = FileConfig {
            auth: AuthFile {
                registration_enabled: Some(true),
                ..AuthFile::default()
            },
            ..FileConfig::default()
        };
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &Overrides::default(),
            &file_on,
        )
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
            merge(
                PathBuf::from("/tmp/data"),
                &Overrides::default(),
                &env,
                &file_on
            ),
            Err(ConfigError::InvalidBool(_))
        ));
    }

    #[test]
    fn sample_toml_parses_to_built_in_defaults() {
        let file = parse_toml(sample_toml()).expect("样例必须是合法 toml");

        // 样例值与内置默认一致——「生成即用」不改变启动行为的前提。
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &Overrides::default(),
            &file,
        )
        .expect("样例合并应成功");
        assert_eq!(cfg.rest_addr, DEFAULT_REST_ADDR);
        assert_eq!(cfg.grpc_addr, DEFAULT_GRPC_ADDR);
        assert_eq!(cfg.log_level, DEFAULT_LOG_LEVEL);
        assert_eq!(cfg.log_format, LogFormat::Json);
        assert!(!cfg.registration_enabled, "样例值与内置默认一致");
        assert_eq!(
            cfg.orphan_grace_minutes, DEFAULT_ORPHAN_GRACE_MINUTES,
            "样例值与内置默认一致（orphan 宽限）"
        );
        assert_eq!(
            cfg.poll_interval_minutes, DEFAULT_POLL_INTERVAL_MINUTES,
            "样例值与内置默认一致（poll 节奏）"
        );
        assert_eq!(
            cfg.retention_days, DEFAULT_RETENTION_DAYS,
            "样例值与内置默认一致（保留期）"
        );
        assert_eq!(
            cfg.metrics_auth, DEFAULT_METRICS_AUTH,
            "样例值与内置默认一致（/metrics 鉴权）"
        );

        // 带注释：样例要能当作文档读。
        assert!(
            sample_toml()
                .lines()
                .any(|l| l.trim_start().starts_with('#'))
        );
    }

    /// 票 B2c-T4：`[scheduler] orphan_grace_minutes` 文件层可配；负值按 0
    /// 处理（宽限即时判败）；未知字段拒绝（防手误静默失效）。
    #[test]
    fn scheduler_orphan_grace_merges_from_file_and_clamps_negative() {
        let file = FileConfig {
            scheduler: SchedulerFile {
                orphan_grace_minutes: Some(30),
            },
            ..FileConfig::default()
        };
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &Overrides::default(),
            &file,
        )
        .expect("文件层 orphan 宽限");
        assert_eq!(cfg.orphan_grace_minutes, 30);

        // 负值/0 按 0（宽限即时判败——调用侧 max(0) 收口）。
        let file_zero = FileConfig {
            scheduler: SchedulerFile {
                orphan_grace_minutes: Some(-5),
            },
            ..FileConfig::default()
        };
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &Overrides::default(),
            &file_zero,
        )
        .expect("负值按 0");
        assert_eq!(cfg.orphan_grace_minutes, 0);

        // 未知字段拒绝（deny_unknown_fields）。
        assert!(matches!(
            parse_toml("[scheduler]\norphan_grace = 5\n"),
            Err(ConfigError::InvalidToml(_))
        ));
    }

    /// 票 B2c-T6：`[triggers] poll_interval_minutes` 文件层可配；负值/0 按 1
    /// 处理（最少 1 分钟——0 分钟会忙轮，无意义）；未知字段拒绝。
    #[test]
    fn triggers_poll_interval_merges_from_file_and_clamps_non_positive() {
        let file = FileConfig {
            triggers: TriggersFile {
                poll_interval_minutes: Some(15),
            },
            ..FileConfig::default()
        };
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &Overrides::default(),
            &file,
        )
        .expect("文件层 poll 节奏");
        assert_eq!(cfg.poll_interval_minutes, 15);

        // 负值/0 按 1（调用侧 max(1) 收口——0 分钟轮询会忙轮）。
        let file_zero = FileConfig {
            triggers: TriggersFile {
                poll_interval_minutes: Some(0),
            },
            ..FileConfig::default()
        };
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &Overrides::default(),
            &file_zero,
        )
        .expect("0 按 1");
        assert_eq!(cfg.poll_interval_minutes, 1);

        let file_neg = FileConfig {
            triggers: TriggersFile {
                poll_interval_minutes: Some(-3),
            },
            ..FileConfig::default()
        };
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &Overrides::default(),
            &file_neg,
        )
        .expect("负值按 1");
        assert_eq!(cfg.poll_interval_minutes, 1);

        // 未知字段拒绝（deny_unknown_fields）。
        assert!(matches!(
            parse_toml("[triggers]\npoll_cadence = 5\n"),
            Err(ConfigError::InvalidToml(_))
        ));
    }

    /// 票 B5-T7：`[metrics] auth` 文件层可配；CLI > env > 文件 > 默认全链
    /// （ADR-0019 默认开）；开关类拼错启动失败；未知字段拒绝。
    #[test]
    fn metrics_auth_merges_priority_chain_and_validates() {
        // 文件层关闭（/metrics 公开——仅限可信内网）。
        let file = FileConfig {
            metrics: MetricsFile { auth: Some(false) },
            ..FileConfig::default()
        };
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &Overrides::default(),
            &file,
        )
        .expect("文件层关闭");
        assert!(!cfg.metrics_auth, "文件层 auth=false 生效");

        // CLI > env > 文件：CLI true 压过文件 false。
        let cli = Overrides {
            metrics_auth: Some("true".into()),
            ..Overrides::default()
        };
        let env = Overrides {
            metrics_auth: Some("false".into()),
            ..Overrides::default()
        };
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &cli,
            &env,
            &file,
        )
        .expect("CLI 层");
        assert!(cfg.metrics_auth, "CLI true 压过 env 与文件");
        let cfg = merge(
            PathBuf::from("/tmp/data"),
            &Overrides::default(),
            &env,
            &file,
        )
        .expect("env 层");
        assert!(!cfg.metrics_auth, "env false 压过文件");

        // env 层常见布尔拼写全收（复用 parse_bool 语义）。
        for (text, expected) in [
            ("true", true),
            ("1", true),
            ("yes", true),
            ("off", false),
        ] {
            let env = Overrides {
                metrics_auth: Some(text.into()),
                ..Overrides::default()
            };
            let cfg = merge(
                PathBuf::from("/tmp/data"),
                &Overrides::default(),
                &env,
                &FileConfig::default(),
            )
            .unwrap_or_else(|e| panic!("{text} 应解析成功：{e}"));
            assert_eq!(cfg.metrics_auth, expected, "env 层 {text}");
        }

        // 拼错的值：启动失败（不静默）。
        let bad = Overrides {
            metrics_auth: Some("onx".into()),
            ..Overrides::default()
        };
        assert!(matches!(
            merge(
                PathBuf::from("/tmp/data"),
                &bad,
                &Overrides::default(),
                &FileConfig::default()
            ),
            Err(ConfigError::InvalidBool(_))
        ));

        // 未知字段拒绝（deny_unknown_fields）。
        assert!(matches!(
            parse_toml("[metrics]\nauth_off = true\n"),
            Err(ConfigError::InvalidToml(_))
        ));
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
            merge(
                PathBuf::from("/tmp/data"),
                &bad_addr,
                &Overrides::default(),
                &FileConfig::default()
            ),
            Err(ConfigError::InvalidAddr(_))
        ));

        let bad_level = Overrides {
            log_level: Some("loud".into()),
            ..Overrides::default()
        };
        assert!(matches!(
            merge(
                PathBuf::from("/tmp/data"),
                &bad_level,
                &Overrides::default(),
                &FileConfig::default()
            ),
            Err(ConfigError::InvalidLogValue(_))
        ));

        let bad_format = Overrides {
            log_format: Some("xml".into()),
            ..Overrides::default()
        };
        assert!(matches!(
            merge(
                PathBuf::from("/tmp/data"),
                &bad_format,
                &Overrides::default(),
                &FileConfig::default()
            ),
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
