//! 进程内集成测试共享装配（Spec B2a 测试缝；B2a-T5 抽出共用）：临时数据
//! 目录 → bootstrap（池+PRAGMA+迁移）→ 与二进制相同的 Router 组合根。
//! 静态资源本地覆盖目录按生产形态落在数据目录 `web/` 子目录
//! （config::WEB_DIR），需要覆盖文件的用例往 `TestApp::web` 写即可。
//!
//! B2b-T1 起附认证辅助：`setup_and_login` 走 setup wizard + login 换会话
//! cookie；`test_app_at` 在同一数据目录重开装配（Server 重启缝）。
//! B2b-T2 起：oneshot 请求默认注入 ConnectInfo 直连地址（login 限流的
//! per-IP 键）；带 cookie 的请求默认附 `Sec-Fetch-Site: same-origin`
//! （模拟同源 SPA——cookie 认证的非安全方法请求过 CSRF 面需要同源凭证；
//! CSRF/限流面用例经 `custom_req` 全自定义）。
//!
//! 各测试二进制只消费本模块的子集（rest_api / auth_rest / static_web 各取
//! 所需），未消费项的 dead_code 告警在此统一豁免。

#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use sisyphus_server::api::{AppState, router};
use sisyphus_server::config::WEB_DIR;
use sisyphus_server::store;
use sqlx::SqlitePool;
use tower::ServiceExt;

/// oneshot 请求的默认直连地址（无真实连接，经扩展注入；限流用例经
/// `custom_req` 换地址驱动 per-IP 键；Bearer 面用例复用同一地址形态）。
pub const DEFAULT_PEER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 52000);

/// 进程内测试装配：TempDir 随结构体存活，测试结束才连同库文件一起清理。
pub struct TestApp {
    /// 与二进制相同的 Router 组合根（oneshot 驱动）。
    pub router: axum::Router,
    /// 组合根状态（含 engine——构建端点用例需直接 drive 推进：缺机密失败、
    /// from_failed 重跑前置失败态等，无 sched 循环时手动驱动）。
    pub state: AppState,
    /// 底层连接池：用例直查/直改库（如把 session 改过期、断言哈希形态）。
    pub pool: SqlitePool,
    /// 静态资源本地覆盖目录（数据目录 `web/` 子目录）：用例自行放置文件。
    /// 只被 static_web 测试面消费；其余二进制里未读，故局部允许。
    #[allow(dead_code)]
    pub web: PathBuf,
    _dir: Option<tempfile::TempDir>,
}

/// 装配测试应用（全新临时数据目录，注册开关关——config 默认形态，票
/// B2b-T4）：真实 store + 临时库，不起 socket、不 spawn 进程。
pub async fn test_app() -> TestApp {
    test_app_with(false).await
}

/// 同 [`test_app`]，但显式给定注册开关（register 用例开启面的装配）。
pub async fn test_app_with(registration_enabled: bool) -> TestApp {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let mut app = test_app_at_with(dir.path(), registration_enabled).await;
    app._dir = Some(dir);
    app
}

/// 在既有数据目录上装配（Server 重启缝：同一目录第二次 bootstrap + 新
/// Router；TempDir 归调用侧持有；注册开关关，与 config 默认一致）。
pub async fn test_app_at(data_dir: &Path) -> TestApp {
    test_app_at_with(data_dir, false).await
}

/// [`test_app_at`] 的开关参数形态（与 [`test_app_with`] 对应）。
pub async fn test_app_at_with(data_dir: &Path, registration_enabled: bool) -> TestApp {
    let pool = store::bootstrap(data_dir).await.expect("bootstrap");
    let web = data_dir.join(WEB_DIR);
    // 主密钥按生产首启语义生成/读回（与二进制 main 同装配，票 B2b-T6）。
    let master_key = sisyphus_server::secrets::ensure_master_key(
        &data_dir.join(sisyphus_server::config::MASTER_KEY_FILE_NAME),
    )
    .expect("测试主密钥");
    let state = AppState::new(pool.clone(), registration_enabled, master_key);
    TestApp {
        router: router(state.clone(), web.clone()),
        state,
        pool,
        web,
        _dir: None,
    }
}

