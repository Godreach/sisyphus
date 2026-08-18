//! Agent 启动配置：CLI flag > `SISYPHUS_` 前缀环境变量 > 内置默认（ADR-0010，
//! 与 Server 同纪律；Agent 无 config.toml 层——配置面即三层的旗语）。
//!
//! 数据目录布局（票 B3-T1，五处约定）：根放 `token` 与 `agent.json` 两个
//! 文件，`workspaces/`、`cache/`、`logbuf/` 三个子目录：
//! - `token`：per-Agent 长期凭据（0600，注册批次落盘，本批只读）；
//! - `agent.json`：本地状态（升级失败计数等，随 upgrader 批次）；
//! - `workspaces/`：工作区根（ADR-0011，`SISYPHUS_AGENT_WORKSPACE_ROOT` 可覆盖）；
//! - `cache/`：缓存根 + `registry.json`（ADR-0012）；
//! - `logbuf/`：断线日志缓冲（ADR-0007/0013）。
//!
//! 与 ADR-0019 的日志纪律同源：`RUST_LOG`（若设置）整体胜出 > `SISYPHUS_LOG_LEVEL`
//! > 默认 `info`；stderr pretty 常开，`--log-file` 可选追加 JSON。

use std::path::{Path, PathBuf};

/// 数据目录默认名（放家目录下，ADR-0010「token 落盘位置独立于二进制安装路径」）。
pub const DEFAULT_DATA_DIR_NAME: &str = ".sisyphus-agent";
/// 日志级别默认值（ADR-0019）。
pub const DEFAULT_LOG_LEVEL: &str = "info";

/// 数据目录内的 token 文件名（per-Agent 长期凭据，注册批次写入）。
pub const TOKEN_FILE_NAME: &str = "token";
/// 数据目录内的本地状态文件名（升级失败计数等，upgrader 批次写入）。
pub const AGENT_JSON_FILE_NAME: &str = "agent.json";
/// 数据目录内的工作区根子目录名（ADR-0011）。
pub const WORKSPACES_DIR: &str = "workspaces";
/// 数据目录内的缓存根子目录名（ADR-0012）。
pub const CACHE_DIR: &str = "cache";
/// 数据目录内的断线日志缓冲子目录名（ADR-0007/0013）。
pub const LOGBUF_DIR: &str = "logbuf";
/// 缓存容量上限默认值（ADR-0012：per-Agent 容量上限，单位 GiB；0 = 不限）。
pub const DEFAULT_CACHE_CAPACITY_GIB: u64 = 20;

/// 合并后的启动配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Server 地址（gRPC 通道；无内置默认，缺则启动失败）。
    pub server_url: String,
    /// Server REST API 基址（注册码兑 token / 产物 / 升级包下载的 HTTP 面，
    /// 票 #57 起消费；与 gRPC 通道地址不同端口，无内置默认）。
    pub api_url: Option<String>,
    /// 数据目录（默认 `~/.sisyphus-agent`）。
    pub data_dir: PathBuf,
    /// 工作区根（ADR-0011：`SISYPHUS_AGENT_WORKSPACE_ROOT` 覆盖，缺省
    /// `<data_dir>/workspaces/`）。布局 `<根>/<pipeline>/<job>/` 的根。
    pub workspace_root: PathBuf,
    /// 日志级别（trace/debug/info/warn/error）。
    pub log_level: String,
    /// 可选追加写 JSON 的运行日志文件（ADR-0019：不自管轮转）。
    pub log_file: Option<PathBuf>,
    /// 缓存容量上限（ADR-0012：per-Agent，单位 GiB；0 = 不限，默认 20）。
    /// Agent 本地配置——磁盘容量是机器的运维属性，不参与调度决策。
    pub cache_capacity_gib: u64,
}

