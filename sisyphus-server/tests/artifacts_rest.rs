//! 产物 REST 集成测试（票 #74 / B5-T2，ADR-0004/0006/0007）：
//! Agent token 鉴权的上传（流式落盘 + 元数据落库）与依赖拉取（含「尚不
//! 存在」清晰报错）→ viewer 档的构建产物列表 / 单产物下载（响应头大小 +
//! 校验和）→ 端到端字节一致（任务 A 上传 → 任务 B 依赖拉取 → 构建详情
//! 页下载到同一份字节）。
//!
//! 形态基准：logs_pipeline（真实 store + 组合根，进程内 Router oneshot
//! 驱动；不经调度循环——本面聚焦产物链路，任务行直接落库）。

mod common;

use common::{DEFAULT_PEER, custom_req};
use http_body_util::BodyExt;
use axum::body::Body as HttpBody;
use sisyphus_model::pipeline::{Job, Pipeline, Revision, Stage};
use sisyphus_model::validate::BuildSnapshot;
use sisyphus_server::auth::{TokenFamily, generate_register_code, generate_token, token_hash};
use sisyphus_server::store::agents::NewAgent;
use sisyphus_server::store::builds::{BuildRepo, BuildRow, StartBuild, TriggerSource};
use sisyphus_server::store::jobs::{JobRepo, NewJob};
use sisyphus_server::store::projects::{NewProject, ProjectRepo, ScmType};
use sisyphus_server::{api, store};
use sha2::{Digest, Sha256};

struct Harness {
    _dir: tempfile::TempDir,
    app: common::TestApp,
    cookie: String,
    agent_token: String,
    /// PAT（用户面 Bearer——非 agent token 的 401 断言用）。
    pat_token: String,
    build: BuildRow,
    /// 任务 A（上传方）行 id。
    job_a: i64,
    /// 任务 B（依赖拉取方）行 id。
    job_b: i64,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let pool = store::bootstrap(dir.path()).await.expect("bootstrap");
    let master_key = sisyphus_server::secrets::ensure_master_key(
        &dir.path()
            .join(sisyphus_server::config::MASTER_KEY_FILE_NAME),
    )
    .expect("测试主密钥");
    let state = api::AppState::new(
        pool.clone(),
        dir.path().to_path_buf(),
        false,
        master_key,
        sisyphus_server::config::DEFAULT_POLL_INTERVAL_MINUTES,
        sisyphus_server::config::DEFAULT_RETENTION_DAYS,
        sisyphus_server::config::DEFAULT_METRICS_AUTH,
    )
    .await
    .expect("装配 AppState");
    let app = common::test_app_from_state(state.clone(), dir.path());

    let project = ProjectRepo::new(pool.clone())
        .create(NewProject {
            name: "demo".into(),
            scm_type: ScmType::Git,
            scm_url: "https://example.com/repo".into(),
            default_branch: Some("main".into()),
        })
        .await
        .expect("建项目");
    let agent_token = generate_token(TokenFamily::Agent);
    let code = generate_register_code();
    let agent = state
        .agents
        .create(NewAgent {
            name: "linux-1".into(),
            token_hash: token_hash(&agent_token),
            system_labels: "[]".into(),
            custom_labels: "[]".into(),
            max_concurrency: 1,
            register_code_hash: token_hash(&code),
            register_code_expires_at: 1_700_000_000_000 + 24 * 60 * 60 * 1000,
        })
        .await
        .expect("建 Agent 条目");

