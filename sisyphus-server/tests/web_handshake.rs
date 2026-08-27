//! 构建产物与 server 组合根握手（票 B4-T9 / #71 AC）：经与二进制相同的
//! Router 组合根做 oneshot 请求——不起 socket、不 spawn 进程（Spec B2a 纪律）。
//!
//! 静态资源面 `static_web.rs` 已单测过 SPA fallback / 覆盖目录 / `/api` 404 /
//! 路径穿越；本用例不重复那些，而是把「真实前端构建产物（sisyphus-web/dist，
//! rust-embed 内嵌）」与「server 组合根」串成一条握手链：
//!
//! 1. 静态伺服：根路径回内嵌入口页（`id="app"` Vue 挂载点）。
//! 2. 产物可达：入口页引用的 `/assets/index-*.js` 主包能被独立取回、带
//!    `text/javascript` MIME、非空——证明构建产物随 server 一起打包且可寻径
//!    （不止是入口页回得来）。
//! 3. SPA fallback：深链（`/projects/demo`，非文件）回入口页而非 404——
//!    history 模式路由刷新可活。
//! 4. 登录往返：`POST /auth/setup` 建首个 admin → `POST /auth/login` 换会话
//!    cookie → `GET /auth/me` 凭 cookie 读回当前用户。
//! 5. 列表往返：`GET /projects` 凭 cookie 回空清单；`POST /projects` 建项目后
//!    `GET /projects` 回含该项目的清单——REST 契约与认证面整组可达。
//!
//! 全链在一个进程内的 Router 组合根上完成：构建产物真正被 server 嵌入伺服、
//! 真实 store + 临时库承载登录与会话、真实端点回清单——是「前端构建产物与
//! server 组合根握手」的最窄全链验证。

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use tower::ServiceExt;

mod common;

use common::{TestApp, body_json, req_with_cookie, test_app};

/// 本用例验证「真实前端构建产物 × server 组合根」的握手链，必须有已构建的
/// dist（sisyphus-web/dist，不入 git）：本地未建前端时跳过并留痕，CI 的
/// build-and-test 经 frontend job 的 artifact 注入真实产物始终真跑（见
/// `common::dist_built`）。
fn skip_unless_dist_built() -> bool {
    if common::dist_built() {
        return false;
    }
    eprintln!(
        "跳过：sisyphus-web/dist 未构建（sisyphus-web/ 下 npm run build）；\
         CI 注入真实产物运行本用例"
    );
    true
}

/// 内嵌构建产物 index.html 的可断言标记（与 `static_web.rs` 同源）：Vite 产物
/// 资源 URL 带内容哈希（每次构建变化），`<div id="app">` 是构建模板固定结构。
const APP_MOUNT_MARKER: &str = "id=\"app\"";

/// 进程内 GET（无附加头、无 cookie）——静态资源与 SPA fallback 用。
///
/// 与 `static_web.rs` 同款局部 `get`：故意不复用 `common::get`——后者注入
/// `ConnectInfo` 直连地址（认证/限流面用例所需），静态资源请求无需扩展注入，
/// 紧贴 `static_web.rs` 的本地形态便于对照。`body_text` 同理。
async fn get(app: &TestApp, path: &str) -> Response {
    app.router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("构造请求"),
        )
        .await
        .expect("oneshot")
}

async fn body_text(resp: Response) -> String {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("读响应体")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("UTF-8")
}

fn assert_content_type_starts_with(resp: &Response, prefix: &str) {
    let actual = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        actual.starts_with(prefix),
        "Content-Type 应以 {prefix} 开头，实际 {actual:?}"
    );
}

/// 从入口页正文提取首个 `/assets/*.js` 主包 URL（Vite 产物 `<script type="module"
/// src="/assets/index-HASH.js">`）。找不到即 panic——构建产物形态变了需同步本用例。
fn extract_main_bundle_src(index_html: &str) -> String {
    let needle = "src=\"/assets/";
    let start = index_html.find(needle).expect("入口页应引用 /assets/ 主包");
    let after = &index_html[start + "src=\"".len()..];
    let end = after.find('"').expect("src 属性闭合");
    after[..end].to_string()
}

