# 存储备份、恢复与一致性检查

Sisyphus 的 SQLite 目录和对象存储 bucket 是同一份制品/日志目录的两部分，
必须成对备份。备份至少包括数据目录中的 `sisyphus.db`（使用 SQLite 在线
backup/VACUUM 语义，不要只复制运行中的 `-wal`）、`master.key`、旧本地产物
目录 `artifacts/`、日志归档目录，以及仍持有未确认正文的 Agent 日志缓冲目录。
配置文件中的凭据材料和自托管 S3 bucket 也应按部署方的密钥策略单独备份。

恢复顺序：先停止 Server，恢复 SQLite、`master.key`、本地对象和同一 endpoint /
bucket / prefix 的对象存储；启动后先让全局管理员调用
`GET /api/v1/storage/consistency`。该检查只读列出 ready 元数据缺对象、大小或
SHA-256 异常、bucket 中未登记对象，以及上传、multipart、删除和日志归档积压。
只有显式 `?deep_hash=true` 才会读取全部正文计算哈希，检查本身不会自动修复或
删除对象。确认结果后再恢复 Agent 和流水线调度。

## 自托管 S3 约束

- endpoint 必须同时可由 Server、需要直传的 Agent 和下载浏览器访问；首版不支持
  内外双 endpoint。Server 不自动创建 bucket，也不自动安装或管理对象存储。
- bucket 生命周期规则不得删除 `artifacts/final/` 或 `logs/final/` 下仍处于
  ready 的对象；临时对象和未完成 multipart 可按部署策略设较短生命周期，但应
  保留足够的上传/重试窗口。删除与日志保留由 Server 状态和保留期驱动。
- 变更 endpoint、bucket 或 prefix 前，必须确认 SQLite 中没有旧对象引用并先做
  一致性检查；已有引用会让 Server 启动拒绝不兼容的后端身份，避免把旧对象误读
  到新 bucket。