    // 两任务构建：任务 build（上传方）+ 任务 package（依赖拉取方）。
    let job_def = |name: &str| Job {
        name: name.into(),
        exec_env: None,
        labels: vec![],
        when: None,
        env: vec![],
        allow_failure: false,
        retry_count: 0,
        timeout_minutes: 0,
        artifact_uploads: vec![],
        artifact_downloads: vec![],
        caches: vec![],
        secrets: vec![],
        steps: vec![],
    };
    let snapshot = BuildSnapshot::new(
        Pipeline {
            name: "release".into(),
            parameters: vec![],
            env: vec![],
            notification: None,
            stages: vec![Stage {
                name: "main".into(),
                when: None,
                jobs: vec![job_def("build"), job_def("package")],
            }],
            revision: None,
        },
        Revision {
            number: 1,
            operator: "tester".into(),
            at_ms: 0,
        },
    );
    let build = BuildRepo::new(pool.clone())
        .start(StartBuild {
            project_id: project.id,
            pipeline_name: "release".into(),
            trigger: TriggerSource::Manual,
            trigger_detail: "{}".into(),
            snapshot,
        })
        .await
        .expect("建构建");
    let job_a = JobRepo::new(pool.clone())
        .insert(NewJob {
            build_id: build.id,
            stage_index: 0,
            name: "build".into(),
            attempt: 1,
            spec_json: None,
            agent_id: Some(agent.id),
            labels: vec![],
            timeout_minutes: 0,
            retry_count: 0,
            allow_failure: false,
        })
        .await
        .expect("建任务 A");
    let job_b = JobRepo::new(pool.clone())
        .insert(NewJob {
            build_id: build.id,
            stage_index: 0,
            name: "package".into(),
            attempt: 1,
            spec_json: None,
            agent_id: Some(agent.id),
            labels: vec![],
            timeout_minutes: 0,
            retry_count: 0,
            allow_failure: false,
        })
        .await
        .expect("建任务 B");

    // setup admin + login（viewer 档端点——全局 admin 隐含）；PAT（用户面
    // Bearer 的非 agent token 断言用）。
    let cookie = common::setup_and_login(&app).await;
    let pat = generate_token(TokenFamily::Pat);
    state
        .pats
        .insert(1, "test", &token_hash(&pat), None, 0)
        .await
        .expect("建 PAT");

    Harness {
        _dir: dir,
        app,
        cookie,
        agent_token,
        pat_token: pat,
        build,
        job_a: job_a.id,
        job_b: job_b.id,
    }
}

/// Agent token 面请求：POST 上传（二进制 body——common 的字符串请求缝
/// 覆盖不了任意字节，此处直接组 Request）。
async fn agent_upload(
    h: &Harness,
    job_id: i64,
    name: &str,
    bytes: &[u8],
) -> axum::response::Response {
    use tower::ServiceExt;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/agent/artifacts/{job_id}/{name}"))
        .header("authorization", format!("Bearer {}", h.agent_token))
        .extension(axum::extract::ConnectInfo(DEFAULT_PEER))
        .body(HttpBody::from(bytes.to_vec()))
        .expect("构造请求");
    h.app
        .router
        .clone()
        .oneshot(req)
        .await
        .expect("oneshot")
}

/// 依赖拉取请求（query 形态的产物名走路径段）。
async fn agent_download(
    h: &Harness,
    job_id: i64,
    source_job: &str,
    name: &str,
    token: &str,
) -> axum::response::Response {
    custom_req(
        &h.app,
        "GET",
        &format!("/api/v1/agent/artifacts/{job_id}/downloads/{source_job}/{name}"),
        None,
        None,
        &[("authorization", format!("Bearer {token}"))],
        DEFAULT_PEER,
    )
    .await
}

