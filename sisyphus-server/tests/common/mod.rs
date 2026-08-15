//! 进程内集成测试共享装配（Spec B2a 测试缝；B2a-T5 抽出共用）：临时数据
//! 目录 → bootstrap（池+PRAGMA+迁移）→ 与二进制相同的 Router 组合根。
//! 静态资源本地覆盖目录按生产形态落在数据目录 `web/` 子目录
//! （config::WEB_DIR），需要覆盖文件的用例往 `TestApp::web` 写即可。

use std::path::PathBuf;

use sisyphus_server::api::{AppState, router};
use sisyphus_server::config::WEB_DIR;
use sisyphus_server::store;

/// 进程内测试装配：TempDir 随结构体存活，测试结束才连同库文件一起清理。
pub struct TestApp {
    /// 与二进制相同的 Router 组合根（oneshot 驱动）。
    pub router: axum::Router,
    /// 静态资源本地覆盖目录（数据目录 `web/` 子目录）：用例自行放置文件。
    /// 只被 static_web 测试面消费；rest_api 二进制里未读，故局部允许。
    #[allow(dead_code)]
    pub web: PathBuf,
    _dir: tempfile::TempDir,
}

/// 装配测试应用：真实 store + 临时库，不起 socket、不 spawn 进程。
pub async fn test_app() -> TestApp {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let pool = store::bootstrap(dir.path()).await.expect("bootstrap");
    let web = dir.path().join(WEB_DIR);
    TestApp {
        router: router(AppState::new(pool), web.clone()),
        web,
        _dir: dir,
    }
}
