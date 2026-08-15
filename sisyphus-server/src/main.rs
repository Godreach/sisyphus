//! sisyphus-server：单进程承载全部 Server 职责（ADR-0009）。
//!
//! B1 交付骨架二进制 + 最小握手闭环（ADR-0017）；启动路径（B2a）：
//! 解析 CLI → 合并配置（ADR-0010）→ 初始化 tracing（ADR-0019）→
//! 确保数据目录布局 → 开池+PRAGMA → 迁移前备份+前向迁移（ADR-0004/0010）→
//! serve。REST Router 与存储消费随后续批次接入。

mod api;
mod auth;
mod config;
mod engine;
mod events;
mod grpc;
mod notify;
mod sched;
mod scm;
mod store;
mod trigger;

use std::path::PathBuf;

use clap::Parser;

use crate::config::{Config, LogFormat, Overrides};

/// sisyphus Server
#[derive(Parser, Debug)]
#[command(name = "sisyphus-server", version, about)]
struct Args {
    /// 单一数据落点（默认 ./data，内含数据库、artifacts/、backups/）
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// REST API 监听地址（CLI 覆盖层，默认 0.0.0.0:8080；Router 归 B2a-T3）
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
    let _pool = match store::bootstrap(&config.data_dir).await {
        Ok(pool) => pool,
        Err(e) => {
            tracing::error!("存储初始化失败：{e}");
            std::process::exit(2);
        }
    };

    let service = grpc::service();
    let addr: std::net::SocketAddr = config.grpc_addr.parse().expect("配置层已校验监听地址");

    tracing::info!(
        rest_addr = %config.rest_addr,
        data_dir = %config.data_dir.display(),
        "sisyphus-server 启动：agent channel on {addr}"
    );

    tonic::transport::Server::builder()
        .add_service(service)
        .serve(addr)
        .await
        .expect("serve");
}

/// tracing 基础初始化（ADR-0019）：RUST_LOG 整体胜出，否则用配置级别
/// （附带 `sqlx=warn`，防逐条 SQL 刷屏）；Server 默认 stdout JSON，可切 pretty。
fn init_tracing(config: &Config) {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{},sqlx=warn", config.log_level)));
    match config.log_format {
        LogFormat::Json => {
            tracing_subscriber::fmt().json().with_env_filter(filter).init();
        }
        LogFormat::Pretty => {
            tracing_subscriber::fmt().pretty().with_env_filter(filter).init();
        }
    }
}
