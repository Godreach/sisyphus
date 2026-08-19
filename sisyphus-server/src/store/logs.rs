//! 日志 chunk 仓储（票 #73 / B5-T1，ADR-0013/0004）：`logs` 表 + gzip chunk。
//!
//! - **append 幂等**：UNIQUE(job_id, attempt, start_seq) + `ON CONFLICT DO
//!   NOTHING`——断线补传（Agent 重放天然从文件头重发未清空段）重复
//!   start_seq 忽略，不重不乱序（ADR-0013）。落库列（end_seq/step/stream）
//!   由 [`crate::logs::chunk_meta`] 自 chunk 内容派生（查询面冗余列）。
//! - **read_from 范围读取**：返回「覆盖或晚于 `from_seq`」的 chunk（
//!   `start_seq >= from` 或 `end_seq >= from`——多事件 chunk 跨游标时整块
//!   返回，事件级过滤归调用侧），按 start_seq 升序；每块独立 gzip、解压
//!   互不依赖（ADR-0013）。SSE `from=<seq>` 回放/续传与整份下载共用。
//! - **读走独立连接**（ADR-0004）：读路径持自有小连接池（同库文件、同
//!   PRAGMA 基线），不与 gRPC 写路径（共享池）争连接——WAL 下读写并发，
//!   长尾随读不挤占元数据/写面。
//!
//! chunk 内部编码见 [`crate::logs`]（JSONL 事件流 + gzip）。

use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;

use super::StoreError;
use super::traits::{LogChunk, LogLocation, LogStore};
use crate::logs;

/// 读路径独立连接数（并发的 SSE 尾随/下载读者互不排队即可）。
const READ_POOL_MAX: u32 = 2;

/// SQLite 日志存储（`LogStore` 的生产实现）。
#[derive(Debug, Clone)]
pub struct SqliteLogStore {
    /// 写路径（grpc 落库）：共享组合根池。
    write: SqlitePool,
    /// 读路径（SSE 回放/尾随、整份下载）：独立小池（ADR-0004）。
    read: SqlitePool,
}

impl SqliteLogStore {
    /// 从既有池装配：共享池作写路径，另开独立小池（同连接选项——同库
    /// 文件、同 PRAGMA 基线）作读路径。库已由 [`super::bootstrap`] 迁移。
    pub async fn open(pool: &SqlitePool) -> Result<Self, StoreError> {
        let read = SqlitePoolOptions::new()
            .max_connections(READ_POOL_MAX)
            .connect_with((*pool.connect_options()).clone())
            .await?;
        Ok(Self {
            write: pool.clone(),
            read,
        })
    }
}

