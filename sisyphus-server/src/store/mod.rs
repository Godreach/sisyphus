//! store 模块：SQLite 池 + PRAGMA 基线 + 编译期嵌入迁移（迁移前自动备份）。
//!
//! 组合根单一入口 [`bootstrap`]：开池 → 校验待应用迁移 → 备份 db 文件
//! （连 `-wal`/`-shm` 一起，ADR-0004 部署注记）→ 前向迁移（ADR-0010）。
//! repo 层（[`projects::ProjectRepo`] / [`pipelines::PipelineRepo`] /
//! [`builds::BuildRepo`] / [`jobs::JobRepo`] / [`agents::AgentRepo`] /
//! [`triggers::TriggerRepo`]）承载元数据与调度状态读写；trait 缝
//! （[`LogStore`](traits::LogStore) / [`ArtifactStore`](traits::ArtifactStore)）
//! 只定形不实现，随消费批次落同一缝。

pub mod agents;
pub mod artifacts;
pub mod audit;
pub mod builds;
pub mod jobs;
pub mod logs;
pub mod members;
pub mod pipelines;
pub mod projects;
pub mod scm_credentials;
pub mod secrets;
pub mod sessions;
pub mod smtp_config;
pub mod tokens;
pub mod triggers;
pub mod upgrade_packages;
pub mod users;

// 缝定形：LogStore 随日志批次（票 #73）落 SqliteLogStore 实现；
// ArtifactStore/ArtifactMetaRepo 随产物批次（票 #74）落实现。
#[allow(dead_code, unused_imports)]
mod traits;

pub use agents::{AgentVersion, PendingUpgrade};
pub use artifacts::{
    ARTIFACT_NAME_MAX, ARTIFACT_RETENTION_DAYS, LocalDiskArtifactStore, SqliteArtifactMetaRepo,
    validate_artifact_name,
};
pub use logs::SqliteLogStore;
pub use scm_credentials::ScmCredentialRepo;
pub use smtp_config::{SmtpConfigRepo, SmtpTls};
pub use traits::{
    ArtifactMeta, ArtifactMetaRepo, ArtifactStore, ByteStream, LogChunk, LogLocation, LogStore,
};
pub use upgrade_packages::{
    LocalDiskUpgradePackageStore, UpgradePackageBytes, UpgradePackageMeta, UpgradePackageRepo,
    PACKAGE_NAME_MAX, validate_package_name,
};

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use sqlx::{
    SqlitePool,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::config::{BACKUPS_DIR, DB_FILE_NAME};

/// 编译期嵌入的迁移（单二进制自带迁移，ADR-0009）。
static MIGRATOR: Migrator = sqlx::migrate!("src/store/migrations");

/// store 层错误。
#[derive(Debug)]
pub enum StoreError {
    /// 数据库错误。
    Db(sqlx::Error),
    /// 迁移执行错误。
    Migrate(sqlx::migrate::MigrateError),
    /// 磁盘/文件 IO 错误（备份拷贝等）。
    Io(std::io::Error),
    /// Pipeline 定义未过 sisyphus-model 校验（整组透传给 API 层 422）。
    InvalidDefinition(Vec<sisyphus_model::validate::ValidationError>),
    /// 唯一冲突（如项目名已存在）。
    Unique(String),
    /// 目标不存在（项目、pipeline 等）。
    NotFound(String),
    /// 事务内条件更新未命中且重试耗尽（并发写冲突）。
    Conflict(String),
    /// 输入非法（如产物名含路径分隔符——产物名是磁盘路径段，非法名拒绝
    /// 落盘，票 #74）。
    Invalid(String),
    /// 定义 JSON 编解码失败（落库内容与 model 形态不符）。
    DefinitionJson(serde_json::Error),
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        StoreError::Db(e)
    }
}

