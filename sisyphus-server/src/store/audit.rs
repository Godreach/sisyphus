//! 审计日志 repo（票 B2b-T7，ADR-0015）：安全事件的记账与回放。
//!
//! **只增契约**：本 repo 只提供 [`AuditRepo::insert`]（记账）与
//! [`AuditRepo::query`]（回放）两个方法——没有任何 UPDATE/DELETE 途径，
//! 这就是防改写契约的兑现面（v1 不做防篡改哈希链，ADR-0015：单机 SQLite
//! 无独立可信存储，能改审计表的人也能重算链）。新消费面只能新增事件来源
//! （调 `insert`），不能改历史。
//!
//! 事件类型取值域由 [`AuditEvent`] 单点收敛（本票 B2b 面清单；Agent 批次、
//! 项目删、全局配置变更等来源随各自批次补进枚举）。`actor` 为操作人实名
//! （认证用户名）；`project_name` 可空（项目域事件记项目**名**不记引用——
//! 项目行可能随未来批次删除，审计回放不悬空）；`detail` 为 JSON 文本
//! （机密事件只记名+操作人+时间，永不记值——值形态在本模块不存在）。

use sqlx::SqlitePool;

use super::StoreError;

/// 审计事件类型（ADR-0015 清单 ∩ B2b 面；落库文本 `as_str()` 为契约值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEvent {
    /// 登录成功。
    LoginSuccess,
    /// 登录失败（用户不存在与密码错误同记——不区分，与 401 响应形态一致）。
    LoginFailure,
    /// 登出（cookie 会话删除；Bearer 面无会话可结束，不记）。
    Logout,
    /// 用户建立（全局 admin 建号 / 自注册 / setup wizard 首个 admin）。
    UserCreated,
    /// 禁用用户（同秒删其全部 session 与 PAT，踢线）。
    UserDisabled,
    /// 启用用户。
    UserEnabled,
    /// 管理员代办重置密码。
    PasswordReset,
    /// 创建 PAT（detail 记令牌名）。
    PatCreated,
    /// 吊销 PAT（detail 记令牌名）。
    PatRevoked,
    /// 创建项目。
    ProjectCreated,
    /// 成员角色变更（整组分配；detail 记落定清单）。
    MemberRolesChanged,
    /// 机密建立（detail 只记名）。
    SecretCreated,
    /// 机密覆写（detail 只记名）。
    SecretOverwritten,
    /// 机密删除（detail 只记名）。
    SecretDeleted,
}

impl AuditEvent {
    /// 全部事件类型（契约单点：`as_str` 映射 + [`Self::parse`] 识别 +
    /// OpenAPI enum 生成的共同来源——新增事件类型只改这里与 [`Self::as_str`]）。
    pub const ALL: [AuditEvent; 14] = [
        Self::LoginSuccess,
        Self::LoginFailure,
        Self::Logout,
        Self::UserCreated,
        Self::UserDisabled,
        Self::UserEnabled,
        Self::PasswordReset,
        Self::PatCreated,
        Self::PatRevoked,
        Self::ProjectCreated,
        Self::MemberRolesChanged,
        Self::SecretCreated,
        Self::SecretOverwritten,
        Self::SecretDeleted,
    ];

    /// 落库 / 查询过滤文本（契约值；filter 参数按此匹配）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LoginSuccess => "login_success",
            Self::LoginFailure => "login_failure",
            Self::Logout => "logout",
            Self::UserCreated => "user_created",
            Self::UserDisabled => "user_disabled",
            Self::UserEnabled => "user_enabled",
            Self::PasswordReset => "password_reset",
            Self::PatCreated => "pat_created",
            Self::PatRevoked => "pat_revoked",
            Self::ProjectCreated => "project_created",
            Self::MemberRolesChanged => "member_roles_changed",
            Self::SecretCreated => "secret_created",
            Self::SecretOverwritten => "secret_overwritten",
            Self::SecretDeleted => "secret_deleted",
        }
    }

    /// 从契约文本解析（审计端点的 `event` 过滤参数校验用）；未知值
    /// `None`（调用侧 422，不落任何查询）。
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == s)
    }
}

