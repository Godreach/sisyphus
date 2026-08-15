//! sisyphus-server：单进程承载全部 Server 职责（ADR-0009）。
//!
//! B1 阶段只交付骨架二进制 + 最小握手闭环：启动 gRPC Agent 通道服务，
//! Agent 连上后完成版本握手（ADR-0017）。真实业务模块随后续批次。

mod api;
mod auth;
mod engine;
mod events;
mod grpc;
mod notify;
mod sched;
mod scm;
mod store;
mod trigger;

use clap::Parser;

/// sisyphus Server
#[derive(Parser, Debug)]
#[command(name = "sisyphus-server", version, about)]
struct Args {
    /// 监听地址
    #[arg(long, default_value = "127.0.0.1:50051")]
    addr: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let service = grpc::service();
    let addr: std::net::SocketAddr = args
        .addr
        .parse()
        .expect("addr 需为 host:port 形式");

    println!("sisyphus-server skeleton: agent channel on {addr}");

    tonic::transport::Server::builder()
        .add_service(service)
        .serve(addr)
        .await
        .expect("serve");
}
