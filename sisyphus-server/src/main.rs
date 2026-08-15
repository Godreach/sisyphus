//! sisyphus-server bin 薄壳（ADR-0009）：模块实现在库面（`sisyphus_server`），
//! 这里只做启动路径：解析 CLI → 合并配置（ADR-0010）→ 初始化 tracing
//! （ADR-0019）→ 存储底座开池+迁移（B2a-T2）→ 绑定双端口 →
//! REST（axum）与 gRPC（tonic）并行 serve。

use std::path::PathBuf;

use clap::Parser;

use sisyphus_server::config::{Config, LogFormat, Overrides};
use sisyphus_server::{api, grpc, store};

/// sisyphus Server
#[derive(Parser, Debug)]
#[command(name = "sisyphus-server", version, about)]
struct Args {
    /// 单一数据落点（默认 ./data，内含数据库、artifacts/、backups/）
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// REST API 监听地址（CLI 覆盖层，默认 0.0.0.0:8080）
    #[arg(long)]
    rest_addr: Option<String>,
    /// Agent gRPC 通道监听地址（CLI 覆盖层，默认 127.0.0.1:50051）
    #[arg(long)]
    grpc_addr: Option<String>,
    /// 日志级别（CLI 覆盖层，默认 info）
    #[arg(long)]
    log_level: Option<String>,
    /// 日志格式 json/pretty（CLI 覆盖层，默认 json）
    #[arg(long)]
    log_format: Option<String>,
}

impl From<&Args> for Overrides {
    fn from(args: &Args) -> Self {
        Overrides {
            rest_addr: args.rest_addr.clone(),
            grpc_addr: args.grpc_addr.clone(),
            log_level: args.log_level.clone(),
            log_format: args.log_format.clone(),
        }
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let config = match Config::load(
        args.data_dir.clone(),
        Overrides::from(&args),
        Overrides::from_env(),
    ) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("配置加载失败：{e}");
            std::process::exit(2);
        }
    };
    init_tracing(&config);

    // 存储底座（B2a-T2）：开池+PRAGMA，有待应用迁移时先备份再前向迁移。
    let pool = match store::bootstrap(&config.data_dir).await {
        Ok(pool) => pool,
        Err(e) => {
            tracing::error!("存储初始化失败：{e}");
            std::process::exit(2);
        }
    };
    // REST 组合根（B2a-T4）：池注入 repo，端点面见 api::router。
    let state = api::AppState::new(pool.clone());
    // 静态资源本地覆盖目录（B2a-T5）：数据目录 web/ 子目录，不存在即纯内嵌。
    let web_override_dir = config.data_dir.join(sisyphus_server::config::WEB_DIR);

    // 双端口先绑定再 serve（ADR-0005 端口合并策略推迟，各自独立监听）：
    // 任一端口被占即启动失败，不带病运行半个服务。
    let rest_addr: std::net::SocketAddr = config.rest_addr.parse().expect("配置层已校验监听地址");
    let grpc_addr: std::net::SocketAddr = config.grpc_addr.parse().expect("配置层已校验监听地址");
    let rest_listener = tokio::net::TcpListener::bind(rest_addr)
        .await
        .unwrap_or_else(|e| panic!("绑定 REST 端口 {rest_addr} 失败：{e}"));
    let grpc_listener = tokio::net::TcpListener::bind(grpc_addr)
        .await
        .unwrap_or_else(|e| panic!("绑定 gRPC 端口 {grpc_addr} 失败：{e}"));

    tracing::info!(
        rest_addr = %rest_addr,
        grpc_addr = %grpc_addr,
        data_dir = %config.data_dir.display(),
        "sisyphus-server 启动：REST + agent channel 双服务"
    );

    // 并行 serve：expect 收在各自 async 块内——任一出错即 panic 带崩整个
    // 进程，不带病运行半个服务。
    let rest = async {
        axum::serve(rest_listener, api::router(state, web_override_dir))
            .await
            .expect("REST serve");
    };
    let grpc = async {
        tonic::transport::Server::builder()
            .add_service(grpc::service())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                grpc_listener,
            ))
            .await
            .expect("gRPC serve");
    };
    tokio::join!(rest, grpc);
}

/// tracing 基础初始化（ADR-0019）：RUST_LOG 整体胜出，否则用配置级别
/// （附带 `sqlx=warn`，防逐条 SQL 刷屏）；Server 默认 stdout JSON，可切 pretty。
fn init_tracing(config: &Config) {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{},sqlx=warn", config.log_level)));
    match config.log_format {
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .init();
        }
        LogFormat::Pretty => {
            tracing_subscriber::fmt()
                .pretty()
                .with_env_filter(filter)
                .init();
        }
    }
}
