CREATE TABLE local_schema_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    version INTEGER NOT NULL CHECK (version > 0)
) STRICT;

CREATE TABLE local_projection_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    applied_repo_seq INTEGER NOT NULL CHECK (applied_repo_seq >= 0)
) STRICT;

CREATE TABLE local_file_heads (
    item_id TEXT PRIMARY KEY,
    item_seq INTEGER NOT NULL CHECK (item_seq > 0),
    blob_id TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0)
) STRICT, WITHOUT ROWID;

CREATE TABLE local_worktree_identities (
    item_id TEXT PRIMARY KEY,
    node_kind TEXT NOT NULL CHECK (node_kind IN ('FILE', 'FOLDER')),
    device_id BLOB NOT NULL CHECK (length(device_id) = 8),
    file_id BLOB NOT NULL CHECK (length(file_id) = 8),
    birth_seconds INTEGER,
    birth_nanos INTEGER,
    path TEXT NOT NULL CHECK (path <> ''),
    CHECK (
        (birth_seconds IS NULL AND birth_nanos IS NULL)
        OR (birth_seconds IS NOT NULL
            AND birth_nanos IS NOT NULL
            AND birth_nanos BETWEEN 0 AND 999999999)
    ),
    UNIQUE (path)
) STRICT, WITHOUT ROWID;

CREATE TABLE local_worktree_entries (
    path TEXT PRIMARY KEY CHECK (path <> ''),
    node_kind TEXT NOT NULL CHECK (node_kind IN ('FILE', 'FOLDER')),
    device_id BLOB NOT NULL CHECK (length(device_id) = 8),
    file_id BLOB NOT NULL CHECK (length(file_id) = 8),
    birth_seconds INTEGER,
    birth_nanos INTEGER,
    size_bytes INTEGER,
    mtime_seconds INTEGER,
    mtime_nanos INTEGER,
    change_seconds INTEGER,
    change_nanos INTEGER,
    plain_sha256 TEXT,
    stat_valid INTEGER,
    CHECK (
        (birth_seconds IS NULL AND birth_nanos IS NULL)
        OR (birth_seconds IS NOT NULL
            AND birth_nanos IS NOT NULL
            AND birth_nanos BETWEEN 0 AND 999999999)
    ),
    CHECK (
        (node_kind = 'FILE'
            AND size_bytes IS NOT NULL AND size_bytes >= 0
            AND mtime_seconds IS NOT NULL
            AND mtime_nanos IS NOT NULL
            AND mtime_nanos BETWEEN 0 AND 999999999
            AND plain_sha256 IS NOT NULL
            AND length(plain_sha256) = 64
            AND plain_sha256 NOT GLOB '*[^0-9a-f]*'
            AND stat_valid IS NOT NULL AND stat_valid IN (0, 1)
            AND ((change_seconds IS NULL AND change_nanos IS NULL)
                OR (change_seconds IS NOT NULL
                    AND change_nanos IS NOT NULL
                    AND change_nanos BETWEEN 0 AND 999999999))
            AND (change_seconds IS NOT NULL OR stat_valid = 0))
        OR
        (node_kind = 'FOLDER'
            AND size_bytes IS NULL
            AND mtime_seconds IS NULL AND mtime_nanos IS NULL
            AND change_seconds IS NULL AND change_nanos IS NULL
            AND plain_sha256 IS NULL AND stat_valid IS NULL)
    )
) STRICT, WITHOUT ROWID;

CREATE TABLE local_worktree_scan_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    ignore_sha256 TEXT CHECK (
        ignore_sha256 IS NULL
        OR (length(ignore_sha256) = 64
            AND ignore_sha256 NOT GLOB '*[^0-9a-f]*')
    ),
    monitor_instance_id TEXT,
    monitor_sequence INTEGER,
    CHECK (
        (monitor_instance_id IS NULL AND monitor_sequence IS NULL)
        OR (monitor_instance_id IS NOT NULL
            AND length(monitor_instance_id) = 36
            AND monitor_sequence IS NOT NULL
            AND monitor_sequence >= 0)
    )
) STRICT;

INSERT INTO local_schema_meta(singleton, version) VALUES (1, 15);
