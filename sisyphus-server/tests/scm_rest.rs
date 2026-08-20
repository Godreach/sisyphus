//! SCM 面进程内集成（票 B5-T3，ADR-0016）：测试连接 / 分支枚举 / 既有项目
//! 测试连接 / 凭据设置端点行为与档位——真实 git ls-remote（本地裸仓库 fixture）
//! + 凭据不回显 + 加密落库 + 审计。只断言 HTTP 状态码与 JSON 形态，不起 socket。
//!
//! 角色准备沿用 secrets_rest.rs：全局 admin 经 setup wizard（建项目、配成员），
//! 普通用户直插 + login 换会话。

use axum::http::StatusCode;
use sqlx::SqlitePool;
use std::fs;
use std::path::PathBuf;
use std::process::Command as StdCommand;

mod common;

use common::{TestApp, body_json, cookie_of, req_with_cookie};

/// 测试用户共用密码（与共用 PHC 对应）。
const USER_PASSWORD: &str = "user-password-1";

/// 直插一个非 admin 用户并 login 换会话 cookie。
async fn user_cookie(app: &TestApp, username: &str) -> String {
    let phc = sisyphus_server::auth::hash_password_blocking(USER_PASSWORD);
    sqlx::query(
        "INSERT INTO users (username, password_hash, is_admin, disabled, created_at, updated_at)
         VALUES (?, ?, 0, 0, 1, 1)",
    )
    .bind(username)
    .bind(&phc)
    .execute(&app.pool)
    .await
    .expect("直插用户");
    let resp = common::post(
        app,
        "/api/v1/auth/login",
        &format!(r#"{{ "username": "{username}", "password": "{USER_PASSWORD}" }}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "login {username}");
    cookie_of(&resp).expect("会话 cookie")
}

/// 创建本地裸仓库（main + dev 两分支），返回 (TempDir, 裸仓库路径, main sha)。
fn bare_repo() -> (tempfile::TempDir, PathBuf, String) {
    let dir = tempfile::tempdir().expect("临时目录");
    let src = dir.path().join("src");
    fs::create_dir_all(&src).expect("建 src");
    let git = |args: &[&str]| {
        let out = StdCommand::new("git")
            .args(args)
            .current_dir(&src)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?}：{}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    git(&["init", "--quiet"]);
    git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(&["config", "user.email", "test@sisyphus.local"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "commit.gpgsign", "false"]);
    fs::write(src.join("hello.txt"), "v1\n").expect("写文件");
    git(&["add", "hello.txt"]);
    git(&["commit", "--quiet", "-m", "v1"]);
    let sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
        .expect("utf8")
        .trim()
        .to_string();
    git(&["branch", "dev"]);
    let bare = dir.path().join("bare");
    StdCommand::new("git")
        .args([
            "clone",
            "--bare",
            "--quiet",
            &src.to_string_lossy(),
            &bare.to_string_lossy(),
        ])
        .output()
        .expect("clone --bare");
    (dir, bare, sha)
}

/// 装配 + 全局 admin + 项目 demo（scm_url 指向裸仓库）+ viewer 成员 alice。
async fn fixture(bare_url: String) -> (TestApp, String, String) {
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/projects",
        Some(
            serde_json::json!({
                "name": "demo",
                "scm_type": "git",
                "scm_url": bare_url,
            })
            .to_string(),
        ),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "admin 建项目");
    // alice viewer（项目 admin 端点的 403 矩阵用）。
    let alice = user_cookie(&app, "alice").await;
    req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/members",
        Some(r#"[ { "username": "alice", "role": "viewer" } ]"#.into()),
        Some(&admin),
    )
    .await
    .status();
    (app, admin, alice)
}

/// 读取项目行 id（直查库）。
async fn project_id(pool: &SqlitePool, name: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM projects WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("查项目 id")
}

/// 创建期测试连接（git）返回当前 head。
#[tokio::test]
async fn scm_probe_git_returns_head() {
    let (repo_dir, bare, sha) = bare_repo();
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;

    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/projects/scm-probe",
        Some(
            serde_json::json!({
                "scm_type": "git",
                "scm_url": bare.to_string_lossy(),
            })
            .to_string(),
        ),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "探测应成功");
    let body = body_json(resp).await;
    assert_eq!(body["head"], sha, "返回 main HEAD sha");
    let _ = repo_dir;
}