/// viewer 面请求（cookie 会话）。
async fn viewer_get(h: &Harness, path: &str) -> axum::response::Response {
    common::req_with_cookie(&h.app, "GET", path, None, Some(&h.cookie)).await
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// 端到端（票 #74 AC）：任务 A 上传 → 任务 B 依赖拉取 → 构建详情页
/// 下载，三处字节一致、大小/校验和响应头正确。
#[tokio::test]
async fn artifact_roundtrip_upload_dep_download_bytes_identical() {
    let h = harness().await;
    let bytes = b"artifact-payload-\xDE\xAD\xBE\xEF".repeat(1000);

    // 任务 A 上传（agent token）。
    let resp = agent_upload(&h, h.job_a, "dist.tar", &bytes).await;
    assert_eq!(resp.status(), 201, "上传应 201");
    let body = common::body_json(resp).await;
    assert_eq!(body["name"], "dist.tar");
    assert_eq!(body["size"], 21000);
    assert_eq!(body["sha256"], sha256_hex(&bytes));

    // 磁盘布局：artifacts/<build_id>/<name>。
    let on_disk = h
        ._dir
        .path()
        .join("artifacts")
        .join(h.build.id.to_string())
        .join("dist.tar");
    assert_eq!(tokio::fs::read(&on_disk).await.expect("读盘"), bytes);

    // 任务 B 依赖拉取：同构建内任务 A 的产物。
    let resp = agent_download(&h, h.job_b, "build", "dist.tar", &h.agent_token).await;
    assert_eq!(resp.status(), 200, "依赖拉取应 200");
    assert_eq!(
        resp.headers().get("content-length").and_then(|v| v.to_str().ok()),
        Some("21000"),
        "响应头带大小"
    );
    assert_eq!(
        resp.headers().get("x-sisyphus-sha256").and_then(|v| v.to_str().ok()),
        Some(sha256_hex(&bytes).as_str()),
        "响应头带校验和"
    );
    let got = resp.into_body().collect().await.expect("collect").to_bytes();
    assert_eq!(&got[..], &bytes[..], "依赖拉取字节一致");

    // 构建详情页：产物列表 + 单产物下载。
    let list_path = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}/artifacts",
        h.build.number
    );
    let resp = viewer_get(&h, &list_path).await;
    assert_eq!(resp.status(), 200);
    let body = common::body_json(resp).await;
    assert_eq!(body["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["items"][0]["name"], "dist.tar");
    assert_eq!(body["items"][0]["sha256"], sha256_hex(&bytes));

    let resp = viewer_get(&h, &format!("{list_path}/dist.tar")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("x-sisyphus-sha256").and_then(|v| v.to_str().ok()),
        Some(sha256_hex(&bytes).as_str())
    );
    let got = resp.into_body().collect().await.expect("collect").to_bytes();
    assert_eq!(&got[..], &bytes[..], "页面下载字节一致");
}

/// 上传鉴权面：非 agent token（PAT / cookie / 无凭据）一律 401。
#[tokio::test]
async fn agent_upload_rejects_non_agent_tokens() {
    let h = harness().await;

    // PAT（用户面 Bearer）→ 401（两族不混用）。
    let resp = custom_req(
        &h.app,
        "POST",
        &format!("/api/v1/agent/artifacts/{}/x.bin", h.job_a),
        None,
        None,
        &[("authorization", format!("Bearer {}", h.pat_token))],
        DEFAULT_PEER,
    )
    .await;
    assert_eq!(resp.status(), 401, "PAT 上传产物应 401");

    // cookie 会话（用户面）→ 401。
    let resp = common::req_with_cookie(
        &h.app,
        "POST",
        &format!("/api/v1/agent/artifacts/{}/x.bin", h.job_a),
        None,
        Some(&h.cookie),
    )
    .await;
    assert_eq!(resp.status(), 401, "cookie 会话上传产物应 401");

    // 无凭据 → 401；假 token → 401。
    let resp = custom_req(
        &h.app,
        "POST",
        &format!("/api/v1/agent/artifacts/{}/x.bin", h.job_a),
        None,
        None,
        &[],
        DEFAULT_PEER,
    )
    .await;
    assert_eq!(resp.status(), 401);
    let resp = custom_req(
        &h.app,
        "POST",
        &format!("/api/v1/agent/artifacts/{}/x.bin", h.job_a),
        None,
        None,
        &[("authorization", "Bearer sisa_forged".into())],
        DEFAULT_PEER,
    )
    .await;
    assert_eq!(resp.status(), 401);
}

/// 下载依赖的清晰报错（票 #74 AC）：产物尚不存在 / 来源任务不存在 /
/// 拉取任务不存在，各自 404 且消息可定位。
#[tokio::test]
async fn agent_download_missing_artifact_reports_clear_error() {
    let h = harness().await;

    // 产物未上传。
    let resp = agent_download(&h, h.job_b, "build", "dist.tar", &h.agent_token).await;
    assert_eq!(resp.status(), 404);
    let body = common::body_json(resp).await;
    let message = body["message"].as_str().unwrap_or_default().to_string();
    assert!(
        message.contains("依赖产物尚不存在") && message.contains("dist.tar"),
        "报错应清晰可定位：{message}"
    );

    // 来源任务名不存在（声明错名）。
    let resp = agent_download(&h, h.job_b, "no-such-job", "dist.tar", &h.agent_token).await;
    assert_eq!(resp.status(), 404);
    let body = common::body_json(resp).await;
    assert!(body["message"].as_str().unwrap_or_default().contains("no-such-job"));

    // 拉取任务行不存在。
    let resp = agent_download(&h, 9999, "build", "dist.tar", &h.agent_token).await;
    assert_eq!(resp.status(), 404);
}

/// 上传输入校验：非法产物名 422（不静默放宽）、任务行不存在 404；
/// 同名再传覆盖为最新（重跑语义）。
#[tokio::test]
async fn agent_upload_validates_name_and_overwrites_on_reupload() {
    let h = harness().await;

    for bad in ["..", "a%2Fb", "a%5Cb"] {
        let resp = agent_upload(&h, h.job_a, bad, b"x").await;
        assert_eq!(resp.status(), 422, "{bad} 应 422");
    }
    let resp = agent_upload(&h, 9999, "ok.bin", b"x").await;
    assert_eq!(resp.status(), 404, "任务不存在应 404");

    // 同名再传：元数据与磁盘均为最新一份。
    agent_upload(&h, h.job_a, "app.tar", b"v1").await;
    let resp = agent_upload(&h, h.job_a, "app.tar", b"v2-longer").await;
    assert_eq!(resp.status(), 201);
    let body = common::body_json(resp).await;
    assert_eq!(body["sha256"], sha256_hex(b"v2-longer"));
    let list_path = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}/artifacts",
        h.build.number
    );
    let resp = viewer_get(&h, &list_path).await;
    let body = common::body_json(resp).await;
    let items = body["items"].as_array().expect("列表");
    assert_eq!(items.len(), 1, "(build, name) 唯一——覆盖非新增");
}