/// 审计行（repo 查询返回；`detail` 为 JSON 文本，API 层解析）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// 行 id。
    pub id: i64,
    /// 事件时间（Unix 毫秒）。
    pub ts: i64,
    /// 操作人（用户名）。
    pub actor: String,
    /// 事件类型（[`AuditEvent::as_str`] 契约值）。
    pub event: String,
    /// 项目名（可空：非项目域事件）。
    pub project_name: Option<String>,
    /// JSON 文本（可空：机密名 / 目标用户 / 成员清单等）。
    pub detail: Option<String>,
}

/// 查询过滤条件（全部可选：仅提供者参与过滤，AND 组合）。
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    /// 时间下限（含，Unix 毫秒）。
    pub since: Option<i64>,
    /// 时间上限（含，Unix 毫秒）。
    pub until: Option<i64>,
    /// 操作人精确匹配。
    pub user: Option<String>,
    /// 项目名精确匹配。
    pub project: Option<String>,
    /// 事件类型（[`AuditEvent::as_str`] 契约值）。
    pub event: Option<String>,
}

/// 审计 repo：只增（[`Self::insert`]）+ 回放（[`Self::query`]）。
#[derive(Debug, Clone)]
pub struct AuditRepo {
    pool: SqlitePool,
}

impl AuditRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 记一笔审计（唯一写入途径）：时间由调用侧传入（与业务写入同一取值，
    /// 假时钟可驱动过滤测试）。返回落定后的行（id 可回读）。
    pub async fn insert(
        &self,
        ts: i64,
        actor: &str,
        event: AuditEvent,
        project_name: Option<&str>,
        detail: Option<&str>,
    ) -> Result<AuditEntry, StoreError> {
        let result = sqlx::query(
            "INSERT INTO audit_log (ts, actor, event_type, project_name, detail)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(ts)
        .bind(actor)
        .bind(event.as_str())
        .bind(project_name)
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(AuditEntry {
            id: result.last_insert_rowid(),
            ts,
            actor: actor.to_string(),
            event: event.as_str().to_string(),
            project_name: project_name.map(ToOwned::to_owned),
            detail: detail.map(ToOwned::to_owned),
        })
    }

    /// 回放：按条件过滤（AND 组合，全部可选）+ 分页，时间倒序（新事件在
    /// 前；同毫秒按 id 倒序——审计页「最新在前」的稳定排序）。
    ///
    /// 空过滤以 SQL 侧 `? IS NULL OR …` 哨兵短路：固定形态查询、逐条件
    /// 值绑定两次（同值），免动态拼 SQL 的类型体操——缺省条件以 NULL 绑定
    /// 即整条件跳过。
    pub async fn query(
        &self,
        filter: &AuditQuery,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AuditEntry>, StoreError> {
        let rows = sqlx::query_as::<_, (i64, i64, String, String, Option<String>, Option<String>)>(
            "SELECT id, ts, actor, event_type, project_name, detail FROM audit_log
             WHERE (? IS NULL OR ts >= ?)
               AND (? IS NULL OR ts <= ?)
               AND (? IS NULL OR actor = ?)
               AND (? IS NULL OR project_name = ?)
               AND (? IS NULL OR event_type = ?)
             ORDER BY ts DESC, id DESC LIMIT ? OFFSET ?",
        )
        .bind(filter.since)
        .bind(filter.since)
        .bind(filter.until)
        .bind(filter.until)
        .bind(filter.user.as_deref())
        .bind(filter.user.as_deref())
        .bind(filter.project.as_deref())
        .bind(filter.project.as_deref())
        .bind(filter.event.as_deref())
        .bind(filter.event.as_deref())
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, ts, actor, event, project_name, detail)| AuditEntry {
                id,
                ts,
                actor,
                event,
                project_name,
                detail,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录 + 已迁移库（store 缝测试形态，沿用 members/secrets）。
    async fn migrated_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().expect("临时目录");
        crate::config::Config::load(
            dir.path().to_path_buf(),
            crate::config::Overrides::default(),
            crate::config::Overrides::default(),
        )
        .expect("目录布局");
        let pool = super::super::bootstrap(dir.path())
            .await
            .expect("bootstrap");
        (dir, pool)
    }

    #[tokio::test]
    async fn insert_round_trips_all_fields_and_detail_is_json_text() {
        let (_dir, pool) = migrated_pool().await;
        let repo = AuditRepo::new(pool.clone());

        // 项目域事件：detail 为 JSON 文本（机密只记名）。
        let row = repo
            .insert(
                1_000,
                "carol",
                AuditEvent::SecretCreated,
                Some("demo"),
                Some(r#"{"secret":"DEPLOY_KEY"}"#),
            )
            .await
            .expect("记审计");
        assert!(row.id > 0);
        assert_eq!(row.ts, 1_000);
        assert_eq!(row.actor, "carol");
        assert_eq!(row.event, "secret_created");
        assert_eq!(row.project_name.as_deref(), Some("demo"));
        assert_eq!(row.detail.as_deref(), Some(r#"{"secret":"DEPLOY_KEY"}"#));

        // 非项目域事件：project_name / detail 可空。
        let row = repo
            .insert(1_001, "alice", AuditEvent::LoginSuccess, None, None)
            .await
            .expect("记审计");
        assert_eq!(row.project_name, None);
        assert_eq!(row.detail, None);

        // store 缝直查临时库：落库形态与返回行一致（JSON 文本原样）。
        let (ts, actor, event_type, project_name, detail): (i64, String, String, Option<String>, Option<String>) =
            sqlx::query_as("SELECT ts, actor, event_type, project_name, detail FROM audit_log WHERE id = ?")
                .bind(row.id)
                .fetch_one(&pool)
                .await
                .expect("直查");
        assert_eq!(ts, 1_001);
        assert_eq!(actor, "alice");
        assert_eq!(event_type, "login_success");
        assert_eq!(project_name, None);
        assert_eq!(detail, None);
    }

    #[tokio::test]
    async fn query_filters_by_event_actor_project_and_time_range() {
        let (_dir, pool) = migrated_pool().await;
        let repo = AuditRepo::new(pool.clone());

        for (ts, actor, event, project) in [
            (100, "alice", AuditEvent::LoginSuccess, None),
            (200, "alice", AuditEvent::LoginFailure, None),
            (300, "bob", AuditEvent::LoginSuccess, None),
            (400, "carol", AuditEvent::ProjectCreated, Some("demo")),
            (500, "carol", AuditEvent::SecretDeleted, Some("demo")),
            (600, "alice", AuditEvent::ProjectCreated, Some("other")),
        ] {
            repo.insert(ts, actor, event, project, None)
                .await
                .expect("记审计");
        }

        let all = repo.query(&AuditQuery::default(), 100, 0).await.expect("全量");
        assert_eq!(all.len(), 6, "无过滤返回全部");
        // 时间倒序（新在前）。
        assert_eq!(all[0].ts, 600);
        assert_eq!(all[5].ts, 100);

        // 按事件类型过滤。
        let q = AuditQuery {
            event: Some(AuditEvent::ProjectCreated.as_str().into()),
            ..Default::default()
        };
        let rows = repo.query(&q, 100, 0).await.expect("按事件");
        assert_eq!(rows.len(), 2, "project_created 两笔");

        // 按操作人过滤。
        let q = AuditQuery {
            user: Some("alice".into()),
            ..Default::default()
        };
        let rows = repo.query(&q, 100, 0).await.expect("按用户");
        assert_eq!(rows.len(), 3, "alice 三笔");

        // 按项目过滤。
        let q = AuditQuery {
            project: Some("demo".into()),
            ..Default::default()
        };
        let rows = repo.query(&q, 100, 0).await.expect("按项目");
        assert_eq!(rows.len(), 2, "demo 两笔");

        // 组合过滤：项目 demo + 事件 secret_deleted。
        let q = AuditQuery {
            project: Some("demo".into()),
            event: Some(AuditEvent::SecretDeleted.as_str().into()),
            ..Default::default()
        };
        let rows = repo.query(&q, 100, 0).await.expect("组合");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].actor, "carol");

        // 时间范围（含边界）。
        let q = AuditQuery {
            since: Some(200),
            until: Some(500),
            ..Default::default()
        };
        let rows = repo.query(&q, 100, 0).await.expect("按时间");
        assert_eq!(rows.len(), 4, "[200, 500] 四笔");

        // 时间 + 用户组合。
        let q = AuditQuery {
            since: Some(300),
            user: Some("alice".into()),
            ..Default::default()
        };
        let rows = repo.query(&q, 100, 0).await.expect("时间+用户");
        assert_eq!(rows.len(), 1, "alice 在 300 及以后仅一笔（600 是 other 项目那笔）");
        assert_eq!(rows[0].project_name.as_deref(), Some("other"));
    }

    #[tokio::test]
    async fn query_paginates_newest_first_with_limit_and_offset() {
        let (_dir, pool) = migrated_pool().await;
        let repo = AuditRepo::new(pool.clone());

        // 同毫秒插入（id 倒序是第二排序键：后插者在前）。
        for i in 0..5 {
            repo.insert(1_000, &format!("u{i}"), AuditEvent::LoginSuccess, None, None)
                .await
                .expect("记审计");
        }

        // 第一页：limit=2 → 最新两笔（同毫秒按 id 倒序：u4, u3）。
        let page = repo.query(&AuditQuery::default(), 2, 0).await.expect("页 1");
        assert_eq!(
            page.iter().map(|e| e.actor.as_str()).collect::<Vec<_>>(),
            ["u4", "u3"]
        );

        // 第二页：offset=2 → u2, u1。
        let page = repo.query(&AuditQuery::default(), 2, 2).await.expect("页 2");
        assert_eq!(
            page.iter().map(|e| e.actor.as_str()).collect::<Vec<_>>(),
            ["u2", "u1"]
        );

        // 越界 offset：空页（不报错）。
        let page = repo.query(&AuditQuery::default(), 2, 10).await.expect("越界");
        assert!(page.is_empty());
    }

    /// 票 B2b-T7 AC：audit 只增——repo 层无改写途径即为契约。可观察断言：
    /// 相同的记账内容重复插入产生两条独立行（无去重/覆写），回读与原值
    /// 一致（读不改写），行数与插入笔数恒等（读路径零副作用）。
    #[tokio::test]
    async fn append_only_contract_no_rewrite_path_through_repo() {
        let (_dir, pool) = migrated_pool().await;
        let repo = AuditRepo::new(pool.clone());

        // 同一内容插两次：两行（append-only，无 upsert 语义）。
        repo.insert(1_000, "alice", AuditEvent::LoginSuccess, None, None)
            .await
            .expect("第一笔");
        repo.insert(1_000, "alice", AuditEvent::LoginSuccess, None, None)
            .await
            .expect("第二笔");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("计数");
        assert_eq!(count, 2, "append：两次插入即两行");

        // 回读与插入等价（查询不改写）。
        let rows = repo.query(&AuditQuery::default(), 100, 0).await.expect("回读");
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.ts, 1_000);
            assert_eq!(row.actor, "alice");
            assert_eq!(row.event, "login_success");
        }
        // 回读后行数与内容不变（读零副作用）。
        let count_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("再计数");
        assert_eq!(count_after, 2);
    }

    #[test]
    fn event_contract_values_round_trip_parse() {
        for event in [
            AuditEvent::LoginSuccess,
            AuditEvent::LoginFailure,
            AuditEvent::Logout,
            AuditEvent::UserCreated,
            AuditEvent::UserDisabled,
            AuditEvent::UserEnabled,
            AuditEvent::PasswordReset,
            AuditEvent::PatCreated,
            AuditEvent::PatRevoked,
            AuditEvent::ProjectCreated,
            AuditEvent::MemberRolesChanged,
            AuditEvent::SecretCreated,
            AuditEvent::SecretOverwritten,
            AuditEvent::SecretDeleted,
        ] {
            assert_eq!(AuditEvent::parse(event.as_str()), Some(event));
        }
        for unknown in ["", "build_started", "secret_value_read", "login"] {
            assert_eq!(AuditEvent::parse(unknown), None, "{unknown:?} 应不识别");
        }
    }
}