/// 创建期分支枚举：列分支 + 默认分支。
#[tokio::test]
async fn scm_branches_lists_branches_and_default() {
    let (repo_dir, bare, _sha) = bare_repo();
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;

    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/projects/scm-branches",
        Some(serde_json::json!({ "scm_url": bare.to_string_lossy() }).to_string()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "分支枚举应成功");
    let body = body_json(resp).await;
    let names: Vec<String> = body["branches"]
        .as_array()
        .expect("branches 数组")
        .iter()
        .map(|b| b["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"main".to_string()), "含 main：{names:?}");
    assert!(names.contains(&"dev".to_string()), "含 dev：{names:?}");
    assert_eq!(body["default_branch"], "main", "默认分支 main");
    let _ = repo_dir;
}

/// 非 admin 调创建期探测 → 403（建项目为全局 admin 专属）。
#[tokio::test]
async fn scm_probe_forbidden_for_non_admin() {
    let (_repo_dir, bare, _sha) = bare_repo();
    let app = common::test_app().await;
    let _admin = common::setup_and_login(&app).await;
    let alice = user_cookie(&app, "alice").await; // 普通用户（无项目角色）。

    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/projects/scm-probe",
        Some(
            serde_json::json!({
                "scm_type": "git",
                "scm_url": bare.to_string_lossy(),
            })
            .to_string(),
        ),
        Some(&alice),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "非全局 admin 403");
    assert_eq!(body_json(resp).await["code"], "FORBIDDEN");
}

/// 探测失败（坏 URL）错误消息不回显凭据（ADR-0016）。
#[tokio::test]
async fn scm_probe_bad_url_error_does_not_echo_credentials() {
    let dir = tempfile::tempdir().expect("临时目录");
    let bad_url = dir.path().join("no-such-repo");
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;

    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/projects/scm-probe",
        Some(
            serde_json::json!({
                "scm_type": "git",
                "scm_url": bad_url.to_string_lossy(),
                "username": "alice",
                "password": "hunter2-secret-pw",
            })
            .to_string(),
        ),
        Some(&admin),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "坏 URL 422"
    );
    let body = body_json(resp).await;
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(!msg.contains("hunter2-secret-pw"), "不回显密码：{msg}");
    assert!(!msg.contains("alice"), "不回显用户名：{msg}");
}

/// 既有项目测试连接（存储凭据）：建项目带凭据 → test-connection 返回 head。
#[tokio::test]
async fn test_connection_existing_project_uses_stored_creds() {
    let (repo_dir, bare, sha) = bare_repo();
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;
    // 建项目带 SCM 凭据（本地裸仓库免认证，凭据递送不破坏探测）。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/projects",
        Some(
            serde_json::json!({
                "name": "demo",
                "scm_type": "git",
                "scm_url": bare.to_string_lossy(),
                "scm_username": "alice",
                "scm_password": "hunter2-secret-pw",
            })
            .to_string(),
        ),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "建项目带凭据");

    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/projects/demo/test-connection",
        None,
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "测试连接应成功");
    let body = body_json(resp).await;
    assert_eq!(body["head"], sha, "返回 head sha");
    let _ = repo_dir;
}

/// PUT scm-credential：加密落库 + 审计，密码明文不入库；viewer 403。
#[tokio::test]
async fn put_scm_credential_encrypts_and_audits_and_blocks_viewer() {
    let (_repo_dir, bare, _sha) = bare_repo();
    let (app, admin, alice) = fixture(bare.to_string_lossy().to_string()).await;
    let pid = project_id(&app.pool, "demo").await;

    // viewer 403。
    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/scm-credential",
        Some(r#"{ "username": "alice", "password": "hunter2-secret-pw" }"#.into()),
        Some(&alice),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "viewer 403");

    // admin PUT → 204。
    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/scm-credential",
        Some(r#"{ "username": "alice", "password": "hunter2-secret-pw" }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "PUT 204");

    // 库内：username 明文、密码为加密密文（非明文、版本字节 1）。
    let (username, ciphertext): (Option<String>, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT username, password_ciphertext FROM project_scm_credentials WHERE project_id = ?",
    )
    .bind(pid)
    .fetch_one(&app.pool)
    .await
    .expect("查凭据");
    assert_eq!(username.as_deref(), Some("alice"));
    let blob = ciphertext.expect("有密文");
    assert_eq!(blob[0], sisyphus_server::secrets::CIPHERTEXT_VERSION);
    assert!(
        !String::from_utf8_lossy(&blob).contains("hunter2-secret-pw"),
        "密文不含明文密码"
    );

    // 审计行：scm_credential_set，detail action=set。
    let (event, detail): (String, String) = sqlx::query_as(
        "SELECT event_type, detail FROM audit_log WHERE project_name = 'demo' AND event_type = 'scm_credential_set' ORDER BY ts DESC LIMIT 1",
    )
    .fetch_one(&app.pool)
    .await
    .expect("查审计");
    assert_eq!(event, "scm_credential_set");
    assert!(detail.contains("\"set\""), "detail action=set：{detail}");
}

/// PUT 空体 = 清凭据（删行）；test-connection 仍可（免认证仓库）。
#[tokio::test]
async fn put_scm_credential_empty_clears() {
    let (_repo_dir, bare, _sha) = bare_repo();
    let (app, admin, _alice) = fixture(bare.to_string_lossy().to_string()).await;
    let pid = project_id(&app.pool, "demo").await;

    // 先设。
    req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/scm-credential",
        Some(r#"{ "username": "alice", "password": "pw" }"#.into()),
        Some(&admin),
    )
    .await
    .status();
    // 再清（空体）。
    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/scm-credential",
        Some(r#"{}"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "清凭据 204");
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM project_scm_credentials WHERE project_id = ?")
            .bind(pid)
            .fetch_one(&app.pool)
            .await
            .expect("计数");
    assert_eq!(count, 0, "清后无行");
    // test-connection 仍可（本地裸仓库免认证）。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/projects/demo/test-connection",
        None,
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "清凭据后测试连接仍可");
}
