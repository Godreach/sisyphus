//! headless 引导：`sisyphus-server admin create` 的建号逻辑（票 B5-T8，
//! ADR-0010）。
//!
//! setup wizard（`api::auth::setup`）的 headless 等价——经 CLI 建首个全局
//! 管理员，跑过即视为引导完成（用户表非空 → wizard 不再进入，既有
//! `UserRepo::count() != 0` 闸）。复用同一建号 + 审计链路：argon2id 哈希
//! （`auth::hash_password`，OWASP m=19MiB/t=2/p=1）、`UserRepo::create` 落库、
//! `AuditRepo::insert(UserCreated)` 入账（actor = 新用户自己，unauthenticated
//! 首建同 setup）。校验复用 `api::auth::validate_new_account`（username 字符集
//! + `MIN_PASSWORD_LEN`），不重复校验代码。
//!
//! 本模块是可测库函数（进程内直调，无需起 server / clap）；`bin/main.rs` 的
//! `admin create` 子命令是薄壳：bootstrap → 读密码 → 调 [`create_admin`] →
//! 打印/退出。测试经此函数直调（tempdir + `store::bootstrap`），不引子进程
//! 测试（仓内无先例，house style 是进程内驱动）。

use sqlx::SqlitePool;

use crate::api::auth::validate_new_account;
use crate::auth::hash_password;
use crate::store::audit::{AuditEvent, AuditRepo};
use crate::store::now_ms;
use crate::store::users::{User, UserRepo};
use crate::store::StoreError;

/// 建首个全局管理员（headless 引导，setup wizard 等价）。
///
/// **首建闸**：用户表非空（`UserRepo::count() != 0`）即拒——与 setup wizard
/// 同一闸，保证「跑过即引导完成、wizard 不再进入」。只能建首个 admin；再建
/// 管理员走 web 全局 admin 建号端点（`POST /api/v1/users`）。
///
/// 步骤镜像 `api::auth::setup`（`api/auth.rs:106-140`）：校验 → argon2id 哈希 →
/// `UserRepo::create(trim, &hash, is_admin=true)` → `AuditRepo::insert(UserCreated,
/// actor=新用户名, detail={"username":...})`。actor 用新用户自己的用户名
/// （unauthenticated 首建，同 setup）。
pub async fn create_admin(
    pool: &SqlitePool,
    username: &str,
    password: &str,
) -> Result<User, AdminCreateError> {
    let users = UserRepo::new(pool.clone());
    // 首建闸：用户表非空即拒（与 setup wizard `count() != 0` 闸同）。
    if users.count().await? != 0 {
        return Err(AdminCreateError::InstanceInitialized);
    }
    // 校验复用 HTTP 路径同款（username 字符集 + MIN_PASSWORD_LEN）。
    if let Err(e) = validate_new_account(username, password) {
        return Err(AdminCreateError::Validation(e.message().to_string()));
    }
    let hash = hash_password(password).await;
    let user = users.create(username.trim(), &hash, true).await?;
    // 审计入账：UserCreated，actor = 新用户名（unauthenticated 首建，同 setup），
    // detail 记目标用户名（审计回放对 user_created 有稳定 schema）。
    let audit = AuditRepo::new(pool.clone());
    audit
        .insert(
            now_ms(),
            &user.username,
            AuditEvent::UserCreated,
            None,
            Some(&serde_json::json!({ "username": user.username }).to_string()),
        )
        .await?;
    Ok(user)
}

/// `admin create` 失败原因（人读消息经 [`Display`]；CLI 据此打印 + exit 1）。
#[derive(Debug)]
pub enum AdminCreateError {
    /// 首建闸：用户表非空（实例已初始化，wizard 已跑过或 web 已建号）。
    InstanceInitialized,
    /// 输入校验失败（username 字符集 / 密码最小长度）；携带人读消息。
    Validation(String),
    /// 存储层错误（开池/读写/审计写；含理论不可达的 Unique——首建闸先挡）。
    Store(StoreError),
}

impl std::fmt::Display for AdminCreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InstanceInitialized => write!(
                f,
                "实例已初始化（用户表非空）：admin create 仅建首个管理员，\
                 再建管理员请经 web 全局 admin 建号端点（POST /api/v1/users）"
            ),
            Self::Validation(msg) => write!(f, "输入校验失败：{msg}"),
            Self::Store(e) => write!(f, "存储错误：{e}"),
        }
    }
}

impl std::error::Error for AdminCreateError {}

impl From<StoreError> for AdminCreateError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}
