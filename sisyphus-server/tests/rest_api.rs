//! REST 进程内集成（Spec B2a 测试缝）：经与二进制相同的 Router 组合根做
//! oneshot 请求——不起 socket、不 spawn 进程，背后挂真实 store + 临时库。
//! 只测外部行为：HTTP 状态码与 JSON 形态。

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use sisyphus_model::pipeline::Pipeline;
use sisyphus_server::api::{AppState, router};
use sisyphus_server::store;
use tower::ServiceExt;

/// 进程内测试装配：临时数据目录 → bootstrap（池+PRAGMA+迁移）→ Router。
/// TempDir 随结构体存活，测试结束才连同库文件一起清理。
struct TestApp {
    router: axum::Router,
    _dir: tempfile::TempDir,
}

async fn test_app() -> TestApp {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let pool = store::bootstrap(dir.path()).await.expect("bootstrap");
    TestApp {
        router: router(AppState::new(pool)),
        _dir: dir,
    }
}

/// 进程内请求（每个用例现装组合根，互不共享状态）。
async fn req(app: &TestApp, method: &str, path: &str, body: Option<String>) -> Response {
    let mut builder = Request::builder().method(method).uri(path);
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let body = Body::from(body.unwrap_or_default());
    app.router
        .clone()
        .oneshot(builder.body(body).expect("构造请求"))
        .await
        .expect("oneshot")
}

async fn get(app: &TestApp, path: &str) -> Response {
    req(app, "GET", path, None).await
}

async fn post(app: &TestApp, path: &str, body: &str) -> Response {
    req(app, "POST", path, Some(body.into())).await
}

async fn put(app: &TestApp, path: &str, body: &str) -> Response {
    req(app, "PUT", path, Some(body.into())).await
}

/// 读出响应体为 UTF-8 文本。
async fn body_text(resp: Response) -> String {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("读响应体")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("UTF-8")
}

async fn body_json(resp: Response) -> serde_json::Value {
    serde_json::from_str(&body_text(resp).await).expect("JSON 体")
}

fn assert_json_content_type(resp: &Response) {
    assert!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .expect("带 Content-Type")
            .to_str()
            .expect("ASCII")
            .starts_with("application/json")
    );
}

/// 最小合法 Pipeline 定义 JSON。
fn valid_definition() -> String {
    serde_json::json!({
        "name": "build",
        "stages": [{
            "name": "build",
            "jobs": [{
                "name": "compile",
                "steps": [{ "type": "shell", "config": { "command": "cargo build --release" } }]
            }]
        }]
    })
    .to_string()
}

#[tokio::test]
async fn healthz_returns_200_without_querying_store() {
    // healthz 不鉴权、不查库（ADR-0010/0019）：handler 无存储触碰，
    // 能答 200 即证进程存活语义（深度检查随可观测性批次接入）。
    let app = test_app().await;
    let resp = get(&app, "/healthz").await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_json_content_type(&resp);
    assert_eq!(body_json(resp).await, serde_json::json!({ "status": "ok" }));
}

#[tokio::test]
async fn api_unknown_path_returns_unified_json_404() {
    // /api 前缀未命中：统一 JSON 错误形态（错误码 + message），供客户端稳定解析。
    let app = test_app().await;
    let resp = get(&app, "/api/v1/does-not-exist").await;

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_json_content_type(&resp);

    let body = body_json(resp).await;
    assert_eq!(body["code"], "NOT_FOUND");
    assert!(
        body["message"].as_str().is_some_and(|m| !m.is_empty()),
        "message 非空：{body}"
    );
}

#[tokio::test]
async fn non_api_unknown_path_keeps_plain_404() {
    // 非 /api 未命中维持普通 404（SPA fallback 归 B2a-T5，届时改语义）。
    let app = test_app().await;
    let resp = get(&app, "/some-frontend-path").await;

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        !resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json")),
        "普通 404 不落 JSON 错误形态"
    );
    assert_eq!(body_text(resp).await, "");
}

