//! 保留策略清理（票 #78/#121/#132，ADR-0013/0026/0027）：旧 SQLite 日志与本地产物共享
//! per-build 保留期（Server 全局配置，默认 30 天），每日低频扫描清理过期
//! 构建的日志与本地产物；构建记录及其它后端产物永久保留。
//! 新日志归档按 attempt 执行结束时间和独立 log_retention_days 清理，保留墓碑。
//!
//! - [`sweep`]：每日扫描。**旧 per-build 保留语义**——一次构建的旧日志与本地产物是
//!   一个整体（重跑 attempt+1 会追加新日志、同名再传刷新产物，同构建数据
//!   落在同一保留期），故过期判定取该构建「最新活动时刻」= max(最近日志
//!   落库时刻, 最近产物上传时刻)，早于 cutoff（now - retention_days）即整
//!   构建过期，日志 chunk 与本地产物一起删。只删数据，builds/jobs 和其它
//!   后端记录保留。
//! - [`delete_build_data`]：手动删构建复用同一裁剪（[`purge_build`]）——
//!   立即删除该构建的日志与本地产物、回收空目录；其它后端留给自己的删除
//!   流程。
//! - 产物字节层容错：元数据与磁盘文件竞争（上传半截/磁盘丢失）时缺失文件
//!   记日志跳过、目录非空不回收——不炸扫描、不误删他人数据。
//! - 与迁移备份协同：清理只删 `artifacts/<build_id>/` 目录内字节，绝不触碰
//!   `backups/`（ADR-0010 迁移前备份目录，与产物布局同级的兄弟目录）。

use std::path::Path;

use crate::storage::S3Client;
use sqlx::SqlitePool;

use super::StoreError;

/// 每日清理扫描的周期（ADR-0013「每日低频扫描」；启动先跑一轮，之后 24h
/// 一轮，MissedTickBehavior::Skip——错过不补跑）。
pub const CLEANUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// 后台每日清理任务（main.rs 与 trigger/心跳扫描同生命周期 spawn，单实例
/// 纪律）：启动先跑一轮（初次部署即清一次遗留），之后按 [`CLEANUP_INTERVAL`]
/// 周期跑 [`sweep`]。运行期不返回（进程级生命周期），错误记日志续跑。
pub async fn run_daily_cleanup(
    pool: SqlitePool,
    artifacts_root: std::path::PathBuf,
    retention_days: i64,
) {
    run_daily_cleanup_with_s3(pool, artifacts_root, retention_days, None).await;
}

/// 生产清理器额外接受可选 S3；运行期故障只留积压，下一轮重试。
pub async fn run_daily_cleanup_with_s3(
    pool: SqlitePool,
    artifacts_root: std::path::PathBuf,
    retention_days: i64,
    s3: Option<std::sync::Arc<S3Client>>,
) {
    run_daily_cleanup_with_log_retention(pool, artifacts_root, retention_days, 30, s3).await;
}

/// 新日志归档使用独立的保留配置；旧 SQLite 日志沿用原规则。
pub async fn run_daily_cleanup_with_log_retention(
    pool: SqlitePool,
    artifacts_root: std::path::PathBuf,
    retention_days: i64,
    log_retention_days: i64,
    s3: Option<std::sync::Arc<S3Client>>,
) {
    let mut ticker = tokio::time::interval(CLEANUP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // 首 tick 立即触发（interval 首 tick 即 now），等价「启动先跑一轮」。
        ticker.tick().await;
        match sweep_with_log_retention(
            &pool,
            &artifacts_root,
            crate::store::now_ms(),
            retention_days,
            log_retention_days,
            s3.as_deref(),
        )
        .await
        {
            Ok(report) if report.builds_purged > 0 => tracing::info!(
                builds = report.builds_purged,
                logs = report.logs_deleted,
                artifact_files = report.artifact_files_deleted,
                artifact_meta = report.artifact_meta_deleted,
                dirs = report.empty_dirs_reclaimed,
                "保留清理：已清理过期构建数据"
            ),
            Ok(_) => tracing::trace!("保留清理：无过期构建"),
            Err(e) => tracing::warn!(error = %e, "保留清理扫描失败（下轮重试）"),
        }
    }
}