/// viewer 面鉴权与 404：未认证 401；构建/产物不存在 404。
#[tokio::test]
async fn viewer_endpoints_require_auth_and_404_on_missing() {
    let h = harness().await;
    let base = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}/artifacts",
        h.build.number
    );

    // 未认证 → 401。
    let resp = common::get(&h.app, &base).await;
    assert_eq!(resp.status(), 401);

    // 构建号不存在 → 404。
    let resp = viewer_get(
        &h,
        "/api/v1/projects/demo/pipelines/release/builds/999/artifacts",
    )
    .await;
    assert_eq!(resp.status(), 404);

    // 产物不存在 → 404。
    let resp = viewer_get(&h, &format!("{base}/absent.bin")).await;
    assert_eq!(resp.status(), 404);
}

/// 归属校验：非本 Agent 承接的任务（agent_id 不匹配）→ 404 同形（不泄
/// 存在性）——Agent 只能写/读自己承接任务的产物。
#[tokio::test]
async fn agent_endpoints_reject_jobs_of_other_agents() {
    let h = harness().await;

    // 另一个 Agent 条目（合法 token，但任务不归它）。
    let other_token = generate_token(TokenFamily::Agent);
    let other_code = generate_register_code();
    h.app
        .state
        .agents
        .create(NewAgent {
            name: "linux-2".into(),
            token_hash: token_hash(&other_token),
            system_labels: "[]".into(),
            custom_labels: "[]".into(),
            max_concurrency: 1,
            register_code_hash: token_hash(&other_code),
            register_code_expires_at: 1_700_000_000_000 + 24 * 60 * 60 * 1000,
        })
        .await
        .expect("建另一 Agent 条目");

    // 上传（other token 打 linux-1 承接的任务）→ 404。
    let resp = custom_req(
        &h.app,
        "POST",
        &format!("/api/v1/agent/artifacts/{}/x.bin", h.job_a),
        None,
        None,
        &[("authorization", format!("Bearer {other_token}"))],
        DEFAULT_PEER,
    )
    .await;
    assert_eq!(resp.status(), 404, "他人任务 404 同形");

    // 归属 Agent 正常（对照组）。
    let resp = agent_upload(&h, h.job_a, "own.bin", b"ok").await;
    assert_eq!(resp.status(), 201);
}
