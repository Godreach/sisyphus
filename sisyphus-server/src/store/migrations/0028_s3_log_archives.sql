-- 新归档可选 S3；旧行保持 local，稀疏索引仍由 Server 持有。
ALTER TABLE log_archives ADD COLUMN backend TEXT NOT NULL DEFAULT 'local'
    CHECK (backend IN ('local', 's3'));
ALTER TABLE log_archives ADD COLUMN index_json TEXT;
ALTER TABLE log_archives ADD COLUMN temp_path TEXT;