impl LogStore for SqliteLogStore {
    async fn append(&self, loc: LogLocation, chunks: Vec<LogChunk>) -> Result<(), StoreError> {
        if chunks.is_empty() {
            return Ok(());
        }
        let now = crate::store::now_ms();
        let mut tx = self.write.begin().await?;
        for chunk in chunks {
            // 元数据列自 chunk 内容派生（end_seq/step/stream，查询面冗余）；
            // 损坏 chunk（非本 codec 产物）记日志跳过——单块异常不炸整批。
            let events = match logs::decode_chunk(&chunk) {
                Ok(events) => events,
                Err(e) => {
                    tracing::warn!(
                        job_id = loc.job_id,
                        attempt = loc.attempt,
                        start_seq = chunk.start_seq,
                        error = %e,
                        "日志 chunk 损坏，跳过落库"
                    );
                    continue;
                }
            };
            let meta = logs::chunk_meta(&events);
            let result = sqlx::query(
                "INSERT INTO logs (build_id, job_id, attempt, start_seq, end_seq, step, stream, data, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (job_id, attempt, start_seq) DO NOTHING",
            )
            .bind(loc.build_id)
            .bind(loc.job_id)
            .bind(loc.attempt)
            .bind(chunk.start_seq as i64)
            .bind(meta.end_seq as i64)
            .bind(meta.step)
            .bind(meta.stream)
            .bind(&chunk.compressed)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            let _ = result;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn read_from(
        &self,
        loc: LogLocation,
        from_seq: u64,
    ) -> Result<Vec<LogChunk>, StoreError> {
        let rows = sqlx::query_as::<_, (i64, Vec<u8>)>(
            "SELECT start_seq, data FROM logs
             WHERE build_id = ? AND job_id = ? AND attempt = ?
               AND (start_seq >= ? OR end_seq >= ?)
             ORDER BY start_seq",
        )
        .bind(loc.build_id)
        .bind(loc.job_id)
        .bind(loc.attempt)
        .bind(from_seq as i64)
        .bind(from_seq as i64)
        .fetch_all(&self.read)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(start_seq, compressed)| LogChunk {
                start_seq: start_seq as u64,
                compressed,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::{LogStream, LogStreamEvent};

    /// 临时库装配：bootstrap（迁移含 0011 logs 表）+ SqliteLogStore。
    async fn fixture() -> (tempfile::TempDir, SqliteLogStore) {
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
        // 外键参照完整性：建父行（项目→pipeline 无需行、builds 只需 project）。
        sqlx::query("INSERT INTO projects (name, scm_type, scm_url, created_at, updated_at) VALUES ('demo', 'git', 'https://example.com/r', 0, 0)")
            .execute(&pool)
            .await
            .expect("建项目");
        sqlx::query("INSERT INTO builds (project_id, pipeline_name, number, status, trigger, trigger_detail, attempt, snapshot, updated_at) VALUES (1, 'release', 1, 'running', 'manual', '{}', 1, '{}', 0)")
            .execute(&pool)
            .await
            .expect("建构建");
        sqlx::query("INSERT INTO jobs (build_id, stage_index, name, status, attempt, labels, timeout_minutes, retry_count, allow_failure) VALUES (1, 0, 'build', 'running', 1, '[]', 0, 0, 0)")
            .execute(&pool)
            .await
            .expect("建任务");
        let store = SqliteLogStore::open(&pool).await.expect("开日志存储");
        (dir, store)
    }

    fn chunk(start_seq: u64, texts: &[&str]) -> LogChunk {
        let events: Vec<LogStreamEvent> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| LogStreamEvent::Output {
                seq: start_seq + i as u64,
                stream: LogStream::Stdout,
                text: (*t).into(),
            })
            .collect();
        crate::logs::encode_chunk(&events)
    }

    fn loc() -> LogLocation {
        LogLocation {
            build_id: 1,
            job_id: 1,
            attempt: 1,
        }
    }

    #[tokio::test]
    async fn append_read_roundtrips_gzip_chunks() {
        let (_dir, store) = fixture().await;
        store
            .append(
                loc(),
                vec![chunk(0, &["hello\n", "world\n"]), chunk(2, &["end\n"])],
            )
            .await
            .expect("落库");

        let all = store.read_from(loc(), 0).await.expect("读回");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].start_seq, 0);
        assert_eq!(all[1].start_seq, 2);
        let events = crate::logs::decode_chunk(&all[0]).expect("解码");
        assert_eq!(
            events,
            vec![
                LogStreamEvent::Output {
                    seq: 0,
                    stream: LogStream::Stdout,
                    text: "hello\n".into()
                },
                LogStreamEvent::Output {
                    seq: 1,
                    stream: LogStream::Stdout,
                    text: "world\n".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn append_is_idempotent_by_start_seq() {
        let (_dir, store) = fixture().await;
        // 首传 + 断线补传重放同一批：不重不乱序。
        store
            .append(loc(), vec![chunk(0, &["a", "b"])])
            .await
            .expect("首传");
        store
            .append(loc(), vec![chunk(0, &["a", "b"])])
            .await
            .expect("重放");
        store
            .append(loc(), vec![chunk(2, &["c"])])
            .await
            .expect("续传");

        let all = store.read_from(loc(), 0).await.expect("读回");
        assert_eq!(all.len(), 2, "重复 start_seq 去重（2 chunk）");
        let events: Vec<LogStreamEvent> = all
            .iter()
            .flat_map(|c| crate::logs::decode_chunk(c).expect("解码"))
            .collect();
        assert_eq!(events.len(), 3, "三个事件，无重复");
        assert_eq!(events[2].seq(), 2);
    }

    #[tokio::test]
    async fn read_from_filters_and_orders_across_attempts() {
        let (_dir, store) = fixture().await;
        // 乱序到达（先 seq 高后 seq 低）：读取按 start_seq 升序。
        store
            .append(loc(), vec![chunk(4, &["e"])])
            .await
            .expect("高 seq 先到");
        store
            .append(
                loc(),
                vec![chunk(0, &["a"]), chunk(1, &["b"]), chunk(2, &["c"])],
            )
            .await
            .expect("低 seq 后到");

        let from_two = store.read_from(loc(), 2).await.expect("续读");
        assert_eq!(
            from_two.iter().map(|c| c.start_seq).collect::<Vec<_>>(),
            vec![2, 4],
            "from=2 起含 2、按 seq 升序"
        );

        // attempt 隔离：attempt=2 从头计 seq，互不污染。
        let loc2 = LogLocation {
            attempt: 2,
            ..loc()
        };
        store
            .append(loc2, vec![chunk(0, &["x"])])
            .await
            .expect("attempt 2");
        assert_eq!(store.read_from(loc2, 0).await.unwrap().len(), 1);
        assert_eq!(store.read_from(loc(), 0).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn read_from_includes_chunk_straddling_cursor() {
        let (_dir, store) = fixture().await;
        // 多事件单 chunk（proto 契约允许一帧多事件）：覆盖 seq 0..=2。
        store
            .append(loc(), vec![chunk(0, &["a", "b", "c"])])
            .await
            .expect("多事件 chunk");
        // from=2：chunk 覆盖 2（start_seq=0 < 2）——须整块返回，事件级过滤
        // 归调用侧（丢尾 = 跨游标续传丢日志）。
        let got = store.read_from(loc(), 2).await.expect("续读");
        assert_eq!(got.len(), 1, "跨游标 chunk 须返回");
        let events = crate::logs::decode_chunk(&got[0]).expect("解码");
        let seqs: Vec<u64> = events.iter().map(|e| e.seq()).collect();
        assert_eq!(seqs, vec![0, 1, 2], "事件级过滤归调用侧");

        // 完全在游标之前的 chunk 不返回。
        let got = store.read_from(loc(), 3).await.expect("越尾读");
        assert!(got.is_empty(), "end_seq < from 不返回");
    }

    #[tokio::test]
    async fn append_empty_is_noop() {
        let (_dir, store) = fixture().await;
        store.append(loc(), vec![]).await.expect("空批");
        assert!(store.read_from(loc(), 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rows_carry_derived_metadata_columns() {
        let (_dir, store) = fixture().await;
        let events = vec![
            LogStreamEvent::StepStart {
                seq: 0,
                step: 2,
                name: String::new(),
                command: "make".into(),
                started_at: 10,
            },
            LogStreamEvent::Output {
                seq: 1,
                stream: LogStream::Stderr,
                text: "err".into(),
            },
        ];
        store
            .append(loc(), vec![crate::logs::encode_chunk(&events)])
            .await
            .expect("落库");
        let row: (i64, i64, i32, String) =
            sqlx::query_as("SELECT start_seq, end_seq, step, stream FROM logs WHERE job_id = 1")
                .fetch_one(&store.read)
                .await
                .expect("读行");
        assert_eq!(row, (0, 1, 2, "stderr".to_string()));
    }
}
