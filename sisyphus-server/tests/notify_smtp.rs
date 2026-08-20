//! 构建终态 SMTP 通知集成测试（票 B5-T5 AC）：
//!
//! - **配置发送**：PUT 全局 SMTP 配置 + pipeline `notification`(recipients +
//!   on_success) → 终态构建经 notify 链路发邮件，断言邮件内容含 构建号 /
//!   pipeline / 项目 / 状态 / 触发者；失败必发、成功按 `on_success`。
//! - **跳过**：`on_success=false` 成功跳过；SMTP 未配置跳过。
//! - **真实 engine 路径**：缺机密 pipeline → `drive_build` → engine 真发
//!   `BuildStatus::Failed` → 通知（守 engine→notify 接线，非纯直接发事件）。
//! - **SMTP 配置 REST**：读脱敏（`password_set` 无密码值）、变更入审计
//!   （`smtp_config_changed`）、非全局 admin PUT 403。
//!
//! 测试缝：[`spawn_notifier`] 注入 [`RecordingSender`]（impl [`MailSender`]）经
//! mpsc 通道捕获 [`MailMessage`] 断言内容——不绑端口、不真连 SMTP（lettre
//! 发送器的编译期/交叉编译覆盖在 F 收口，运行期由本缝覆盖决策与渲染）。

mod common;

use std::future::Future;
use std::time::Duration;

use axum::http::StatusCode;

use common::{TestApp, body_json, cookie_of, req_with_cookie};
use sisyphus_server::events::Event;
use sisyphus_server::notify::{MailMessage, MailSender, MailSendError, SmtpConnection, spawn_notifier};
use sisyphus_server::store::builds::BuildStatus;
use tokio::sync::mpsc;

/// 普通用户共用密码（与 authorization.rs 同形）。
const USER_PASSWORD: &str = "user-password-1";

// ---------------------------------------------------------------------------
// 测试缝：假发送器
// ---------------------------------------------------------------------------

/// 捕获 [`MailMessage`] 到 mpsc 通道供断言；忽略 [`SmtpConnection`]（不真连 SMTP）。
struct RecordingSender {
    tx: mpsc::UnboundedSender<MailMessage>,
}

impl MailSender for RecordingSender {
    fn send(
        &self,
        _conn: SmtpConnection,
        msg: MailMessage,
    ) -> impl Future<Output = Result<(), MailSendError>> + Send {
        let _ = self.tx.send(msg);
        async { Ok::<(), MailSendError>(()) }
    }
}

