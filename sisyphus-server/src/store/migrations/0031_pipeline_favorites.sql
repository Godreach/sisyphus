-- 0031 用户级流水线收藏（票 #137）。
-- 主键同时给出用户隔离与重复收藏幂等所需的唯一约束。
CREATE TABLE pipeline_favorites (
    user_id       INTEGER NOT NULL REFERENCES users(id),
    project_id    INTEGER NOT NULL,
    pipeline_name TEXT NOT NULL,
    added_at      INTEGER NOT NULL,
    PRIMARY KEY (user_id, project_id, pipeline_name),
    FOREIGN KEY (project_id, pipeline_name) REFERENCES pipelines(project_id, name) ON DELETE CASCADE
);

CREATE INDEX idx_pipeline_favorites_user_added
    ON pipeline_favorites(user_id, added_at DESC);