impl Config {
    /// 从 CLI 覆盖层与环境变量层合并出启动配置（ADR-0010：CLI > env > 默认），
    /// 并确保数据目录布局（五处约定中的三个子目录；token/agent.json 为文件，
    /// 按需生成）。环境变量层由调用方注入（生产路径传 [`Overrides::from_env`]，
    /// 测试传空层保证封闭——不读真实进程环境）。
    pub fn load(cli: &Overrides, env: &Overrides) -> Result<Config, ConfigError> {
        // 数据目录布局：根 + 三个子目录（token/agent.json 是文件，按需生成）。
        let data_dir = match pick_path(&cli.data_dir, &env.data_dir) {
            Some(path) => path,
            None => default_data_dir()?,
        };
        for sub in [WORKSPACES_DIR, CACHE_DIR, LOGBUF_DIR] {
            std::fs::create_dir_all(data_dir.join(sub))?;
        }

        let server_url = pick_str(&cli.server_url, &env.server_url)
            .ok_or(ConfigError::MissingServerUrl)?
            .to_string();
        let api_url = pick_str(&cli.api_url, &env.api_url).map(ToOwned::to_owned);
        let log_level = pick_str(&cli.log_level, &env.log_level)
            .unwrap_or(DEFAULT_LOG_LEVEL)
            .to_string();
        if !["trace", "debug", "info", "warn", "error"].contains(&log_level.as_str()) {
            return Err(ConfigError::InvalidLogLevel(log_level));
        }

        // 工作区根（ADR-0011）：环境变量覆盖优先，缺省取数据目录下的
        // `workspaces/` 子目录。覆盖层在此缝上收口（与 server-url 同纪律），
        // 下游 workspace 模块只读 `Config::workspace_root`，不再触碰环境变量。
        let workspace_root = match pick_path(&cli.workspace_root, &env.workspace_root) {
            Some(path) => path,
            None => data_dir.join(WORKSPACES_DIR),
        };
        // 工作区根存在性确保（覆盖到外部路径时数据目录布局不会自动建它）；
        // workspace 模块 resolve 亦 mkdir -p，此处保证采样器有目录可遍历。
        std::fs::create_dir_all(&workspace_root)?;

        // 安全护栏（ADR-0011「清理永不触碰缓存目录」）：工作区根不得与缓存根
        // （`<data>/cache/`）重叠或互相包含——否则 `WorkspaceClean` 的删树会越界
        // 删到缓存（如把 `SISYPHUS_AGENT_WORKSPACE_ROOT` 指到 `<data>` 或
        // `<data>/cache`）。默认兄弟布局天然满足；覆盖到外部路径时在此强校验。
        let cache_root = data_dir.join(CACHE_DIR);
        if overlaps(&workspace_root, &cache_root) {
            return Err(ConfigError::WorkspaceRootOverlapsCache {
                workspace_root,
                cache_root,
            });
        }

        Ok(Config {
            server_url,
            api_url,
            data_dir,
            workspace_root,
            log_level,
            log_file: pick_path(&cli.log_file, &env.log_file),
            cache_capacity_gib: cli
                .cache_capacity_gib
                .or(env.cache_capacity_gib)
                .unwrap_or(DEFAULT_CACHE_CAPACITY_GIB),
        })
    }

    /// token 文件路径（per-Agent 长期凭据）。
    pub fn token_path(&self) -> PathBuf {
        self.data_dir.join(TOKEN_FILE_NAME)
    }

    /// 本地状态文件路径（升级失败计数等）。
    pub fn agent_json_path(&self) -> PathBuf {
        self.data_dir.join(AGENT_JSON_FILE_NAME)
    }

    /// 工作区根（ADR-0011：`<根>/<pipeline>/<job>/` 布局；环境变量
    /// `SISYPHUS_AGENT_WORKSPACE_ROOT` 可覆盖，缺省 `<data>/workspaces/`）。
    pub fn workspaces_dir(&self) -> PathBuf {
        self.workspace_root.clone()
    }

    /// 缓存根（ADR-0012：registry.json 记账落此）。
    pub fn cache_dir(&self) -> PathBuf {
        self.data_dir.join(CACHE_DIR)
    }

    /// 缓存容量上限（字节；ADR-0012：`cache_capacity_gib` GiB → 字节，0 = 不限）。
    pub fn cache_capacity_bytes(&self) -> u64 {
        self.cache_capacity_gib.saturating_mul(1024 * 1024 * 1024)
    }

    /// 断线日志缓冲目录（ADR-0007/0013：每 (job, attempt) 一个 jsonl 文件）。
    pub fn logbuf_dir(&self) -> PathBuf {
        self.data_dir.join(LOGBUF_DIR)
    }
}

/// 同一形态的覆盖层：CLI flag 与 `SISYPHUS_` 环境变量都归约为它。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Overrides {
    /// Server 地址覆盖。
    pub server_url: Option<String>,
    /// Server REST API 基址覆盖（注册/产物/升级下载的 HTTP 面）。
    pub api_url: Option<String>,
    /// 数据目录覆盖（相对路径按相对当前工作目录解析）。
    pub data_dir: Option<PathBuf>,
    /// 工作区根覆盖（ADR-0011：`SISYPHUS_AGENT_WORKSPACE_ROOT`，缺省
    /// `<data>/workspaces/`）。
    pub workspace_root: Option<PathBuf>,
    /// 日志级别覆盖。
    pub log_level: Option<String>,
    /// 日志文件覆盖。
    pub log_file: Option<PathBuf>,
    /// 缓存容量上限覆盖（ADR-0012：GiB，0 = 不限）。
    pub cache_capacity_gib: Option<u64>,
}

