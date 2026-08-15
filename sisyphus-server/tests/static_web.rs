//! 静态资源进程内集成（票 B2a-T5 AC）：经与二进制相同的 Router 组合根做
//! oneshot 请求。覆盖四条验收：SPA fallback 回占位 index.html、本地覆盖
//! 目录同名文件压过内嵌、`/api` 前缀未命中不落 fallback、healthz/Swagger
//! 不受影响；外加路径穿越不外泄文件系统（含数据目录内的库文件）。

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use tower::ServiceExt;

mod common;

use common::test_app;

/// 内嵌占位 index.html 的可断言标记（sisyphus-web/dist/index.html）。
const EMBEDDED_MARKER: &str = "sisyphus-web placeholder";

async fn get(app: &common::TestApp, path: &str) -> Response {
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

fn assert_content_type(resp: &Response, prefix: &str) {
    assert!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with(prefix)),
        "Content-Type 应以 {prefix} 开头：{:?}",
        resp.headers().get(header::CONTENT_TYPE)
    );
}

/// AC1：非 `/api` 未命中路径与根路径都返回占位 index.html。
#[tokio::test]
async fn non_api_miss_returns_embedded_index_html() {
    let app = test_app().await;

    for path in ["/", "/some-frontend-path", "/deep/nested/route"] {
        let resp = get(&app, path).await;
        assert_eq!(resp.status(), StatusCode::OK, "路径 {path} 应回 index.html");
        assert_content_type(&resp, "text/html");
        let body = body_text(resp).await;
        assert!(
            body.contains(EMBEDDED_MARKER),
            "路径 {path} 应回内嵌占位页：{body}"
        );
    }
}

/// AC2：本地覆盖目录中的同名文件优先于内嵌资源（index.html 与嵌套资产都成立）。
#[tokio::test]
async fn local_override_dir_shadows_embedded_assets() {
    let app = test_app().await;
    // 生产不预建 web/（handler 容忍缺失），放覆盖文件的用例自建。
    std::fs::create_dir_all(&app.web).unwrap();

    // 同名 index.html：SPA 入口被覆盖（含 fallback 场景）。
    std::fs::write(app.web.join("index.html"), "<!doctype html><p>override-home</p>").unwrap();
    // 嵌套资产：只存在于覆盖目录。
    std::fs::create_dir_all(app.web.join("assets")).unwrap();
    std::fs::write(app.web.join("assets").join("app.js"), "// override-asset").unwrap();

    // 同名覆盖：根路径与未命中路径都拿到覆盖版本，不再是内嵌占位页。
    for path in ["/", "/some-frontend-path"] {
        let resp = get(&app, path).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("override-home"), "路径 {path} 应被覆盖：{body}");
        assert!(!body.contains(EMBEDDED_MARKER), "内嵌版本应被压过：{body}");
    }

    // 覆盖目录独有的嵌套资产：可寻径、带正确 MIME。
    let resp = get(&app, "/assets/app.js").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_content_type(&resp, "text/javascript");
    assert_eq!(body_text(resp).await, "// override-asset");

    // 未被覆盖的路径仍走 SPA fallback：回覆盖版 index.html。
    let resp = get(&app, "/assets/missing.css").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_content_type(&resp, "text/html");
    assert!(body_text(resp).await.contains("override-home"));
}

/// AC3：`/api` 前缀未命中仍回统一 JSON 404，不落入 SPA fallback。
#[tokio::test]
async fn api_prefix_miss_stays_json_404_without_fallback() {
    let app = test_app().await;

    // 覆盖目录放上文件也不能把 /api 未命中拉进静态解析。
    std::fs::create_dir_all(&app.web).unwrap();
    std::fs::write(app.web.join("v1"), "should-not-serve").unwrap();

    for path in ["/api/v1/does-not-exist", "/api/v2/other", "/apix"] {
        let resp = get(&app, path).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "路径 {path} 应 404");
        assert_content_type(&resp, "application/json");
        let body = body_text(resp).await;
        assert!(body.contains("NOT_FOUND"), "统一错误形态：{body}");
        assert!(
            !body.contains(EMBEDDED_MARKER) && !body.contains("should-not-serve"),
            "不得落入 SPA fallback 或静态解析：{body}"
        );
    }
}

/// AC3 续：healthz 与 Swagger 不受静态层影响。
#[tokio::test]
async fn healthz_and_swagger_unaffected_by_static_layer() {
    let app = test_app().await;

    // healthz：仍是不鉴权 JSON 探活，不进静态解析。
    let resp = get(&app, "/healthz").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_content_type(&resp, "application/json");
    assert_eq!(body_text(resp).await, r#"{"status":"ok"}"#);

    // Swagger UI 仅开发期挂载（debug 构建）；挂载时不应被静态层遮蔽。
    #[cfg(debug_assertions)]
    {
        let resp = get(&app, "/swagger-ui/").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_content_type(&resp, "text/html");
    }
}

/// 路径穿越不外泄文件系统：明文与百分号编码的 `..`、反斜杠、盘符形态
/// 都不得读出覆盖目录之外的文件（含数据目录里的 SQLite 库文件），
/// 一律落回 SPA 入口页。
#[tokio::test]
async fn path_traversal_cannot_escape_override_dir() {
    let app = test_app().await;

    // 覆盖目录同级的诱饵文件（web/ 之外、数据目录之内）。
    let data_dir = app.web.parent().expect("web 收在数据目录内").to_path_buf();
    std::fs::write(data_dir.join("secret.txt"), "outside-secret").unwrap();
    // 库文件必在（bootstrap 已建库）：穿越不得摸到它。
    assert!(data_dir.join("sisyphus.db").is_file());

    for path in [
        "/../secret.txt",
        "/%2e%2e/secret.txt",
        "/..%2fsecret.txt",
        "/%2e%2e%2fsecret.txt",
        "/..\\secret.txt",
        "/..%5csecret.txt",
        "/c:/secret.txt",
        "/%2e%2e%5csecret.txt",
        "/../sisyphus.db",
        "/%2e%2e/sisyphus.db",
    ] {
        let resp = get(&app, path).await;
        let body = body_text(resp).await;
        assert!(
            !body.contains("outside-secret") && !body.contains("SQLite format 3"),
            "穿越形态 {path} 不得读出目录外文件：{}",
            body.chars().take(80).collect::<String>()
        );
    }

    // 穿越形态统一落 SPA 入口（合法前端行为，非错误页）。
    let resp = get(&app, "/../secret.txt").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_content_type(&resp, "text/html");
    assert!(body_text(resp).await.contains(EMBEDDED_MARKER));
}
