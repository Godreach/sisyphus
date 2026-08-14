# 0004 - v1 存储用 SQLite（WAL）+ sqlx，产物存本地磁盘

日期：2026-08-15
状态：已接受

## 背景

Server 端存储选型受产品第一承诺约束：**单二进制、零依赖、开箱即用**。调研（`research/server-storage` 分支，issue #3）对比了 SQLite/PostgreSQL/libsql/DuckDB 引擎与 sqlx/SeaORM/Diesel 数据库，核实了 Woodpecker、Drone(Gitness)、Gitea Actions 三个竞品的实际做法。

## 决策

- **数据库**：SQLite（WAL 模式），sqlx（bundled SQLite，静态链接）。PRAGMA 基线：`journal_mode=WAL; synchronous=NORMAL; busy_timeout=5000; foreign_keys=ON`。
- **数据访问**：sqlx（异步、编译期 SQL 校验、内置迁移）。不用 sqlx `any` 驱动做双后端（官方强警告 + AnyKind 废弃）。
- **构建日志**：直接进 SQLite，Agent 端缓冲批量合并写（约 200ms/64KB 触发），每 step 日志单 blob 追加；读走独立连接。
- **产物**：本地磁盘目录 `data/artifacts/<build_id>/<name>`，元数据（路径/大小/校验和/retention）进库，下载走 server HTTP 流式响应。
- **代码分层**：从 v1 起就用 trait 隔离（`LogStore` / `ArtifactStore` / 元数据 repo 层）。

## 理由

- 与 Woodpecker"默认 SQLite 零安装零配置、可换 Postgres"同构，是行业默认答案；DuckDB（OLAP 定位）、libsql（重心转向 Turso beta）、PostgreSQL（外部依赖）均不适配 v1。
- WAL 单写者模式下，"Agent 批量日志写 + 心跳/状态小事务"的负载（~100 agent、日千级构建）完全在舒适区。
- 产物是大文件，天然不属于关系库；Woodpecker/Gitea 默认同样是本地盘。

## 后果

- **日志保留清理是 v1 必做项**（默认 90 天 + 定期 DELETE + `PRAGMA wal_checkpoint(TRUNCATE)`），否则单文件数据库无限膨胀（Woodpecker 官方警示）。
- **sqlx bundled SQLite 需目标平台 C 编译器**：主流平台无碍，ARM musl 交叉编译是发布矩阵的验证项（转入发布形态票 #16）。
- **部署文档必须警告**：WAL 不支持网络文件系统，数据目录必须本地盘；备份需连 `-wal`/`-shm` 一起或走 backup API。
- **v2 升级路径**：元数据先支持 PostgreSQL（两套 SQL/迁移，sqlx 同套 API，Gitness 同做法）；产物/日志经 `object_store` crate（Apache Arrow 治理，LocalFileSystem/S3/MinIO 统一 trait，运行时配置切换）；产物键用正斜杠无盘符风格，为迁移留缝。rust-s3（维护间歇）排除。
- **避免长读事务**（防 checkpoint 饥饿、WAL 无限增长）：实时日志查看按 step blob + 偏移量拉取。