impl Overrides {
    /// 读 `SISYPHUS_` 前缀环境变量层（ADR-0010；薄适配，语义在 merge 缝上测试）。
    pub fn from_env() -> Overrides {
        let get = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
        Overrides {
            server_url: get("SISYPHUS_SERVER_URL"),
            api_url: get("SISYPHUS_API_URL"),
            data_dir: get("SISYPHUS_DATA_DIR").map(PathBuf::from),
            workspace_root: get("SISYPHUS_AGENT_WORKSPACE_ROOT").map(PathBuf::from),
            log_level: get("SISYPHUS_LOG_LEVEL"),
            log_file: get("SISYPHUS_LOG_FILE").map(PathBuf::from),
            cache_capacity_gib: get("SISYPHUS_CACHE_CAPACITY_GIB")
                .and_then(|v| v.parse::<u64>().ok()),
        }
    }
}

/// 配置加载错误。
#[derive(Debug)]
pub enum ConfigError {
    /// 缺 server-url（CLI 与环境变量都没有；ADR-0010：无参数启动打印缺参提示）。
    MissingServerUrl,
    /// 日志级别取值非法。
    InvalidLogLevel(String),
    /// 找不到家目录（默认数据目录的基准）。
    NoHomeDir,
    /// 工作区根与缓存根重叠或互相包含（ADR-0011：清理永不触碰缓存；覆盖配置
    /// 把工作区根指到缓存根或其祖先/后代会让全清越界删缓存）。
    WorkspaceRootOverlapsCache {
        /// 工作区根。
        workspace_root: PathBuf,
        /// 缓存根。
        cache_root: PathBuf,
    },
    /// 数据目录或配置文件 IO 失败。
    Io(std::io::Error),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::MissingServerUrl => write!(
                f,
                "缺 Server 地址：请传 --server-url 或设置 SISYPHUS_SERVER_URL"
            ),
            ConfigError::InvalidLogLevel(v) => {
                write!(f, "日志级别非法：{v}（期望 trace/debug/info/warn/error）")
            }
            ConfigError::NoHomeDir => {
                write!(f, "找不到家目录（默认数据目录 ~/.sisyphus-agent 的基准）")
            }
            ConfigError::WorkspaceRootOverlapsCache {
                workspace_root,
                cache_root,
            } => write!(
                f,
                "工作区根 {} 与缓存根 {} 重叠或互相包含（ADR-0011：清理永不触碰缓存；\
                 请把 SISYPHUS_AGENT_WORKSPACE_ROOT 指到缓存根之外的独立目录）",
                workspace_root.display(),
                cache_root.display()
            ),
            ConfigError::Io(e) => write!(f, "数据目录 IO 失败：{e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

/// 字符串覆盖层按「CLI > 环境变量」取首层。
fn pick_str<'a>(cli: &'a Option<String>, env: &'a Option<String>) -> Option<&'a str> {
    cli.as_deref().or(env.as_deref())
}

/// 路径覆盖层按「CLI > 环境变量」取首层。
fn pick_path(cli: &Option<PathBuf>, env: &Option<PathBuf>) -> Option<PathBuf> {
    cli.clone().or_else(|| env.clone())
}

/// 默认数据目录：`<家目录>/.sisyphus-agent`。Windows 取 `USERPROFILE`，
/// Unix 取 `HOME`（尽力而为，两者都无则报错——默认值无法落位）。
fn default_data_dir() -> Result<PathBuf, ConfigError> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or(ConfigError::NoHomeDir)?;
    Ok(PathBuf::from(home).join(DEFAULT_DATA_DIR_NAME))
}

/// 从数据目录读 token（尽力而为：不存在/空/不可读 = `None`，通道侧以
/// 缺凭据连接、被拒后退避重试——注册批次落盘前这是预期状态）。
pub fn read_token(data_dir: &Path) -> Option<String> {
    std::fs::read_to_string(data_dir.join(TOKEN_FILE_NAME))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 两条路径是否重叠：相等、或一条是另一条的祖先（互相包含）。canonicalize
/// 解析真实路径（消除 `.`/`..`/符号链接/盘符大小写差异）；任一不存在返回
/// `false`（缺省布局下两者都已 create_dir_all，存在性由调用方保证）。
fn overlaps(a: &Path, b: &Path) -> bool {
    let Some(a) = a.canonicalize().ok() else {
        return false;
    };
    let Some(b) = b.canonicalize().ok() else {
        return false;
    };
    a == b || a.starts_with(&b) || b.starts_with(&a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_server_url_is_rejected() {
        // server-url 无内置默认：CLI/env 都缺则启动失败（ADR-0010 缺参提示
        // 与示例命令）。数据目录默认值取之于 env（HOME/USERPROFILE），
        // 其展开在 load_with_all_layers_merges_and_creates_layout 覆盖。
        let err = Config::load(&Overrides::default(), &Overrides::default())
            .expect_err("缺 server-url 应失败");
        assert!(matches!(err, ConfigError::MissingServerUrl));
    }

    #[test]
    fn load_with_all_layers_merges_and_creates_layout() {
        let dir = tempfile::tempdir().expect("临时数据目录");
        let cli = Overrides {
            server_url: Some("http://127.0.0.1:50051".into()),
            api_url: Some("http://127.0.0.1:8080".into()),
            data_dir: Some(dir.path().to_path_buf()),
            log_level: Some("debug".into()),
            log_file: Some(dir.path().join("agent.log")),
            ..Overrides::default()
        };
        let cfg = Config::load(&cli, &Overrides::default()).expect("CLI 层完整");

        assert_eq!(cfg.server_url, "http://127.0.0.1:50051");
        assert_eq!(cfg.api_url.as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.log_file, Some(dir.path().join("agent.log")));

        // 五处约定：三个子目录随加载创建。
        for sub in [WORKSPACES_DIR, CACHE_DIR, LOGBUF_DIR] {
            assert!(dir.path().join(sub).is_dir(), "数据目录应含 {sub}/");
        }
        // token / agent.json 是文件位（路径约定，按需生成——本批不落盘）。
        assert_eq!(cfg.token_path(), dir.path().join(TOKEN_FILE_NAME));
        assert_eq!(cfg.agent_json_path(), dir.path().join(AGENT_JSON_FILE_NAME));
    }

    #[test]
    fn env_beats_default_and_cli_beats_env() {
        // env 层压过内置默认。
        let env = Overrides {
            server_url: Some("http://env:50051".into()),
            api_url: Some("http://env:8080".into()),
            log_level: Some("warn".into()),
            ..Overrides::default()
        };
        let cfg = Config::load(&Overrides::default(), &env).expect("env 层");
        assert_eq!(cfg.server_url, "http://env:50051");
        assert_eq!(cfg.api_url.as_deref(), Some("http://env:8080"));
        assert_eq!(cfg.log_level, "warn");

        // CLI 层压过 env 层。
        let cli = Overrides {
            server_url: Some("http://cli:50051".into()),
            api_url: Some("http://cli:8080".into()),
            log_level: Some("trace".into()),
            ..Overrides::default()
        };
        let cfg = Config::load(&cli, &env).expect("CLI 层");
        assert_eq!(cfg.server_url, "http://cli:50051");
        assert_eq!(cfg.api_url.as_deref(), Some("http://cli:8080"));
        assert_eq!(cfg.log_level, "trace");
    }

    #[test]
    fn invalid_log_level_is_rejected() {
        let cli = Overrides {
            server_url: Some("http://127.0.0.1:50051".into()),
            log_level: Some("loud".into()),
            ..Overrides::default()
        };
        assert!(matches!(
            Config::load(&cli, &Overrides::default()),
            Err(ConfigError::InvalidLogLevel(_))
        ));
    }

    #[test]
    fn read_token_is_tolerant_of_missing_and_blank() {
        let dir = tempfile::tempdir().expect("临时数据目录");
        assert_eq!(read_token(dir.path()), None, "无 token 文件 = None");

        std::fs::write(dir.path().join(TOKEN_FILE_NAME), "  sisa_abc  \n").expect("写 token");
        assert_eq!(
            read_token(dir.path()),
            Some("sisa_abc".into()),
            "trim 后取用"
        );

        std::fs::write(dir.path().join(TOKEN_FILE_NAME), "   ").expect("写空白");
        assert_eq!(read_token(dir.path()), None, "空白 token 视为缺凭据");
    }

    #[test]
    fn cache_capacity_defaults_and_respects_override() {
        let dir = tempfile::tempdir().expect("临时数据目录");
        let cli = Overrides {
            server_url: Some("http://127.0.0.1:50051".into()),
            data_dir: Some(dir.path().to_path_buf()),
            ..Overrides::default()
        };
        // 缺省：20 GiB（ADR-0012）。
        let cfg = Config::load(&cli, &Overrides::default()).expect("缺省");
        assert_eq!(cfg.cache_capacity_gib, DEFAULT_CACHE_CAPACITY_GIB);
        assert_eq!(
            cfg.cache_capacity_bytes(),
            DEFAULT_CACHE_CAPACITY_GIB * 1024 * 1024 * 1024
        );

        // env 层覆盖。
        let env = Overrides {
            cache_capacity_gib: Some(5),
            ..Overrides::default()
        };
        let cfg = Config::load(&cli, &env).expect("env 覆盖");
        assert_eq!(cfg.cache_capacity_gib, 5);

        // CLI 层压过 env 层；0 = 不限（字节 0）。
        let cli_override = Overrides {
            cache_capacity_gib: Some(0),
            ..cli.clone()
        };
        let cfg = Config::load(&cli_override, &env).expect("CLI 胜 env");
        assert_eq!(cfg.cache_capacity_gib, 0, "0 = 不限");
        assert_eq!(cfg.cache_capacity_bytes(), 0);
    }

    #[test]
    fn workspace_root_defaults_under_data_and_respects_override() {
        // 缺省：工作区根 = 数据目录下的 workspaces/（ADR-0011）。
        let dir = tempfile::tempdir().expect("临时数据目录");
        let cli = Overrides {
            server_url: Some("http://127.0.0.1:50051".into()),
            data_dir: Some(dir.path().to_path_buf()),
            ..Overrides::default()
        };
        let cfg = Config::load(&cli, &Overrides::default()).expect("缺省");
        assert_eq!(cfg.workspace_root, dir.path().join(WORKSPACES_DIR));
        assert!(cfg.workspace_root.is_dir(), "缺省工作区根随加载创建");
        assert_eq!(cfg.workspaces_dir(), cfg.workspace_root);

        // 覆盖：环境变量层把工作区根指到数据目录之外的独立目录。
        let external = tempfile::tempdir().expect("外部工作区根");
        let env = Overrides {
            workspace_root: Some(external.path().to_path_buf()),
            ..Overrides::default()
        };
        let cfg = Config::load(&cli, &env).expect("覆盖");
        assert_eq!(cfg.workspace_root, external.path(), "env 层工作区根胜出");
        assert!(cfg.workspace_root.is_dir(), "外部工作区根随加载创建");

        // CLI 层压过 env 层。
        let cli_override = Overrides {
            workspace_root: Some(dir.path().join("cli-ws").to_path_buf()),
            ..cli.clone()
        };
        let cfg = Config::load(&cli_override, &env).expect("CLI 胜 env");
        assert_eq!(cfg.workspace_root, dir.path().join("cli-ws"));
    }

    #[test]
    fn workspace_root_overlapping_cache_is_rejected() {
        let dir = tempfile::tempdir().expect("临时数据目录");
        let base = Overrides {
            server_url: Some("http://127.0.0.1:50051".into()),
            data_dir: Some(dir.path().to_path_buf()),
            ..Overrides::default()
        };
        // 把工作区根指到缓存根本身 → 重叠，拒收（清理会越界删缓存）。
        let cache_root = dir.path().join(CACHE_DIR);
        let env = Overrides {
            workspace_root: Some(cache_root.clone()),
            ..Overrides::default()
        };
        let err = Config::load(&base, &env).expect_err("工作区根=缓存根应拒收");
        assert!(matches!(
            err,
            ConfigError::WorkspaceRootOverlapsCache { .. }
        ));

        // 把工作区根指到数据目录（缓存的祖先）→ 互相包含，拒收
        // （全清会删 <data>/cache）。
        let env = Overrides {
            workspace_root: Some(dir.path().to_path_buf()),
            ..Overrides::default()
        };
        let err = Config::load(&base, &env).expect_err("工作区根包含缓存根应拒收");
        assert!(matches!(
            err,
            ConfigError::WorkspaceRootOverlapsCache { .. }
        ));

        // 工作区根嵌在缓存根之下 → 互相包含，拒收（缓存清理会删工作区）。
        let env = Overrides {
            workspace_root: Some(dir.path().join(CACHE_DIR).join("ws")),
            ..Overrides::default()
        };
        let err = Config::load(&base, &env).expect_err("工作区根在缓存根下应拒收");
        assert!(matches!(
            err,
            ConfigError::WorkspaceRootOverlapsCache { .. }
        ));

        // 独立外部目录 → 接受。
        let external = tempfile::tempdir().expect("外部工作区根");
        let env = Overrides {
            workspace_root: Some(external.path().to_path_buf()),
            ..Overrides::default()
        };
        let cfg = Config::load(&base, &env).expect("独立目录应接受");
        assert_eq!(cfg.workspace_root, external.path());
    }
}