/// 单轮清理报告（测试断言 + 日志计数）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CleanupReport {
    /// 过期被清理的构建数（sweep 的构建粒度）。
    pub builds_purged: usize,
    /// 删除的日志 chunk 行数。
    pub logs_deleted: i64,
    /// 删除的产物字节文件数（磁盘）。
    pub artifact_files_deleted: usize,
    /// 删除的产物元数据行数。
    pub artifact_meta_deleted: i64,
    /// 回收的空构建目录数（`artifacts/<build_id>/`）。
    pub empty_dirs_reclaimed: usize,
    /// 删除的终态日志归档行数。
    pub log_archives_deleted: i64,
    /// 删除的归档正文/索引文件数。
    pub log_archive_files_deleted: usize,
}

/// 每日扫描：找出「最新活动早于 cutoff」的过期构建，逐个 [`purge_build`]。
/// `now` 注入（生产 [`now_ms`]，测试假时钟）；`retention_days` 来自合并后
/// 配置（默认 30）。单个构建裁剪失败记日志不中断全扫（运维面容错）。
pub async fn sweep(
    pool: &SqlitePool,
    artifacts_root: &Path,
    now: i64,
    retention_days: i64,
) -> Result<CleanupReport, StoreError> {
    sweep_with_s3(pool, artifacts_root, now, retention_days, None).await
}

/// 同 [`sweep`]，并清理 S3 归档及中断后遗留的候选对象。
pub async fn sweep_with_s3(
    pool: &SqlitePool,
    artifacts_root: &Path,
    now: i64,
    retention_days: i64,
    s3: Option<&S3Client>,
) -> Result<CleanupReport, StoreError> {
    sweep_with_log_retention(pool, artifacts_root, now, retention_days, 30, s3).await
}

/// 一轮独立保留期扫描（假时钟为测试缝）。
pub async fn sweep_with_log_retention(
    pool: &SqlitePool,
    artifacts_root: &Path,
    now: i64,
    retention_days: i64,
    log_retention_days: i64,
    s3: Option<&S3Client>,
) -> Result<CleanupReport, StoreError> {
    let cutoff = now - retention_days.max(1) * 24 * 60 * 60 * 1000;
    let log_cutoff = now.saturating_sub(
        log_retention_days
            .max(1)
            .saturating_mul(24 * 60 * 60 * 1000),
    );
    let mut report = purge_expired_archives(pool, log_cutoff, s3).await?;
    let build_ids = sqlx::query_scalar::<_, i64>(
        "SELECT build_id FROM (
             SELECT build_id, created_at AS last FROM logs
             UNION ALL
             SELECT build_id, created_at AS last FROM artifacts WHERE backend = 'local'
         )
         GROUP BY build_id HAVING MAX(last) < ?",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;

    for build_id in build_ids {
        match purge_build(pool, artifacts_root, build_id, s3, false).await {
            Ok(partial) => {
                report.builds_purged += 1;
                report.logs_deleted += partial.logs_deleted;
                report.artifact_files_deleted += partial.artifact_files_deleted;
                report.artifact_meta_deleted += partial.artifact_meta_deleted;
                report.empty_dirs_reclaimed += partial.empty_dirs_reclaimed;
                report.log_archives_deleted += partial.log_archives_deleted;
                report.log_archive_files_deleted += partial.log_archive_files_deleted;
            }
            Err(e) => tracing::warn!(build_id, error = %e, "保留清理：构建数据裁剪失败"),
        }
    }
    // 归档行可能已删，但迟到的 CopyObject 仍可在首次 DELETE 后生成对象。
    // 保留候选 key 登记，后续每日扫描继续幂等删除（S3 不可用则下次重试）。
    if let Some(s3) = s3 {
        let orphan_keys = sqlx::query_scalar::<_, String>(
            "SELECT c.key FROM log_archive_publish_candidates c
             WHERE NOT EXISTS (SELECT 1 FROM log_archives a WHERE a.job_id=c.job_id AND a.attempt=c.attempt)",
        )
        .fetch_all(pool)
        .await?;
        for key in orphan_keys {
            if let Err(error) = s3.delete_object(&key).await {
                tracing::warn!(key, error = %error, "迟到的日志归档候选对象清理失败，保留登记待重试");
            }
        }
    }
    // 批量 DELETE 后收缩 WAL（ADR-0004「定期 DELETE + PRAGMA
    // wal_checkpoint(TRUNCATE)」）：大批过期行删除会让 -wal 短暂膨胀，
    // checkpoint 把已删页面落回主库并截断 -wal，防单文件无限增长。
    if report.logs_deleted > 0 || report.artifact_meta_deleted > 0 {
        sqlx::raw_sql("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(pool)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "保留清理：WAL checkpoint 失败（无害，下轮重试）");
                e
            })
            .ok();
    }
    Ok(report)
}