/// 由既有 AppState 装配 TestApp（票 B2c-T5 tracer bullet：REST router 与
/// scheduler/gRPC 共享同一 state——REST 触发经事件总线唤醒调度循环下发，
/// fake Agent 经真实 tonic 通道收 JobSpec）。调用侧持有 TempDir。
pub fn test_app_from_state(state: AppState, data_dir: &Path) -> TestApp {
    let web = data_dir.join(WEB_DIR);
    let pool = state.pool.clone();
    TestApp {
        router: router(state.clone(), web.clone()),
        state,
        pool,
        web,
        _dir: None,
    }
}

/// setup wizard 建首个 admin + login 换会话，返回 Cookie 请求头值
/// （`sisyphus_session=...`，业务端点用例统一经它过认证）。
pub async fn setup_and_login(app: &TestApp) -> String {
    let resp = post(
        app,
        "/api/v1/auth/setup",
        r#"{ "username": "admin", "password": "admin-password-1" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "setup 建首个 admin");

    let resp = post(
        app,
        "/api/v1/auth/login",
        r#"{ "username": "admin", "password": "admin-password-1" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "login 应成功");
    cookie_of(&resp).expect("login 应下发会话 cookie")
}

/// 从响应取会话 cookie 的完整请求头值。
pub fn cookie_of(resp: &Response) -> Option<String> {
    resp.headers()
        .get(header::SET_COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .next()
        .map(|kv| kv.trim().to_string())
}

/// 进程内请求（默认直连地址、无附加头）。
pub async fn req(app: &TestApp, method: &str, path: &str, body: Option<String>) -> Response {
    custom_req(app, method, path, body, None, &[], DEFAULT_PEER).await
}

/// 带 cookie 的进程内请求（认证面用例）：默认附 `Sec-Fetch-Site:
/// same-origin`，模拟同源 SPA——cookie 认证的非安全方法请求自 B2b-T2
/// 起须过 CSRF 面，不带同源凭证会 403。
pub async fn req_with_cookie(
    app: &TestApp,
    method: &str,
    path: &str,
    body: Option<String>,
    cookie: Option<&str>,
) -> Response {
    let headers = match cookie {
        Some(_) => vec![("sec-fetch-site", "same-origin".to_string())],
        None => Vec::new(),
    };
    custom_req(app, method, path, body, cookie, &headers, DEFAULT_PEER).await
}

/// 全自定义进程内请求（CSRF / 限流用例）：任意直连地址（login 限流的
/// per-IP 键）+ 任意附加头（Origin / Sec-Fetch-Site / Host /
/// Authorization）。不隐含任何同源凭证——由调用侧显式给。
pub async fn custom_req(
    app: &TestApp,
    method: &str,
    path: &str,
    body: Option<String>,
    cookie: Option<&str>,
    headers: &[(&str, String)],
    peer: SocketAddr,
) -> Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .extension(axum::extract::ConnectInfo(peer));
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    for (name, value) in headers {
        builder = builder.header(*name, value.as_str());
    }
    let body = Body::from(body.unwrap_or_default());
    app.router
        .clone()
        .oneshot(builder.body(body).expect("构造请求"))
        .await
        .expect("oneshot")
}

pub async fn get(app: &TestApp, path: &str) -> Response {
    req(app, "GET", path, None).await
}

pub async fn post(app: &TestApp, path: &str, body: &str) -> Response {
    req(app, "POST", path, Some(body.into())).await
}

pub async fn put(app: &TestApp, path: &str, body: &str) -> Response {
    req(app, "PUT", path, Some(body.into())).await
}

/// 读出响应体为 UTF-8 文本。
pub async fn body_text(resp: Response) -> String {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("读响应体")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("UTF-8")
}

pub async fn body_json(resp: Response) -> serde_json::Value {
    serde_json::from_str(&body_text(resp).await).expect("JSON 体")
}

/// 直接驱动 engine 推进构建（构建 REST 测试缝：common harness 无 sched 循环，
/// 缺机密失败 / from_failed 重跑前置失败态等需手动驱动；与 sched 循环共享
/// 同一 engine，drive 幂等）。
pub async fn drive_build(app: &TestApp, build_id: i64) {
    app.state
        .engine
        .drive(build_id)
        .await
        .expect("engine drive");
}