/// AC1+2+3：构建产物随 server 嵌入伺服——入口页、主包可达、深链 SPA fallback。
#[tokio::test]
async fn built_dist_serves_via_combo_root() {
    if skip_unless_dist_built() {
        return;
    }
    let app = test_app().await;

    // 1. 根路径回内嵌入口页（Vue 挂载点在）。
    let resp = get(&app, "/").await;
    assert_eq!(resp.status(), StatusCode::OK, "根路径应回入口页");
    assert_content_type_starts_with(&resp, "text/html");
    let index_html = body_text(resp).await;
    assert!(
        index_html.contains(APP_MOUNT_MARKER),
        "入口页应含 Vue 挂载点：{}",
        index_html.chars().take(120).collect::<String>()
    );

    // 2. 入口页引用的主包能被独立取回、带 JS MIME、非空。
    let bundle_src = extract_main_bundle_src(&index_html);
    assert!(
        bundle_src.starts_with("/assets/") && bundle_src.ends_with(".js"),
        "主包 URL 形态：{bundle_src}"
    );
    let resp = get(&app, &bundle_src).await;
    assert_eq!(resp.status(), StatusCode::OK, "主包应可达：{bundle_src}");
    assert_content_type_starts_with(&resp, "text/javascript");
    let bundle = body_text(resp).await;
    assert!(!bundle.is_empty(), "主包不应为空");

    // 3. SPA fallback：深链（非文件）回入口页而非 404——history 模式刷新可活。
    let resp = get(&app, "/projects/demo").await;
    assert_eq!(resp.status(), StatusCode::OK, "深链应 SPA fallback");
    assert_content_type_starts_with(&resp, "text/html");
    let deep = body_text(resp).await;
    assert!(
        deep.contains(APP_MOUNT_MARKER),
        "深链应回入口页（SPA fallback）：{}",
        deep.chars().take(120).collect::<String>()
    );
}

/// AC4+5：登录 + 列表往返——REST 契约与认证面在组合根上整组可达。
#[tokio::test]
async fn login_and_list_round_trip_via_combo_root() {
    let app = test_app().await;

    // setup 建首个全局 admin（公开端点）。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/auth/setup",
        Some(r#"{ "username": "admin", "password": "admin-password-1" }"#.into()),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "setup 建首个 admin");
    let me = body_json(resp).await;
    assert_eq!(me["username"], "admin");
    assert_eq!(me["is_admin"], true, "setup 出来的是全局管理员");

    // login 换会话 cookie。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/auth/login",
        Some(r#"{ "username": "admin", "password": "admin-password-1" }"#.into()),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "login 应成功");
    let cookie = common::cookie_of(&resp).expect("login 应下发会话 cookie");

    // 凭 cookie 读回当前用户（会话恢复锚点）。
    let resp = req_with_cookie(&app, "GET", "/api/v1/auth/me", None, Some(&cookie)).await;
    assert_eq!(resp.status(), StatusCode::OK, "me 凭 cookie 应可达");
    let me = body_json(resp).await;
    assert_eq!(me["username"], "admin", "me 回当前用户");
    assert_eq!(me["is_admin"], true);

    // 列表往返：空库先回空清单。
    let resp = req_with_cookie(&app, "GET", "/api/v1/projects", None, Some(&cookie)).await;
    assert_eq!(resp.status(), StatusCode::OK, "projects 列表应 200");
    let list = body_json(resp).await;
    assert!(
        list.as_array().is_some_and(|a| a.is_empty()),
        "空库应回空清单"
    );

    // 建项目后列表回含该项目的清单——整组往返成立。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/projects",
        Some(
            r#"{ "name": "demo", "scm_type": "git", "scm_url": "https://example.com/demo" }"#
                .into(),
        ),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "建项目");
    let resp = req_with_cookie(&app, "GET", "/api/v1/projects", None, Some(&cookie)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let names: Vec<&str> = list
        .as_array()
        .expect("清单应为数组")
        .iter()
        .map(|p| p["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["demo"], "建项目后列表含该项目");
}
