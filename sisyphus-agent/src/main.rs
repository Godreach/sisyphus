//! sisyphus-agent：构建机守护进程（ADR-0002）。
//!
//! 全矩阵支持（Linux/macOS/Windows × x86_64/aarch64），向 Server 注册、
//! 领取任务、在宿主机直接执行步骤（默认）或按配置选容器后端。B1 阶段
//! 只交付骨架 + 最小握手闭环：CLI 可解析、连上 Server 完成版本握手。
//! 只依赖 `sisyphus-proto`，不依赖 `sisyphus-model`（ADR-0009）。

mod cache;
mod channel;
mod cli;
mod runner;
mod upgrader;
mod workspace;

use clap::Parser;
use cli::Cli;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let agent_name = hostname().unwrap_or_else(|| "unknown".to_string());
    println!(
        "sisyphus-agent skeleton: connecting to {} as {agent_name}",
        cli.server_url
    );

    match channel::connect_and_handshake(&cli.server_url, &agent_name).await {
        Ok((_client, server_version)) => {
            println!(
                "握手成功：Server v{}.{}.{}",
                server_version.major, server_version.minor, server_version.patch
            );
            // B1 握手即收官；真实心跳/任务循环随后续批次。
        }
        Err(e) => {
            eprintln!("握手失败：{e}");
            std::process::exit(1);
        }
    }
}

/// 取主机名（尽力而为）。
fn hostname() -> Option<String> {
    std::env::var("HOSTNAME").ok()
}