impl From<sqlx::migrate::MigrateError> for StoreError {
    fn from(e: sqlx::migrate::MigrateError) -> Self {
        StoreError::Migrate(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Db(e) => write!(f, "数据库错误：{e}"),
            StoreError::Migrate(e) => write!(f, "迁移错误：{e}"),
            StoreError::Io(e) => write!(f, "存储 IO 错误：{e}"),
            StoreError::InvalidDefinition(errors) => {
                write!(f, "Pipeline 定义校验失败（{} 处）", errors.len())
            }
            StoreError::Unique(what) => write!(f, "唯一冲突：{what}"),
            StoreError::NotFound(what) => write!(f, "不存在：{what}"),
            StoreError::Conflict(what) => write!(f, "并发写冲突：{what}"),
            StoreError::Invalid(what) => write!(f, "输入非法：{what}"),
            StoreError::DefinitionJson(e) => write!(f, "定义 JSON 编解码失败：{e}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// 打开数据目录里的数据库：建池并应用 PRAGMA 基线
/// （WAL / NORMAL / busy_timeout 5000 / 外键，每连接生效，ADR-0004）。
async fn open_pool(db_path: &Path) -> Result<SqlitePool, StoreError> {
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_millis(5000))
        .foreign_keys(true);
    SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .map_err(StoreError::from)
}

/// 组合根单一入口：开池 → 有待应用迁移则先备份 db 文件 → 前向迁移。
pub async fn bootstrap(data_dir: &Path) -> Result<SqlitePool, StoreError> {
    let db_path = data_dir.join(DB_FILE_NAME);
    let db_existed = db_path.is_file();
    let pool = open_pool(&db_path).await?;

    // 迁移前自动备份（ADR-0010）：只在确有待应用迁移时做，避免每次重启膨胀。
    if db_existed && has_pending_migrations(&pool).await? {
        let dest = backup_db(&pool, data_dir).await?;
        tracing::info!(backup = %dest.display(), "迁移前已备份数据库");
    }

    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

/// 库中已应用版本是否落后于嵌入迁移（`_sqlx_migrations` 不存在视为全新）。
async fn has_pending_migrations(pool: &SqlitePool) -> Result<bool, StoreError> {
    let marker: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?;
    let applied: Vec<i64> = if marker.is_some() {
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations")
            .fetch_all(pool)
            .await?
    } else {
        Vec::new()
    };
    Ok(MIGRATOR
        .migrations
        .iter()
        .any(|m| !applied.contains(&m.version)))
}

/// 迁移前备份：`VACUUM INTO` 在线一致性快照（ADR-0004「或走 backup API」路线），
/// 单文件落 `backups/<毫秒时间戳>/`，免去拷贝运行中的 `-wal`/`-shm`。
async fn backup_db(pool: &SqlitePool, data_dir: &Path) -> Result<PathBuf, StoreError> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| StoreError::Io(std::io::Error::other(e)))?
        .as_millis();
    let dest = data_dir
        .join(BACKUPS_DIR)
        .join(stamp.to_string())
        .join(DB_FILE_NAME);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let dest_str = dest.to_string_lossy().into_owned();
    sqlx::query("VACUUM INTO ?")
        .bind(&dest_str)
        .execute(pool)
        .await?;
    Ok(dest)
}

/// 当前 Unix 毫秒时间戳（落库时间列统一用毫秒，与 Revision 语义一致）。
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时钟晚于 Unix 纪元")
        .as_millis() as i64
}

/// 是否 UNIQUE 约束冲突（SQLite 扩展码 2067；兜底看错误文本）。
pub(crate) fn is_unique_violation(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db) => {
            db.code().as_deref() == Some("2067") || db.message().contains("UNIQUE constraint")
        }
        _ => false,
    }
}

