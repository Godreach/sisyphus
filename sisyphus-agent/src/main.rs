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
        data_dir = %config.data_dir.display(),
        "sisyphus-agent 启动"
    );

    let agent = sisyphus_agent::Agent::new(config);
    // 关闭 watch：发送端由本进程持有（drop = 进程退出）；常驻运行，
    // 断线指数退避重连、永久重试不自杀（ADR-0007）。
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    agent.run(shutdown_rx).await;
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
