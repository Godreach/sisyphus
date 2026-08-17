//! sisyphus-agent bin 薄壳（ADR-0009）：模块实现在库面（`sisyphus_agent`），
//! 这里只做启动路径：解析 CLI → 合并配置（ADR-0010）→ 初始化 tracing
//! （ADR-0019：stderr pretty + 可选 --log-file JSON 追加）→ 装配组合根 →
//! 常驻运行（断线指数退避重连，永久重试不自杀）。

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use sisyphus_agent::config::{Config, Overrides};

/// sisyphus Agent
#[derive(Parser, Debug)]
#[command(name = "sisyphus-agent", version, about)]
struct Args {
    /// Server 地址（gRPC 通道；也可经 SISYPHUS_SERVER_URL 设置）
    #[arg(long)]
    server_url: Option<String>,
    /// Server REST API 基址（注册码兑 token / 产物 / 升级下载的 HTTP 面；
    /// 也可经 SISYPHUS_API_URL 设置）
    #[arg(long)]
    api_url: Option<String>,
    /// 一次性注册码（票 #57：存在则先向 Server 兑长期 token 落盘，再常驻；
    /// 首次接入引导用）
    #[arg(long)]
    reg_key: Option<String>,
    /// 数据目录（默认 ~/.sisyphus-agent；也可经 SISYPHUS_DATA_DIR 设置）
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// 日志级别（默认 info；也可经 SISYPHUS_LOG_LEVEL 设置；RUST_LOG 整体胜出）
    #[arg(long)]
    log_level: Option<String>,
    /// 可选追加写 JSON 的运行日志文件（ADR-0019：不自管轮转；也可经
    /// SISYPHUS_LOG_FILE 设置）
    #[arg(long)]
    log_file: Option<PathBuf>,
}

impl From<&Args> for Overrides {
    fn from(args: &Args) -> Self {
        Overrides {
            server_url: args.server_url.clone(),
            api_url: args.api_url.clone(),
            data_dir: args.data_dir.clone(),
            log_level: args.log_level.clone(),
            log_file: args.log_file.clone(),
        }
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // CLI flag > SISYPHUS_ 环境变量 > 内置默认（ADR-0010；无 config.toml 层）。
    // 环境变量层单独注入（clap 的 env 帮助面在 --help 里展示，合并语义在
    // 配置缝上收口，与 server 同纪律）。
    let config = match Config::load(&Overrides::from(&args), &Overrides::from_env()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("配置加载失败：{e}");
            std::process::exit(2);
        }
    };

    if let Err(e) = init_tracing(&config) {
        eprintln!("日志初始化失败：{e}");
        std::process::exit(2);
    }

    tracing::info!(
        server_url = %config.server_url,
        api_url = %config.api_url.as_deref().unwrap_or("(未配置)"),
        data_dir = %config.data_dir.display(),
        "sisyphus-agent 启动"
    );

    // 注册引导（票 #57，Spec B3 §7）：`--reg-key` 存在 → 先兑 token 落盘
    // 再常驻；失败明确报错退出（注册是引导步骤，无 token 连不上通道）。
    if let Some(reg_key) = &args.reg_key
        && let Err(e) = bootstrap_register(&config, reg_key).await
    {
        tracing::error!(error = %e, "注册失败");
        eprintln!("注册失败：{e}");
        std::process::exit(1);
    }

    let agent = sisyphus_agent::Agent::new(config);
    // 关闭 watch：发送端由本进程持有（drop = 进程退出）；常驻运行，
    // 断线指数退避重连、永久重试不自杀（ADR-0007）。
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    agent.run(shutdown_rx).await;
}

/// 注册引导：`--reg-key` 兑 token 落盘。Agent 名取主机名（与通道握手
/// agent_name 同源——Server 建条目时的构建机名即主机名）。`api_url` 缺则
/// 明确报错（注册走 REST HTTP 面，与 gRPC 通道地址不同端口）。
async fn bootstrap_register(config: &Config, reg_key: &str) -> Result<(), Box<dyn std::error::Error>> {
    let api_url = config
        .api_url
        .as_deref()
        .ok_or("注册需要 Server REST 基址：请传 --api-url 或设置 SISYPHUS_API_URL（与 gRPC 通道 --server-url 是不同端口）")?;
    let name = sisyphus_agent::channel::hostname();
    let client = reqwest::Client::new();
    let token = sisyphus_agent::register::register(&client, api_url, &name, reg_key).await?;
    sisyphus_agent::register::persist_token(&config.data_dir, &token)?;
    tracing::info!(agent = %name, "注册成功：token 已落盘，后续启动直连不再需要注册码");
    Ok(())
}

/// tracing 基础初始化（ADR-0019）：RUST_LOG（若设置）整体胜出，否则用
/// 配置级别；stderr pretty 常开，`--log-file` 存在则叠加追加写 JSON。
fn init_tracing(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.log_level.clone()));
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    match &config.log_file {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            let json_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_writer(Arc::new(file));
            tracing_subscriber::registry()
                .with(stderr_layer.pretty().with_filter(filter.clone()))
                .with(json_layer.with_filter(filter))
                .try_init()?;
        }
        None => {
            tracing_subscriber::registry()
                .with(stderr_layer.pretty().with_filter(filter))
                .try_init()?;
        }
    }
    Ok(())
}