/// 是否写锁竞争类错误（BUSY / BUSY_SNAPSHOT：busy_timeout 耗尽或读快照过期）。
pub(crate) fn is_busy(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db) => matches!(db.code().as_deref(), Some("5") | Some("517")),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bootstrap_applies_pragma_baseline() {
        let dir = layout_fixture().expect("布局夹具");
        let pool = bootstrap(dir.path()).await.expect("bootstrap 应成功");

        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .expect("读 journal_mode");
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&pool)
            .await
            .expect("读 synchronous");
        let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(&pool)
            .await
            .expect("读 busy_timeout");
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .expect("读 foreign_keys");

        assert_eq!(journal_mode, "wal");
        assert_eq!(synchronous, 1, "NORMAL = 1");
        assert_eq!(busy_timeout, 5000);
        assert_eq!(foreign_keys, 1, "ON = 1");
    }

    #[tokio::test]
    async fn bootstrap_creates_schema_on_first_boot_and_second_boot_is_idempotent() {
        let dir = layout_fixture().expect("布局夹具");

        let pool = bootstrap(dir.path()).await.expect("首启应建表");
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .expect("读表清单");
        for expected in [
            "pipelines",
            "projects",
            "users",
            "sessions",
            "personal_access_tokens",
            "project_members",
            "secrets",
            "audit_log",
            "agents",
            "builds",
            "jobs",
            "triggers",
            "logs",
            "artifacts",
            "project_scm_credentials",
            "global_smtp_config",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "缺表 {expected}：{tables:?}"
            );
        }
        pool.close().await;

        // 二启：无待应用迁移，迁移幂等。
        let pool = bootstrap(dir.path()).await.expect("二启应幂等成功");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects")
            .fetch_one(&pool)
            .await
            .expect("二启后表应可用");
        assert_eq!(count, 0);

        // 无待应用迁移时不产生备份（避免每次重启膨胀 backups/）。
        assert_eq!(
            count_backups(dir.path()).expect("枚举备份"),
            0,
            "幂等二启不应产生备份"
        );
    }

    #[tokio::test]
    async fn bootstrap_backs_up_db_before_pending_migration() {
        let dir = layout_fixture().expect("布局夹具");

        // 先正常建库并写入一行数据（让待备份的库非空）。
        let pool = bootstrap(dir.path()).await.expect("首启");
        sqlx::query("INSERT INTO projects (name, scm_type, scm_url, created_at, updated_at) VALUES ('demo', 'git', 'https://example.com/repo', 0, 0)")
            .execute(&pool)
            .await
            .expect("写入项目");
        pool.close().await;

        // 模拟旧库升级：抹掉迁移标记与业务表，使下一次 bootstrap 见到待应用迁移。
        let pool = open_raw_pool_for_test(dir.path()).await;
        for stmt in [
            "DROP TABLE pipelines",
            "DROP TABLE projects",
            "DROP TABLE users",
            "DROP TABLE sessions",
            "DROP TABLE personal_access_tokens",
            "DROP TABLE project_members",
            "DROP TABLE secrets",
            "DROP TABLE audit_log",
            "DROP TABLE agents",
            "DROP TABLE builds",
            "DROP TABLE jobs",
            "DROP TABLE triggers",
            "DROP TABLE logs",
            "DROP TABLE artifacts",
            "DROP TABLE upgrade_packages",
            "DROP TABLE project_scm_credentials",
            "DROP TABLE global_smtp_config",
        ] {
            sqlx::raw_sql(stmt)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("执行 {stmt}: {e}"));
        }
        sqlx::query("DELETE FROM _sqlx_migrations")
            .execute(&pool)
            .await
            .expect("清迁移标记");
        pool.close().await;

        let pool = bootstrap(dir.path())
            .await
            .expect("带待应用迁移的 bootstrap 应成功");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .expect("迁移标记");
        assert_eq!(
            rows as usize,
            MIGRATOR.migrations.len(),
            "嵌入迁移应全部重新应用"
        );

        // 迁移前备份产生：backups/<stamp>/sisyphus.db，且为迁移前的旧内容（非空）。
        let backups = count_backups(dir.path()).expect("枚举备份");
        assert_eq!(backups, 1, "恰好一份迁移前备份");
        let backup_db = latest_backup_db(dir.path()).expect("备份内应有 db 文件");
        let size = std::fs::metadata(&backup_db).expect("备份元数据").len();
        assert!(size > 0, "备份应是迁移前的非空旧库");
    }

    /// 测试夹具：走生产序列 Config::load（建目录布局）后返回临时数据目录。
    fn layout_fixture() -> std::io::Result<tempfile::TempDir> {
        let dir = tempfile::tempdir()?;
        crate::config::Config::load(
            dir.path().to_path_buf(),
            crate::config::Overrides::default(),
            crate::config::Overrides::default(),
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(dir)
    }

    /// 测试辅助：不跑迁移只开池（模拟旧库状态操作）。
    async fn open_raw_pool_for_test(data_dir: &Path) -> SqlitePool {
        let db_path = data_dir.join(DB_FILE_NAME);
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .foreign_keys(false);
        SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .expect("测试开池")
    }

    fn count_backups(data_dir: &Path) -> std::io::Result<usize> {
        Ok(std::fs::read_dir(data_dir.join(BACKUPS_DIR))?.count())
    }

    #[tokio::test]
    async fn foreign_keys_enforced_on_insert() {
        let dir = layout_fixture().expect("布局夹具");
        let pool = bootstrap(dir.path()).await.expect("bootstrap");

        // 引用不存在的项目：外键约束应拒绝（PRAGMA foreign_keys=ON 落到行为上）。
        let err = sqlx::query(
            "INSERT INTO pipelines (project_id, name, definition, revision, operator, created_at, updated_at)
             VALUES (999, 'p', '{}', 1, 'tester', 0, 0)",
        )
        .execute(&pool)
        .await
        .expect_err("外键违规应报错");
        assert!(
            err.to_string().contains("FOREIGN KEY"),
            "应为外键错误：{err}"
        );
    }

    fn latest_backup_db(data_dir: &Path) -> Option<PathBuf> {
        let backups = data_dir.join(BACKUPS_DIR);
        let mut stamps: Vec<PathBuf> = std::fs::read_dir(&backups)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        stamps.sort();
        stamps
            .pop()
            .map(|p| p.join(DB_FILE_NAME))
            .filter(|p| p.is_file())
    }
}
