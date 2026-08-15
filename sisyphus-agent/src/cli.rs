//! Agent CLI（clap）：`--server-url` 与可选注册码（ADR-0007）。

use clap::Parser;

/// sisyphus Agent
#[derive(Parser, Debug)]
#[command(name = "sisyphus-agent", version, about)]
pub struct Cli {
    /// Server 地址（gRPC 通道）
    #[arg(long)]
    pub server_url: String,

    /// 一次性注册码（ADR-0007：Agent 首次接入凭注册码换长期 token）
    #[arg(long)]
    pub reg_key: Option<String>,
}