/// 永远发送失败的假发送器（AC3「发送失败不判败不重试」）：每次 send 记一次调用
/// 并返回 Err——断言「一次终态一次发送尝试」（无重试风暴）+ 构建不判败。
/// `Clone` 走 `Arc` 共享计数：spawn 一份进 notifier 任务，测试侧持一份读 `count`。
#[derive(Clone)]
struct FailingSender {
    attempts: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl MailSender for FailingSender {
    fn send(
        &self,
        _conn: SmtpConnection,
        _msg: MailMessage,
    ) -> impl Future<Output = Result<(), MailSendError>> + Send {
        self.attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        async {
            Err(MailSendError::Send("fake failure（AC3 验证）".into()))
        }
    }
}

impl FailingSender {
    fn new() -> Self {
        Self {
            attempts: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }
    fn count(&self) -> u32 {
        self.attempts.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// 订阅 notifier + 收一封邮件（超时即 panic）。
async fn recv_mail(rx: &mut mpsc::UnboundedReceiver<MailMessage>) -> MailMessage {
    tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .expect("15s 内应收到通知邮件")
        .expect("通道未关")
}

// ---------------------------------------------------------------------------
// 装配辅助
// ---------------------------------------------------------------------------

/// 全局 admin cookie + 项目 demo + pipeline release（带 notification）。
/// 是否配 SMTP 由调用侧决定（`smtp` 为真则 PUT 全局配置）。
async fn fixture(smtp: bool, on_success: bool, recipients: &[&str]) -> (TestApp, String) {
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;
    create_project(&app, &admin).await;
    save_definition(&app, &admin, "release", definition_with_notification(on_success, recipients)).await;
    if smtp {
        put_smtp_config(&app, &admin).await;
    }
    (app, admin)
}

async fn create_project(app: &TestApp, cookie: &str) {
    let resp = req_with_cookie(
        app,
        "POST",
        "/api/v1/projects",
        Some(
            r#"{"name":"demo","scm_type":"git","scm_url":"https://example.com/demo"}"#.into(),
        ),
        Some(cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "建项目");
}

/// pipeline 定义带 `notification`（recipients + on_success）+ 一个 shell 任务。
fn definition_with_notification(on_success: bool, recipients: &[&str]) -> String {
    serde_json::json!({
        "name": "release",
        "notification": { "on_success": on_success, "recipients": recipients },
        "stages": [{
            "name": "build",
            "jobs": [{
                "name": "compile",
                "steps": [{ "type": "shell", "config": { "command": "true" } }]
            }]
        }]
    })
    .to_string()
}

/// 引用缺失机密的 pipeline（`drive_build` 即组装失败 → 构建 Failed）+ notification。
fn definition_with_missing_secret(on_success: bool, recipients: &[&str]) -> String {
    serde_json::json!({
        "name": "release",
        "notification": { "on_success": on_success, "recipients": recipients },
        "stages": [{
            "name": "build",
            "jobs": [{
                "name": "compile",
                "secrets": ["MISSING"],
                "steps": [{ "type": "shell", "config": { "command": "true" } }]
            }]
        }]
    })
    .to_string()
}

async fn save_definition(app: &TestApp, cookie: &str, pipeline: &str, body: String) {
    let resp = req_with_cookie(
        app,
        "PUT",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}"),
        Some(body),
        Some(cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "存定义应 200");
}

async fn put_smtp_config(app: &TestApp, cookie: &str) {
    let resp = req_with_cookie(
        app,
        "PUT",
        "/api/v1/config/smtp",
        Some(
            r#"{"host":"smtp.example.com","port":587,"username":"postmaster","tls":"starttls","from_address":"ci@example.com","password":"relay-pw"}"#
                .into(),
        ),
        Some(cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "PUT SMTP 配置应 200");
}

/// 触发 release 的构建，返回 (build_id, number)。
async fn trigger_build(app: &TestApp, cookie: &str) -> (i64, i64) {
    let resp = req_with_cookie(
        app,
        "POST",
        "/api/v1/projects/demo/pipelines/release/builds",
        Some("{}".into()),
        Some(cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "触发应 202");
    let b = body_json(resp).await;
    (
        b["build_id"].as_i64().expect("build_id"),
        b["number"].as_i64().expect("number"),
    )
}

/// spawn notifier（订阅先于事件）+ 直接广播终态事件（与 engine finish_build 同形）。
fn publish_terminal(app: &TestApp, build_id: i64, number: i64, status: BuildStatus) {
    app.state.bus.publish(Event::BuildStatus {
        build_id,
        project_name: "demo".into(),
        pipeline_name: "release".into(),
        number,
        status,
        attempt: 1,
    });
}

/// 起一个带 RecordingSender 的 notifier，返回其接收端（spawn_notifier 同步订阅，
/// 调用后再发事件即不漏）。
fn spawn_recording(app: &TestApp) -> mpsc::UnboundedReceiver<MailMessage> {
    let (tx, rx) = mpsc::unbounded_channel::<MailMessage>();
    let _notifier = spawn_notifier(
        app.state.bus.clone(),
        app.state.clone(),
        RecordingSender { tx },
    );
    rx
}

/// 起一个带自定义 sender 的 notifier（AC3 用 FailingSender）。
fn spawn_with_sender<S: MailSender + 'static>(app: &TestApp, sender: S) {
    let _notifier = spawn_notifier(app.state.bus.clone(), app.state.clone(), sender);
}

// ---------------------------------------------------------------------------
// AC：配置发送（失败必发 / 成功按 on_success）+ 内容
// ---------------------------------------------------------------------------

/// 失败必发：on_success=false 也发；邮件内容含 构建号/pipeline/项目/状态/触发者
/// + 收件人 + 发件人。
#[tokio::test]
async fn failure_always_sends_with_content() {
    let (app, admin) = fixture(true, false, &["dev@example.com", "ops@example.com"]).await;
    let (build_id, number) = trigger_build(&app, &admin).await;
    let mut rx = spawn_recording(&app);
    publish_terminal(&app, build_id, number, BuildStatus::Failed);
    let msg = recv_mail(&mut rx).await;
    assert!(msg.body.contains(&format!("#{number}")), "含构建号：{}", msg.body);
    assert!(msg.body.contains("release"), "含 pipeline：{}", msg.body);
    assert!(msg.body.contains("demo"), "含项目：{}", msg.body);
    assert!(msg.body.contains("失败"), "含状态：{}", msg.body);
    assert!(msg.body.contains("admin"), "含触发者 admin：{}", msg.body);
    assert_eq!(msg.from, "ci@example.com");
    assert_eq!(
        msg.to,
        vec!["dev@example.com".to_string(), "ops@example.com".to_string()]
    );
}

/// 成功 + on_success=true → 发送（状态「成功」）。
#[tokio::test]
async fn success_sends_when_on_success_true() {
    let (app, admin) = fixture(true, true, &["dev@example.com"]).await;
    let (build_id, number) = trigger_build(&app, &admin).await;
    let mut rx = spawn_recording(&app);
    publish_terminal(&app, build_id, number, BuildStatus::Succeeded);
    let msg = recv_mail(&mut rx).await;
    assert!(msg.body.contains("成功"), "成功状态：{}", msg.body);
    assert!(msg.body.contains(&format!("#{number}")), "含构建号");
    assert!(msg.body.contains("admin"), "含触发者");
}

/// 成功 + on_success=false → 跳过。稳健两事件法：先发 Succeeded（应跳过），
/// 再发 Failed（失败必发）；只应收到 Failed 邮件——若 Succeeded 误发会先收到
/// 「成功」邮件使断言失败（不依赖「N 秒无邮件」的时序假设）。
#[tokio::test]
async fn success_skipped_when_on_success_false() {
    let (app, admin) = fixture(true, false, &["dev@example.com"]).await;
    let (build_id, number) = trigger_build(&app, &admin).await;
    let mut rx = spawn_recording(&app);
    publish_terminal(&app, build_id, number, BuildStatus::Succeeded);
    publish_terminal(&app, build_id, number, BuildStatus::Failed);
    let msg = recv_mail(&mut rx).await;
    // 收到的应是 Failed 邮件（含「失败」、不含「成功」）——Succeeded 被跳过。
    assert!(msg.body.contains("失败"), "应收到失败邮件：{}", msg.body);
    assert!(
        !msg.body.contains("成功"),
        "不应收到成功邮件（on_success=false 跳过）：{}",
        msg.body
    );
}

/// SMTP 未配置 → 跳过（无邮件）。
#[tokio::test]
async fn skipped_when_smtp_not_configured() {
    let (app, admin) = fixture(false, false, &["dev@example.com"]).await;
    let (build_id, number) = trigger_build(&app, &admin).await;
    let mut rx = spawn_recording(&app);
    publish_terminal(&app, build_id, number, BuildStatus::Failed);
    // 失败必发但 SMTP 未配置 → 跳过：3s 内不应收到邮件。
    let none = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await;
    assert!(
        none.is_err(),
        "SMTP 未配置应跳过（3s 内不应有邮件）——notifier 存活由其余发送测试佐证"
    );
}

/// 真实 engine 终态路径：缺机密 pipeline → drive_build → engine 真发
/// `BuildStatus::Failed` → 通知发送（守 engine→notify 接线，非纯直接发事件）。
#[tokio::test]
async fn failure_sends_via_real_engine_drive() {
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;
    create_project(&app, &admin).await;
    save_definition(
        &app,
        &admin,
        "release",
        definition_with_missing_secret(true, &["dev@example.com"]),
    )
    .await;
    put_smtp_config(&app, &admin).await;
    let (build_id, _number) = trigger_build(&app, &admin).await;
    let mut rx = spawn_recording(&app);
    // drive_build 触发 engine 真发 BuildStatus::Failed（spawn_notifier 已订阅）。
    common::drive_build(&app, build_id).await;
    let msg = recv_mail(&mut rx).await;
    assert!(msg.body.contains("失败"), "失败状态：{}", msg.body);
    assert!(msg.body.contains("admin"), "触发者 admin：{}", msg.body);
    assert!(msg.body.contains("release"), "含 pipeline");
}

/// AC3「发送失败不判败不重试」：注入永远失败的发送器 → 真实 engine 终态
/// （缺机密 drive → Failed）触发通知，一次终态事件只触发一次 send 调用（无重试
/// 风暴），且构建行状态不被发送失败影响（不判败）。notifier warn 后继续 loop
/// 不 panic。
#[tokio::test]
async fn send_failure_does_not_retry_or_fail_build() {
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;
    create_project(&app, &admin).await;
    save_definition(
        &app,
        &admin,
        "release",
        definition_with_missing_secret(true, &["dev@example.com"]),
    )
    .await;
    put_smtp_config(&app, &admin).await;
    let (build_id, _number) = trigger_build(&app, &admin).await;
    let sender = FailingSender::new();
    spawn_with_sender(&app, sender.clone());
    // drive_build 触发 engine 真发 BuildStatus::Failed（一次终态事件）。
    common::drive_build(&app, build_id).await;

    // 一次终态事件 → 一次 send 尝试（失败）。轮询 attempts 达 1（超时即判失败）。
    let started = std::time::Instant::now();
    while sender.count() == 0 && started.elapsed() < Duration::from_secs(15) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(sender.count(), 1, "一次终态一次 send 尝试（不重试风暴）");
    // 再等一拍确认没有第二次 send（重试风暴会在此期间补发）。
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(sender.count(), 1, "发送失败后不应重试（仍 1 次）");

    // 不判败：构建行状态仍为 engine 判定的 Failed（终态），发送失败不改构建状态。
    let row = sisyphus_server::store::builds::BuildRepo::new(app.pool.clone())
        .get(build_id)
        .await
        .expect("查")
        .expect("应存在");
    assert_eq!(
        row.status,
        BuildStatus::Failed,
        "发送失败不判败——构建终态状态不变"
    );
}

// ---------------------------------------------------------------------------
// AC：SMTP 配置 REST（审计 / 脱敏 / 权限）
// ---------------------------------------------------------------------------

/// PUT 变更入审计；GET 读脱敏（password_set、无密码值、不含明文密码）。
#[tokio::test]
async fn put_smtp_audits_and_get_is_desensitized() {
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;

    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/config/smtp",
        Some(
            r#"{"host":"smtp.example.com","port":465,"username":"postmaster","tls":"implicit","from_address":"ci@example.com","password":"relay-pw"}"#
                .into(),
        ),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "PUT 200");

    // GET 脱敏。
    let resp = req_with_cookie(&app, "GET", "/api/v1/config/smtp", None, Some(&admin)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["configured"], true);
    assert_eq!(body["config"]["host"], "smtp.example.com");
    assert_eq!(body["config"]["port"], 465);
    assert_eq!(body["config"]["tls"], "implicit");
    assert_eq!(body["config"]["from_address"], "ci@example.com");
    assert_eq!(body["config"]["password_set"], true, "回 password_set 布尔");
    assert!(
        body["config"].get("password").is_none(),
        "脱敏：config 不含 password 字段"
    );
    assert!(
        !body.to_string().contains("relay-pw"),
        "脱敏：响应不含明文密码"
    );

    // 审计：audit_log 含 smtp_config_changed（一次 PUT = 一条）。
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT event_type FROM audit_log WHERE event_type = 'smtp_config_changed'")
            .fetch_all(&app.pool)
            .await
            .expect("查审计");
    assert_eq!(rows.len(), 1, "一次 PUT 应记一条 smtp_config_changed");
}

/// 非全局 admin PUT → 403（全局资源、全局 admin 档）。
#[tokio::test]
async fn put_smtp_rejects_non_global_admin() {
    let app = common::test_app().await;
    let _admin = common::setup_and_login(&app).await;
    // 建普通（非 admin）用户并 login。
    let phc = sisyphus_server::auth::hash_password_blocking(USER_PASSWORD);
    sqlx::query(
        "INSERT INTO users (username, password_hash, is_admin, disabled, created_at, updated_at)
         VALUES ('alice', ?, 0, 0, 1, 1)",
    )
    .bind(&phc)
    .execute(&app.pool)
    .await
    .expect("建普通用户");
    let resp = common::post(
        &app,
        "/api/v1/auth/login",
        &format!(r#"{{ "username": "alice", "password": "{USER_PASSWORD}" }}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "alice login");
    let alice = cookie_of(&resp).expect("cookie");

    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/config/smtp",
        Some(
            r#"{"host":"smtp.example.com","port":587,"tls":"none","from_address":"ci@example.com"}"#
                .into(),
        ),
        Some(&alice),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "非全局 admin PUT 应 403");
}

/// GET 未配置时 configured=false、config 省略。
#[tokio::test]
async fn get_smtp_unconfigured_returns_false() {
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;
    let resp = req_with_cookie(&app, "GET", "/api/v1/config/smtp", None, Some(&admin)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["configured"], false);
    assert!(body.get("config").is_none() || body["config"].is_null(), "未配置无 config");
}