/// 按 attempt 终态归档时间独立清理归档；查看、重试或同构建的新活动不会
/// 延长旧 attempt 的日志保留期。`created_at` 在终态 pending 首次登记时写入。
async fn purge_expired_archives(
    pool: &SqlitePool,
    cutoff: i64,
    s3: Option<&S3Client>,
) -> Result<CleanupReport, StoreError> {
    let rows = sqlx::query_as::<_, (i64, i32, String, String, String, String, Option<String>)>(
        "SELECT job_id, attempt, path, index_path, state, backend, temp_path FROM log_archives
         WHERE state='lost' OR COALESCE(execution_finished_at, created_at) <= ?",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    let mut report = CleanupReport::default();
    for (job_id, attempt, path, index_path, state, backend, temp_path) in &rows {
        // 先撤销可读/发布资格，之后字节删除失败只产生清理积压，不复活日志。
        if state != "lost" {
            sqlx::query(
                "UPDATE log_archives SET state='lost', lost_reason='retention_expired', lost_at=?
                WHERE job_id=? AND attempt=? AND state!='lost'",
            )
            .bind(super::now_ms())
            .bind(job_id)
            .bind(attempt)
            .execute(pool)
            .await?;
        }
        if !delete_archive_bytes(
            pool,
            ArchiveBytes {
                job_id: *job_id,
                attempt: *attempt,
                backend,
                path,
                index_path,
                temp_path: temp_path.as_deref(),
            },
            s3,
            &mut report,
        )
        .await?
        {
            continue;
        }
        tracing::debug!(job_id, attempt, "已按 attempt 保留期删除日志归档");
        sqlx::query(
            "UPDATE log_archives SET index_json=NULL WHERE job_id=? AND attempt=? AND state='lost'",
        )
        .bind(job_id)
        .bind(attempt)
        .execute(pool)
        .await?;
        if state != "lost" {
            report.log_archives_deleted += 1;
        }
    }
    Ok(report)
}

struct ArchiveBytes<'a> {
    job_id: i64,
    attempt: i32,
    backend: &'a str,
    path: &'a str,
    index_path: &'a str,
    temp_path: Option<&'a str>,
}

/// 已撤销的日志正文立即尝试删除；失败保留登记，日常扫描会重试。
pub(crate) async fn cleanup_lost_archives(
    pool: &SqlitePool,
    s3: Option<&S3Client>,
) -> Result<(), StoreError> {
    purge_expired_archives(pool, i64::MIN, s3).await?;
    Ok(())
}