/// Swagger UI 与 OpenAPI JSON 仅开发期（debug 构建）挂载（ADR-0005）。
#[cfg(debug_assertions)]
#[tokio::test]
async fn swagger_ui_browsable_and_openapi_json_fetchable() {
    use sisyphus_server::api::ApiDoc;
    use utoipa::OpenApi;

    let app = test_app().await;

    // UI：/swagger-ui 重定向到带斜杠的入口，入口本身 200 且是 HTML。
    let entry = get(&app, "/swagger-ui").await;
    assert!(
        entry.status().is_redirection() || entry.status() == StatusCode::OK,
        "swagger-ui 入口可达：{}",
        entry.status()
    );

    let page = get(&app, "/swagger-ui/").await;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(
        page.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/html")),
        "swagger-ui 返回 HTML"
    );

    // OpenAPI JSON：可获取，且与组合根用的同一份 ApiDoc 完全一致。
    let resp = get(&app, "/api-docs/openapi.json").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let served = body_json(resp).await;
    let expect: serde_json::Value =
        serde_json::from_str(&ApiDoc::openapi().to_json().expect("生成 OpenAPI JSON"))
            .expect("OpenAPI JSON");
    assert_eq!(served, expect, "HTTP 面与 ApiDoc 逐字一致");
}

