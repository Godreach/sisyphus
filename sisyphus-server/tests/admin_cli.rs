//! `admin create` headless 引导库函数集成测试（票 B5-T8 #80）：
//!
//! 进程内直调 `sisyphus_server::admin::create_admin`（不起 server / 不走 clap），
//! 覆盖 4 条验收：建号成功（argon2id 哈希落库 + 审计入账）、首建闸幂等拒绝
//! （用户表非空可读错误）、建号后用户表 count==1（wizard 不再进入）、校验失败
//! 不落库。仓内无子进程测试先例，house style 是进程内直调（镜像
//! `store/audit.rs::migrated_pool` fixture）。

use sisyphus_server::admin::{AdminCreateError, create_admin};
use sisyphus_server::config::Overrides;
use sisyphus_server::store;

/// 临时数据目录 + 已迁移库（Config::load 建目录布局 + bootstrap 跑迁移）。
async fn fixture() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().expect("临时目录");
    sisyphus_server::config::Config::load(
        dir.path().to_path_buf(),
        Overrides::default(),
        Overrides::default(),
    )
    .expect("目录布局");
    let pool = store::bootstrap(dir.path()).await.expect("bootstrap");
    (dir, pool)
}

/// 直查 users 表的 (count, is_admin, password_hash)。
async fn user_row(pool: &sqlx::SqlitePool, username: &str) -> Option<(i64, String)> {
    sqlx::query_as::<_, (i64, String)>(
        "SELECT is_admin, password_hash FROM users WHERE username = ?",
    )
    .bind(username)
    .fetch_optional(pool)
    .await
    .expect("查 users")
}

/// users 表行数。
async fn user_count(pool: &sqlx::SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await
        .expect("count users")
}

/// audit_log 中 user_created 行数（actor + detail）。
async fn audit_user_created(pool: &sqlx::SqlitePool, actor: &str) -> Vec<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT actor, detail FROM audit_log WHERE event_type = 'user_created' AND actor = ?",
    )
    .bind(actor)
    .fetch_all(pool)
    .await
    .expect("查审计")
}

#[tokio::test]
async fn create_admin_success_hashes_argon2id_and_audits() {
    let (_dir, pool) = fixture().await;
    let user = create_admin(&pool, "admin", "admin-password-1")
        .await
        .expect("建号成功");
    assert_eq!(user.username, "admin");
    assert!(user.is_admin, "首个 admin is_admin=true");

    // 落库：is_admin=1 + argon2id PHC（$argon2id$v=19$m=19456,t=2,p=1$...）。
    assert_eq!(user_count(&pool).await, 1, "用户表 1 行");
    let (is_admin, hash) = user_row(&pool, "admin").await.expect("行应存在");
    assert_eq!(is_admin, 1, "DB is_admin=1");
    assert!(
        hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
        "argon2id OWASP PHC：{hash}"
    );
    assert_ne!(hash, "admin-password-1", "明文永不上库");

    // 审计入账：user_created，actor=admin，detail={"username":"admin"}。
    let rows = audit_user_created(&pool, "admin").await;
    assert_eq!(rows.len(), 1, "一条 user_created 审计");
    assert_eq!(rows[0].0, "admin", "actor = 新用户名");
    assert_eq!(rows[0].1, r#"{"username":"admin"}"#, "detail 记目标用户名");
}

#[tokio::test]
async fn create_admin_refuses_when_instance_initialized() {
    let (_dir, pool) = fixture().await;
    create_admin(&pool, "admin", "admin-password-1")
        .await
        .expect("首次建号");
    // 首建闸：用户表非空即拒——count 闸在 create 之前先跑，故同或异 username
    // 都走 InstanceInitialized（与 setup wizard `count()!=0` 闸同语义；重名
    // 的 StoreError::Unique 因 count 闸先挡而不可达，UsernameTaken 为防御映射）。
    let err = create_admin(&pool, "admin", "another-password-1")
        .await
        .expect_err("实例已初始化应拒（同名）");
    assert!(
        matches!(err, AdminCreateError::InstanceInitialized),
        "同名首建闸拒：{err}"
    );
    let err = create_admin(&pool, "second", "second-password-1")
        .await
        .expect_err("实例已初始化应拒（异名）");
    assert!(
        matches!(err, AdminCreateError::InstanceInitialized),
        "异名首建闸拒：{err}"
    );
    assert_eq!(user_count(&pool).await, 1, "拒绝不增行");
}

#[tokio::test]
async fn create_admin_rejects_invalid_input_without_writing() {
    let (_dir, pool) = fixture().await;
    // 空 username。
    let err = create_admin(&pool, "  ", "admin-password-1")
        .await
        .expect_err("空用户名应拒");
    assert!(matches!(err, AdminCreateError::Validation(_)), "{err}");
    // 短密码（< MIN_PASSWORD_LEN 8）。
    let err = create_admin(&pool, "admin", "short")
        .await
        .expect_err("短密码应拒");
    assert!(matches!(err, AdminCreateError::Validation(_)), "{err}");
    // 非法字符用户名。
    let err = create_admin(&pool, "bad name!", "admin-password-1")
        .await
        .expect_err("非法字符应拒");
    assert!(matches!(err, AdminCreateError::Validation(_)), "{err}");
    // 校验失败不落库、不记审计。
    assert_eq!(user_count(&pool).await, 0, "校验拒不落库");
    assert_eq!(
        audit_user_created(&pool, "admin").await.len(),
        0,
        "校验拒不记审计"
    );
}

#[tokio::test]
async fn create_admin_then_wizard_gate_is_closed() {
    // 验收「建号后 web wizard 不再进入」：建号后用户表 count==1，setup wizard
    // 的 count()!=0 闸即关闭——直接断言 count，wizard 逻辑复用同闸（api::auth::setup）。
    let (_dir, pool) = fixture().await;
    assert_eq!(user_count(&pool).await, 0, "建号前空库");
    create_admin(&pool, "admin", "admin-password-1")
        .await
        .expect("建号");
    assert_eq!(user_count(&pool).await, 1, "建号后非空 → wizard 闸关闭");
}