async fn delete_archive_bytes(
    pool: &SqlitePool,
    archive: ArchiveBytes<'_>,
    s3: Option<&S3Client>,
    report: &mut CleanupReport,
) -> Result<bool, StoreError> {
    // 候选对象属于发布尝试，不属于行当前的后端：pending S3 改走 local
    // 后也须先回收它们，不能因行级联删除而丢失最后的 S3 key 线索。
    let candidates = sqlx::query_scalar::<_, String>(
        "SELECT key FROM log_archive_publish_candidates WHERE job_id=? AND attempt=?",
    )
    .bind(archive.job_id)
    .bind(archive.attempt)
    .fetch_all(pool)
    .await?;
    if !candidates.is_empty() && s3.is_none() {
        return Ok(false);
    }
    if let Some(s3) = s3 {
        for key in &candidates {
            if let Err(error) = s3.delete_object(key).await {
                tracing::warn!(key, error = %error, "S3 日志归档清理失败，保留元数据待重试");
                return Ok(false);
            }
        }
    }
    if archive.backend == "s3" {
        let Some(s3) = s3 else { return Ok(false) };
        for key in [archive.temp_path, Some(archive.path)]
            .into_iter()
            .flatten()
        {
            if let Err(error) = s3.delete_object(key).await {
                tracing::warn!(key, error = %error, "S3 日志归档清理失败，保留元数据待重试");
                return Ok(false);
            }
        }
        return Ok(true);
    }
    for candidate in [archive.path, archive.index_path] {
        match tokio::fs::remove_file(candidate).await {
            Ok(()) => report.log_archive_files_deleted += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(StoreError::Io(e)),
        }
    }
    Ok(true)
}

/// 手动删构建的数据裁剪（REST DELETE 端点消费）：立即删除该构建的日志与
/// 本地产物（文件 + 元数据）并回收空目录；其它后端记录保留给各自的异步
/// 删除流程。构建记录（builds/jobs 行）保留（ADR-0013/0026 语义）。构建
/// 不存在也返回空报告（幂等）。
pub async fn delete_build_data(
    pool: &SqlitePool,
    artifacts_root: &Path,
    build_id: i64,
) -> Result<CleanupReport, StoreError> {
    delete_build_data_with_s3(pool, artifacts_root, build_id, None).await
}

/// 手动删除构建时清理所有可用后端；S3 暂不可用的行保留供每日重试。
pub async fn delete_build_data_with_s3(
    pool: &SqlitePool,
    artifacts_root: &Path,
    build_id: i64,
    s3: Option<&S3Client>,
) -> Result<CleanupReport, StoreError> {
    purge_build(pool, artifacts_root, build_id, s3, true).await
}

