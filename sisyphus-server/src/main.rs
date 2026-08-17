//! sisyphus-server bin 薄壳（ADR-0009）：模块实现在库面（`sisyphus_server`），
//! 这里只做启动路径：解析 CLI → 合并配置（ADR-0010）→ 初始化 tracing
//! （ADR-0019）→ 存储底座开池+迁移（B2a-T2）→ 绑定双端口 →
//! REST（axum）与 gRPC（tonic）并行 serve。

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use sisyphus_server::config::{Config, LogFormat, Overrides};
use sisyphus_server::{api, grpc, sched, store};

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
    /// 用户自注册开关（CLI 覆盖层，默认 false：register 403，账号由全局
    /// 管理员建立）
    #[arg(long)]
    registration_enabled: Option<bool>,
    /// 主密钥文件路径（CLI 覆盖层，默认 <data-dir>/master.key：首启自动生成，
    /// 可改到独立卷；相对路径按相对数据目录解析）
    #[arg(long)]
    master_key_path: Option<String>,
}

impl From<&Args> for Overrides {
    fn from(args: &Args) -> Self {
        Overrides {
            rest_addr: args.rest_addr.clone(),
            grpc_addr: args.grpc_addr.clone(),
            log_level: args.log_level.clone(),
            log_format: args.log_format.clone(),
            registration_enabled: args.registration_enabled.map(|b| b.to_string()),
            master_key_path: args.master_key_path.clone(),
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
    // 主密钥文件（票 B2b-T6，ADR-0015）：首启自动生成、已有文件不重生成；
    // 路径经 config 可改到独立卷。机密加密链的唯一锚点，启动失败即退出
    // （不带病运行——静默换钥 = 全部机密不可解）。
    let master_key = match sisyphus_server::secrets::ensure_master_key(&config.master_key_path) {
        Ok(key) => key,
        Err(e) => {
            tracing::error!(
                path = %config.master_key_path.display(),
                "主密钥初始化失败：{e}",
            );
            std::process::exit(2);
        }
    };
    // REST 组合根（B2a-T4）：池注入 repo，端点面见 api::router；注册开关
    // 随配置注入（票 B2b-T4）、主密钥随配置注入（票 B2b-T6）。
    let state = api::AppState::new(
        pool.clone(),
        config.registration_enabled,
        master_key,
        config.poll_interval_minutes,
    );
    // 静态资源本地覆盖目录（B2a-T5）：数据目录 web/ 子目录，不存在即纯内嵌。
    let web_override_dir = config.data_dir.join(sisyphus_server::config::WEB_DIR);

    // 调度装配（票 B2c-T4，ADR-0008）：会话注册表（grpc 面）→ 下发端口
    // （真实通道）→ 调度循环（事件驱动，共享 engine + 事件总线）。调度
    // 循环与 gRPC 服务同生命周期（server 进程即调度进程，单实例纪律）。
    let sessions = Arc::new(grpc::SessionRegistry::new());
    let dispatcher = Arc::new(grpc::GrpcDispatcher::new(sessions.clone()));
    let scheduler = sched::Scheduler::new(
        state.engine.clone(),
        pool.clone(),
        dispatcher,
        config.orphan_grace_minutes,
    );
    let scheduler_handle = scheduler.handle();
    let scheduler_state = state.clone();
    let sched_task = tokio::spawn(async move {
        // 启动从库重建（running/queued/unknown 任务、挂起取消补发）后进入
        // 事件驱动循环。
        if let Err(e) = scheduler.reconstruct().await {
            tracing::error!(error = %e, "调度状态重建失败");
        }
        scheduler.run(scheduler_state.bus.clone()).await;
    });
    // 构建终态通知钩子（票 #46 留位；notify 批次在此接 SMTP 发送）。
    let _notifier = sisyphus_server::notify::spawn_notifier(state.bus.clone());

    // 触发器装配（票 B2c-T6，ADR-0016）：触发引擎共享 engine + 事件总线，
    // 后台周期扫表（cron 按表达式命中触发、poll 按节奏轮询）。探测经 scm
    // trait 缝隔离——本批挂 [`scm::UnimplementedProbe`]（poll 记
    // last_probe_error、按节奏重试，真实 git/svn 探测随 scm 批次换入），
    // cron 不经探测照常工作。与 server 进程同生命周期（单实例纪律）。
    let trigger_engine = sisyphus_server::trigger::TriggerEngine::new(
        state.engine.clone(),
        pool.clone(),
        Arc::new(sisyphus_server::scm::UnimplementedProbe) as Arc<dyn sisyphus_server::scm::ScmProbe>,
    );
    let trigger_task = tokio::spawn(async move {
        trigger_engine.run(sisyphus_server::trigger::TRIGGER_LOOP_INTERVAL).await;
    });

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
    // 进程，不带病运行半个服务。REST 侧带连接信息（login 限流的 per-IP
    // 键取直连地址，票 B2b-T2）。状态克隆给 gRPC/扫描共用（同一组合根，
    // 共享同一池）。
    let grpc_state = state.clone();
    let sweep_state = state.clone();
    let grpc_sessions = sessions.clone();
    let grpc_scheduler = scheduler_handle.clone();
    let rest = async {
        axum::serve(
            rest_listener,
            api::router(state, web_override_dir)
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .expect("REST serve");
    };
    let grpc = async {
        tonic::transport::Server::builder()
            .add_service(grpc::service(grpc_state, grpc_sessions, grpc_scheduler))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                grpc_listener,
            ))
            .await
            .expect("gRPC serve");
    };
    // 心跳超时扫描（票 B2c-T3，ADR-0007/0008）：45s 无心跳判离线。与
    // gRPC 服务同生命周期（server 进程即扫描进程，单实例调度纪律）。
    let sweep = tokio::spawn(grpc::heartbeat_sweep(sweep_state));
    tokio::select! {
        _ = rest => {}
        _ = grpc => {}
    }
    sweep.abort();
    sched_task.abort();
    trigger_task.abort();
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
