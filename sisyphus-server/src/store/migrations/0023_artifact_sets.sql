-- Directory artifact sets retain every attempt; files use private artifact names
-- until the complete manifest has been verified and published.
CREATE TABLE artifact_sets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    build_id INTEGER NOT NULL REFERENCES builds(id) ON DELETE CASCADE,
    job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    attempt INTEGER NOT NULL,
    name TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'ready')),
    created_at INTEGER NOT NULL,
    UNIQUE (job_id, attempt, name)
);
CREATE INDEX idx_artifact_sets_build ON artifact_sets(build_id, state);

CREATE TABLE artifact_set_entries (
    set_id INTEGER NOT NULL REFERENCES artifact_sets(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    executable INTEGER NOT NULL CHECK (executable IN (0, 1)),
    artifact_name TEXT UNIQUE,
    PRIMARY KEY (set_id, path)
);