/// 单个构建的数据裁剪：删本地后端的磁盘产物文件 → 删 logs 行 + 本地产物
/// 元数据行（一事务）→ 回收空构建目录。其它后端不对应 Server 本地路径，
/// 由各自的删除流程处理。
async fn purge_build(
    pool: &SqlitePool,
    artifacts_root: &Path,
    build_id: i64,
    s3: Option<&S3Client>,
    include_archives: bool,
) -> Result<CleanupReport, StoreError> {
    let mut report = CleanupReport::default();

    if include_archives {
        // 先固化清理决定，包括尚未到达 Server 的终态日志；迟到补传不得复活。
        sqlx::query(
            "INSERT INTO log_archives (job_id, attempt, state, path, index_path, created_at,
            execution_finished_at, lost_reason, lost_at)
            SELECT id, attempt, 'lost', '', '', ?, finished_at, 'build_data_cleaned', ? FROM jobs
            WHERE build_id=? AND status NOT IN ('running', 'unknown', 'queued')
            ON CONFLICT(job_id, attempt) DO UPDATE SET state='lost',
            lost_reason=COALESCE(log_archives.lost_reason, 'build_data_cleaned'),
            lost_at=COALESCE(log_archives.lost_at, excluded.lost_at)",
        )
        .bind(super::now_ms())
        .bind(super::now_ms())
        .bind(build_id)
        .execute(pool)
        .await?;
        sqlx::query("UPDATE log_archives SET state='lost', lost_reason=COALESCE(lost_reason, 'build_data_cleaned'),
            lost_at=COALESCE(lost_at, ?) WHERE job_id IN (SELECT id FROM jobs WHERE build_id=?)")
            .bind(super::now_ms()).bind(build_id).execute(pool).await?;
    }

    // 归档正文与索引不在 artifacts/<build> 下，先按 job 归属取出并删除；
    // 删除成功后清除索引与临时定位，保留 lost 墓碑阻止迟到补传复活。
    let archives = sqlx::query_as::<_, (i64, i32, String, String, String, Option<String>)>(
        "SELECT a.job_id, a.attempt, a.path, a.index_path, a.backend, a.temp_path FROM log_archives a
         JOIN jobs j ON j.id = a.job_id WHERE j.build_id = ? AND ?",
    )
    .bind(build_id)
    .bind(include_archives)
    .fetch_all(pool)
    .await?;
    let mut cleaned = Vec::new();
    for (job_id, attempt, path, index_path, backend, temp_path) in &archives {
        if delete_archive_bytes(
            pool,
            ArchiveBytes {
                job_id: *job_id,
                attempt: *attempt,
                backend,
                path,
                index_path,
                temp_path: temp_path.as_deref(),
            },
            s3,
            &mut report,
        )
        .await?
        {
            cleaned.push((*job_id, *attempt));
            tracing::debug!(build_id, job_id, "已删除任务日志归档文件");
        }
    }

    // 产物字节先删。path 可能包含任务 attempt 隔离键，不能再由 public name
    // 反推；旧行 path 同样是 build/name 形式，直接按根拼接兼容两者。
    let paths = sqlx::query_scalar::<_, String>(
        "SELECT path FROM artifacts WHERE build_id = ? AND backend = 'local'",
    )
    .bind(build_id)
    .fetch_all(pool)
    .await?;
    for relative in &paths {
        let path = artifacts_root.join(relative);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => report.artifact_files_deleted += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // 元数据有行但磁盘已无（上传半截/外部删除）：不炸，照删元数据。
                tracing::trace!(build_id, artifact = %relative, "产物磁盘文件已不存在");
            }
            Err(e) => {
                return Err(StoreError::Io(e));
            }
        }
    }

    // 日志 chunk + 产物元数据行：一事务删（外键关联均以 build_id 定位）。
    let mut tx = pool.begin().await?;
    let logs = sqlx::query("DELETE FROM logs WHERE build_id = ?")
        .bind(build_id)
        .execute(&mut *tx)
        .await?;
    let metas = sqlx::query("DELETE FROM artifacts WHERE build_id = ? AND backend = 'local'")
        .bind(build_id)
        .execute(&mut *tx)
        .await?;
    let mut archives_deleted = 0;
    for (job_id, attempt) in cleaned {
        archives_deleted += sqlx::query(
            "UPDATE log_archives SET index_json=NULL, temp_path=NULL WHERE job_id=? AND attempt=?",
        )
        .bind(job_id)
        .bind(attempt)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    report.logs_deleted = logs.rows_affected() as i64;
    report.artifact_meta_deleted = metas.rows_affected() as i64;
    report.log_archives_deleted = archives_deleted as i64;

    // 空目录回收：仅当目录已空（全部产物删净）才移除；非空/不存在忽略——
    // 绝不误删非本构建数据（目录即 build_id，命名空间隔离）。
    if !paths.is_empty() {
        let dir = artifacts_root.join(build_id.to_string());
        match tokio::fs::remove_dir(&dir).await {
            Ok(()) => report.empty_dirs_reclaimed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                // 目录内残留（乱入文件/临时文件）：留待人工/下一轮，不删数据。
                tracing::trace!(build_id, "产物目录非空，不回收");
            }
            Err(e) => return Err(StoreError::Io(e)),
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时库装配：Config::load（建 artifacts/ 布局）+ bootstrap（迁移含
    /// 0011 logs / 0012 artifacts / 0016 索引）+ 项目行。
    async fn fixture() -> (tempfile::TempDir, SqlitePool, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("临时数据目录");
        crate::config::Config::load(
            dir.path().to_path_buf(),
            crate::config::Overrides::default(),
            crate::config::Overrides::default(),
        )
        .expect("目录布局");
        let pool = crate::store::bootstrap(dir.path())
            .await
            .expect("bootstrap");
        sqlx::query("INSERT INTO projects (name, scm_type, scm_url, created_at, updated_at) VALUES ('demo', 'git', 'https://example.com/r', 0, 0)")
            .execute(&pool)
            .await
            .expect("建项目");
        let artifacts_root = dir.path().join(crate::config::ARTIFACTS_DIR);
        (dir, pool, artifacts_root)
    }

    /// 建一个构建行 + 一个任务行（logs.job_id 外键），返回 (build_id, job_id)。
    async fn create_build(
        pool: &SqlitePool,
        project_id: i64,
        pipeline: &str,
        number: i64,
    ) -> (i64, i64) {
        let build_id = sqlx::query(
            "INSERT INTO builds (project_id, pipeline_name, number, status, trigger, trigger_detail, attempt, snapshot, updated_at)
             VALUES (?, ?, ?, 'succeeded', 'manual', '{}', 1, '{}', 0)",
        )
        .bind(project_id)
        .bind(pipeline)
        .bind(number)
        .execute(pool)
        .await
        .expect("建构建")
        .last_insert_rowid();
        let job_id = sqlx::query(
            "INSERT INTO jobs (build_id, stage_index, name, status, attempt, labels, timeout_minutes, retry_count, allow_failure)
             VALUES (?, 0, 'compile', 'succeeded', 1, '[]', 0, 0, 0)",
        )
        .bind(build_id)
        .execute(pool)
        .await
        .expect("建任务")
        .last_insert_rowid();
        (build_id, job_id)
    }

    /// 直插一条日志 chunk（created_at 可控——假时钟缝）。
    async fn insert_log(pool: &SqlitePool, build_id: i64, job_id: i64, created_at: i64) {
        sqlx::query(
            "INSERT INTO logs (build_id, job_id, attempt, start_seq, end_seq, step, stream, data, created_at)
             VALUES (?, ?, 1, 0, 0, -1, '', X'1f8b', ?)",
        )
        .bind(build_id)
        .bind(job_id)
        .bind(created_at)
        .execute(pool)
        .await
        .expect("插日志");
    }

    /// 直插一条产物元数据 + 落对应磁盘文件（created_at 可控）。
    async fn insert_artifact(
        pool: &SqlitePool,
        artifacts_root: &Path,
        build_id: i64,
        name: &str,
        created_at: i64,
    ) {
        let retention_until = created_at + 30 * 24 * 60 * 60 * 1000;
        sqlx::query(
            "INSERT INTO artifacts (build_id, name, path, size, sha256, created_at, retention_until)
             VALUES (?, ?, ?, 3, 'abc', ?, ?)",
        )
        .bind(build_id)
        .bind(name)
        .bind(format!("{build_id}/{name}"))
        .bind(created_at)
        .bind(retention_until)
        .execute(pool)
        .await
        .expect("插产物元数据");
        let dir = artifacts_root.join(build_id.to_string());
        std::fs::create_dir_all(&dir).expect("建产物目录");
        std::fs::write(dir.join(name), b"xyz").expect("写产物文件");
    }

    /// 时钟基线（epoch 起毫秒），天数换算。
    const NOW: i64 = 10_000_000_000;
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;

    /// 审计：builds 记录数（保留断言）。
    async fn build_count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM builds")
            .fetch_one(pool)
            .await
            .expect("builds 计数")
    }

    /// AC（票 #78）：per-build 保留边界——29 天留 / 31 天删；日志与产物一起
    /// 裁（同一构建是一个整体）；构建记录保留。
    #[tokio::test]
    async fn sweep_deletes_expired_build_and_keeps_fresh_and_records() {
        let (dir, pool, artifacts_root) = fixture().await;
        let (fresh, fresh_job) = create_build(&pool, 1, "release", 1).await;
        let (expired, expired_job) = create_build(&pool, 1, "release", 2).await;

        // fresh：29 天前（cutoff 内，留）。expired：31 天前（cutoff 外，删）。
        insert_log(&pool, fresh, fresh_job, NOW - 29 * DAY_MS).await;
        insert_artifact(
            &pool,
            &artifacts_root,
            fresh,
            "fresh.bin",
            NOW - 29 * DAY_MS,
        )
        .await;
        insert_log(&pool, expired, expired_job, NOW - 31 * DAY_MS).await;
        insert_artifact(
            &pool,
            &artifacts_root,
            expired,
            "old.bin",
            NOW - 31 * DAY_MS,
        )
        .await;

        let report = sweep(&pool, &artifacts_root, NOW, 30)
            .await
            .expect("扫描应成功");
        assert_eq!(report.builds_purged, 1, "只清过期构建");
        assert_eq!(report.logs_deleted, 1, "过期构建的日志 chunk 删");
        assert_eq!(report.artifact_meta_deleted, 1, "过期构建的产物元数据删");
        assert_eq!(report.artifact_files_deleted, 1, "过期构建的产物文件删");
        assert_eq!(report.empty_dirs_reclaimed, 1, "产物目录回收");

        // fresh 构建的日志/产物/磁盘字节保留（29 天留）。
        let fresh_logs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM logs WHERE build_id = ?")
            .bind(fresh)
            .fetch_one(&pool)
            .await
            .expect("fresh 日志");
        assert_eq!(fresh_logs, 1, "29 天留");
        let fresh_meta: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE build_id = ?")
                .bind(fresh)
                .fetch_one(&pool)
                .await
                .expect("fresh 元数据");
        assert_eq!(fresh_meta, 1, "29 天留");
        assert!(
            artifacts_root
                .join(fresh.to_string())
                .join("fresh.bin")
                .exists(),
            "fresh 产物文件保留"
        );

        // 过期构建：日志/元数据/磁盘全清、目录回收。
        let old_logs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM logs WHERE build_id = ?")
            .bind(expired)
            .fetch_one(&pool)
            .await
            .expect("expired 日志");
        assert_eq!(old_logs, 0, "31 天删");
        assert!(
            !artifacts_root.join(expired.to_string()).exists(),
            "过期构建产物目录已回收"
        );

        // 构建记录永久保留（状态/号/时长可查）。
        assert_eq!(build_count(&pool).await, 2, "builds 行保留");
        let _ = dir;
    }

    /// #121：混合后端构建清理只裁剪旧本地产物，不把 S3 记录当成本地路径。
    #[tokio::test]
    async fn sweep_preserves_non_local_artifacts_in_mixed_build() {
        let (dir, pool, artifacts_root) = fixture().await;
        let (build_id, job_id) = create_build(&pool, 1, "release", 1).await;
        insert_log(&pool, build_id, job_id, NOW - 31 * DAY_MS).await;
        insert_artifact(
            &pool,
            &artifacts_root,
            build_id,
            "legacy.bin",
            NOW - 31 * DAY_MS,
        )
        .await;
        sqlx::query(
            "INSERT INTO artifacts
                (build_id, job_id, attempt, backend, state, name, path, size, sha256,
                 created_at, retention_until)
             VALUES (?, ?, 1, 's3', 'ready', 'remote.bin', 'objects/remote.bin', 3,
                     'abc', ?, ?)",
        )
        .bind(build_id)
        .bind(job_id)
        .bind(NOW - 31 * DAY_MS)
        .bind(NOW - DAY_MS)
        .execute(&pool)
        .await
        .expect("插 S3 元数据");

        let report = sweep(&pool, &artifacts_root, NOW, 30).await.expect("扫描");
        assert_eq!(report.logs_deleted, 1);
        assert_eq!(report.artifact_files_deleted, 1);
        assert_eq!(report.artifact_meta_deleted, 1, "只删除本地元数据");
        let remaining: Vec<(String, String)> =
            sqlx::query_as("SELECT name, backend FROM artifacts WHERE build_id = ?")
                .bind(build_id)
                .fetch_all(&pool)
                .await
                .expect("查询剩余产物");
        assert_eq!(remaining, vec![("remote.bin".into(), "s3".into())]);
        assert!(!artifacts_root.join(build_id.to_string()).exists());
        let _ = dir;
    }

    /// AC：产物与日志互为裁剪触发器——构建只有产物过期（无日志）也应被清。
    #[tokio::test]
    async fn sweep_picks_up_build_with_only_expired_artifacts() {
        let (dir, pool, artifacts_root) = fixture().await;
        let only_artifact = create_build(&pool, 1, "release", 1).await;
        insert_artifact(
            &pool,
            &artifacts_root,
            only_artifact.0,
            "a.bin",
            NOW - 31 * DAY_MS,
        )
        .await;

        let report = sweep(&pool, &artifacts_root, NOW, 30).await.expect("扫描");
        assert_eq!(report.builds_purged, 1, "无日志但产物过期也清");
        assert_eq!(report.artifact_meta_deleted, 1);
        assert!(!artifacts_root.join(only_artifact.0.to_string()).exists());
        let _ = dir;
    }

    /// AC：清理不触碰 backups/ 迁移备份目录（删产物不碰备份，ADR-0010）。
    #[tokio::test]
    async fn sweep_leaves_backups_untouched() {
        let (dir, pool, artifacts_root) = fixture().await;
        let expired = create_build(&pool, 1, "release", 1).await;
        insert_log(&pool, expired.0, expired.1, NOW - 31 * DAY_MS).await;

        // 在 backups/ 放一个假备份文件（模拟迁移前备份存在）。
        let backups = dir.path().join(crate::config::BACKUPS_DIR).join("123");
        std::fs::create_dir_all(&backups).expect("备份目录");
        std::fs::write(backups.join("sisyphus.db"), b"backup").expect("假备份");

        let report = sweep(&pool, &artifacts_root, NOW, 30).await.expect("扫描");
        assert_eq!(report.logs_deleted, 1, "过期日志删");
        assert!(
            dir.path()
                .join(crate::config::BACKUPS_DIR)
                .join("123")
                .join("sisyphus.db")
                .exists(),
            "backups/ 备份文件不受清理影响"
        );
        assert_eq!(build_count(&pool).await, 1, "构建记录保留");
    }

    /// AC：无过期数据时扫描是 no-op（每日空转不删任何东西）。
    #[tokio::test]
    async fn sweep_noop_when_nothing_expired() {
        let (dir, pool, artifacts_root) = fixture().await;
        let b = create_build(&pool, 1, "release", 1).await;
        insert_log(&pool, b.0, b.1, NOW - 10 * DAY_MS).await;
        insert_artifact(&pool, &artifacts_root, b.0, "x.bin", NOW - 5 * DAY_MS).await;

        let report = sweep(&pool, &artifacts_root, NOW, 30).await.expect("扫描");
        assert_eq!(report, CleanupReport::default(), "全留即空报告");
        assert!(artifacts_root.join(b.0.to_string()).join("x.bin").exists());
        let _ = dir;
    }

    /// AC：手动删构建（REST DELETE）复用同一裁剪——无论新旧立即全删日志 +
    /// 产物 + 回收空目录；构建记录保留（ADR-0013 语义）。
    #[tokio::test]
    async fn delete_build_data_purges_whole_build_keeps_record() {
        let (dir, pool, artifacts_root) = fixture().await;
        let b = create_build(&pool, 1, "release", 1).await;
        insert_log(&pool, b.0, b.1, NOW - DAY_MS).await;
        insert_artifact(&pool, &artifacts_root, b.0, "fresh.bin", NOW - DAY_MS).await;

        let report = delete_build_data(&pool, &artifacts_root, b.0)
            .await
            .expect("手动删");
        assert_eq!(report.logs_deleted, 1);
        assert_eq!(report.artifact_meta_deleted, 1);
        assert_eq!(report.artifact_files_deleted, 1);
        assert_eq!(report.empty_dirs_reclaimed, 1);
        assert!(!artifacts_root.join(b.0.to_string()).exists(), "目录回收");
        assert_eq!(build_count(&pool).await, 1, "构建记录保留");
        let _ = dir;
    }

    /// AC：手动删构建幂等——不存在的构建返回空报告（不炸）。
    #[tokio::test]
    async fn delete_build_data_on_missing_build_is_noop() {
        let (dir, pool, artifacts_root) = fixture().await;
        let report = delete_build_data(&pool, &artifacts_root, 9999)
            .await
            .expect("幂等");
        assert_eq!(report, CleanupReport::default());
        let _ = dir;
    }
}