/// tracer bullet 全链路（票 B2a-T4 AC）：建项目 → PUT 非法定义 422 +
/// 结构化校验错误清单 → PUT 合法定义 revision=1 → 再存 revision=2 →
/// GET 读回与提交等价。
#[tokio::test]
async fn definition_save_read_back_tracer_bullet() {
    let app = test_app().await;

    // 1. 建项目。
    let resp = post(
        &app,
        "/api/v1/projects",
        r#"{ "name": "demo", "scm_type": "git", "scm_url": "https://example.com/repo", "default_branch": "main" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "建项目应 201");
    let project = body_json(resp).await;
    assert_eq!(project["name"], "demo");
    assert_eq!(project["scm_type"], "git");

    // 2. PUT 非法定义：多处违规整组透传（不短路）。
    let invalid = serde_json::json!({
        "name": "build",
        "parameters": [{
            "name": "target", "type": "string", "required": true, "default": null
        }],
        "stages": [{
            "name": "build",
            "when": "${SISY_WORKSPACE} == \"/x\"",
            "jobs": [{
                "name": "compile",
                "steps": [
                    { "type": "shell", "config": { "command": "   " } },
                    { "type": "shell", "config": { "command": "true", "when": "(a == \"b\"" } }
                ]
            }]
        }]
    })
    .to_string();
    let resp = put(&app, "/api/v1/projects/demo/pipelines/build", &invalid).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "非法定义应 422"
    );
    assert_json_content_type(&resp);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "VALIDATION_FAILED");
    let errors = body["detail"]["errors"]
        .as_array()
        .expect("detail.errors 应为数组");
    assert!(errors.len() >= 4, "四处违规应整组透传：{errors:?}");
    assert!(errors.iter().any(|e| {
        e["message"]
            .as_str()
            .is_some_and(|m| m.contains("必填参数必须带默认值"))
    }));
    assert!(errors.iter().any(|e| {
        e["path"]
            .as_str()
            .is_some_and(|p| p.starts_with("stages[0].jobs[0].steps["))
    }));
    // 每条错误都带 path + message（前端可定位渲染）。
    for e in errors {
        assert!(e["path"].as_str().is_some_and(|p| !p.is_empty()));
        assert!(e["message"].as_str().is_some_and(|m| !m.is_empty()));
    }

    // 3. PUT 合法定义：revision=1。
    let resp = put(
        &app,
        "/api/v1/projects/demo/pipelines/build",
        &valid_definition(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "合法定义应 200");
    let saved = body_json(resp).await;
    assert_eq!(saved["revision"], 1, "首存 revision=1");
    assert_eq!(saved["operator"], "anonymous", "auth 落地前占位操作人");
    assert!(saved["updated_at"].as_i64().is_some_and(|t| t > 0));

    // 4. 再存：revision=2。
    let resp = put(
        &app,
        "/api/v1/projects/demo/pipelines/build",
        &valid_definition(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["revision"], 2, "续存 revision=2");

    // 5. GET：读回与提交等价（serde 反序列化成 model 类型比对）。
    let resp = get(&app, "/api/v1/projects/demo/pipelines/build").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_json_content_type(&resp);
    let body = body_json(resp).await;
    assert_eq!(body["revision"], 2);
    assert_eq!(body["operator"], "anonymous");
    assert!(body["updated_at"].as_i64().is_some_and(|t| t > 0));
    let submitted: Pipeline =
        serde_json::from_str(&valid_definition()).expect("提交定义可反序列化");
    let read_back: Pipeline =
        serde_json::from_value(body["definition"].clone()).expect("读回定义应可反序列化");
    assert_eq!(read_back, submitted, "读回定义与提交等价");
    assert_eq!(read_back.stages.len(), 1);
    assert_eq!(read_back.stages[0].name, "build");
}

/// projects list / create / get 往返与错误面（票 B2a-T4 AC）。
#[tokio::test]
async fn projects_round_trip() {
    let app = test_app().await;

    let resp = post(
        &app,
        "/api/v1/projects",
        r#"{ "name": "alpha", "scm_type": "git", "scm_url": "https://example.com/a" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let alpha = body_json(resp).await;
    assert_eq!(alpha["name"], "alpha");
    assert_eq!(alpha["scm_type"], "git");
    assert!(alpha["default_branch"].is_null(), "git 默认分支可空");
    assert!(alpha["id"].as_i64().is_some_and(|id| id > 0));

    let resp = post(
        &app,
        "/api/v1/projects",
        r#"{ "name": "beta", "scm_type": "svn", "scm_url": "https://svn.example.com/trunk" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // list：全量、按名排序。
    let resp = get(&app, "/api/v1/projects").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let names: Vec<&str> = list
        .as_array()
        .expect("清单应为数组")
        .iter()
        .map(|p| p["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["alpha", "beta"]);

    // get：字段等价读回；不存在 404 统一形态。
    let resp = get(&app, "/api/v1/projects/alpha").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await, alpha, "get 与 create 读回等价");

    let resp = get(&app, "/api/v1/projects/nope").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["code"], "NOT_FOUND");

    // 重名 409。
    let resp = post(
        &app,
        "/api/v1/projects",
        r#"{ "name": "alpha", "scm_type": "git", "scm_url": "https://example.com/other" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(body_json(resp).await["code"], "CONFLICT");

    // 输入校验 422：空名 + svn 带默认分支，错误清单整组。
    let resp = post(
        &app,
        "/api/v1/projects",
        r#"{ "name": "  ", "scm_type": "svn", "scm_url": "https://s.example/t", "default_branch": "trunk" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "VALIDATION_FAILED");
    let errors = body["detail"]["errors"].as_array().expect("错误清单");
    assert!(errors.len() >= 2, "空名与 svn 分支应都在：{errors:?}");

    // 请求体非法 JSON：同样落统一 422 形态。
    let resp = post(&app, "/api/v1/projects", "{ not json").await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(resp).await["code"], "VALIDATION_FAILED");
}

/// pipeline 定义端点的寻径错误面与非法 JSON。
#[tokio::test]
async fn definition_endpoint_error_surface() {
    let app = test_app().await;

    // 项目不存在：PUT/GET 都 404。
    let resp = put(
        &app,
        "/api/v1/projects/nope/pipelines/build",
        &valid_definition(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["code"], "NOT_FOUND");

    let resp = get(&app, "/api/v1/projects/nope/pipelines/build").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 项目在、pipeline 不在：GET 404。
    post(
        &app,
        "/api/v1/projects",
        r#"{ "name": "demo", "scm_type": "git", "scm_url": "https://example.com/repo" }"#,
    )
    .await;
    let resp = get(&app, "/api/v1/projects/demo/pipelines/ghost").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 定义 JSON 不合 model 形态（字段类型错）：422 校验形态。
    let resp = put(
        &app,
        "/api/v1/projects/demo/pipelines/build",
        r#"{ "name": 123, "stages": [] }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(resp).await["code"], "VALIDATION_FAILED");

    // 完全非 JSON：422 统一形态（不走 axum 默认纯文本拒绝）。
    let resp = put(&app, "/api/v1/projects/demo/pipelines/build", "not json").await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_json_content_type(&resp);
    assert_eq!(body_json(resp).await["code"], "VALIDATION_FAILED");
}
