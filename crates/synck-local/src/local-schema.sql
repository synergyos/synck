CREATE TABLE local_schema_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    version INTEGER NOT NULL CHECK (version > 0)
) STRICT;

CREATE TABLE local_projection_state (
    repo_id TEXT PRIMARY KEY,
    applied_repo_seq INTEGER NOT NULL CHECK (applied_repo_seq >= 0)
) STRICT;

CREATE TABLE local_file_heads (
    repo_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    item_seq INTEGER NOT NULL CHECK (item_seq > 0),
    blob_id TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    PRIMARY KEY (repo_id, item_id)
) STRICT;

CREATE TABLE local_worktree_identities (
    repo_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
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
    PRIMARY KEY (repo_id, item_id),
    UNIQUE (repo_id, path)
) STRICT;

CREATE TABLE local_workspace_sessions (
    repo_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    actor TEXT NOT NULL CHECK (actor <> ''),
    initiator TEXT NOT NULL CHECK (initiator <> ''),
    scope_item_id TEXT NOT NULL,
    scope_path TEXT NOT NULL CHECK (scope_path <> ''),
    baseline_repo_seq INTEGER NOT NULL CHECK (baseline_repo_seq > 0),
    status TEXT NOT NULL CHECK (status IN ('OPEN', 'CONFLICTED', 'COMPLETE')),
    result_commit_id TEXT,
    result_repo_seq INTEGER CHECK (result_repo_seq > 0),
    created_at TEXT NOT NULL,
    completed_at TEXT,
    CHECK (
        (status = 'COMPLETE' AND result_commit_id IS NOT NULL
            AND result_repo_seq IS NOT NULL AND completed_at IS NOT NULL)
        OR
        (status <> 'COMPLETE' AND result_commit_id IS NULL
            AND result_repo_seq IS NULL AND completed_at IS NULL)
    ),
    PRIMARY KEY (repo_id, session_id),
    UNIQUE (repo_id, result_commit_id)
) STRICT;

CREATE TABLE local_workspace_entries (
    repo_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    item_seq INTEGER NOT NULL CHECK (item_seq > 0),
    node_kind TEXT NOT NULL CHECK (node_kind IN ('FILE', 'FOLDER')),
    parent_item_id TEXT NOT NULL,
    name TEXT NOT NULL CHECK (name <> ''),
    path TEXT NOT NULL CHECK (path <> ''),
    observed_path TEXT CHECK (observed_path IS NULL OR observed_path <> ''),
    blob_id TEXT,
    size_bytes INTEGER CHECK (size_bytes >= 0),
    device_id BLOB CHECK (device_id IS NULL OR length(device_id) = 8),
    file_id BLOB CHECK (file_id IS NULL OR length(file_id) = 8),
    birth_seconds INTEGER,
    birth_nanos INTEGER,
    CHECK (
        (node_kind = 'FILE' AND blob_id IS NOT NULL AND size_bytes IS NOT NULL)
        OR
        (node_kind = 'FOLDER' AND blob_id IS NULL AND size_bytes IS NULL)
    ),
    CHECK (
        (observed_path IS NULL
            AND device_id IS NULL
            AND file_id IS NULL
            AND birth_seconds IS NULL
            AND birth_nanos IS NULL)
        OR
        (observed_path IS NOT NULL
            AND device_id IS NOT NULL
            AND file_id IS NOT NULL
            AND (
                (birth_seconds IS NULL AND birth_nanos IS NULL)
                OR (birth_seconds IS NOT NULL
                    AND birth_nanos IS NOT NULL
                    AND birth_nanos BETWEEN 0 AND 999999999)
            ))
    ),
    PRIMARY KEY (repo_id, session_id, item_id),
    UNIQUE (repo_id, session_id, path),
    UNIQUE (repo_id, session_id, observed_path),
    FOREIGN KEY (repo_id, session_id)
        REFERENCES local_workspace_sessions(repo_id, session_id)
        ON DELETE CASCADE
) STRICT;

CREATE TABLE local_object_storage (
    repo_id TEXT PRIMARY KEY,
    mode TEXT NOT NULL CHECK (mode IN ('WHOLE_FILE', 'CHUNKED'))
) STRICT;

CREATE TABLE local_worktree_entries (
    repo_id TEXT NOT NULL,
    path TEXT NOT NULL CHECK (path <> ''),
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
    observed_blob_id TEXT,
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
            AND observed_blob_id IS NOT NULL
            AND length(observed_blob_id) = 71
            AND substr(observed_blob_id, 1, 7) = 'sha256:'
            AND substr(observed_blob_id, 8) NOT GLOB '*[^0-9a-f]*'
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
            AND observed_blob_id IS NULL AND stat_valid IS NULL)
    ),
    PRIMARY KEY (repo_id, path)
) STRICT;

CREATE TABLE local_worktree_scan_state (
    repo_id TEXT PRIMARY KEY,
    ignore_blob_id TEXT CHECK (
        ignore_blob_id IS NULL
        OR (length(ignore_blob_id) = 71
            AND substr(ignore_blob_id, 1, 7) = 'sha256:'
            AND substr(ignore_blob_id, 8) NOT GLOB '*[^0-9a-f]*')
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

CREATE TABLE local_import_jobs (
    repo_id TEXT NOT NULL,
    job_id TEXT NOT NULL CHECK (
        length(job_id) = 36 AND job_id = lower(job_id)
    ),
    state TEXT NOT NULL CHECK (state IN (
        'DISCOVERING', 'READY', 'IMPORTING', 'PAUSED', 'COMPLETE',
        'ABANDONED', 'REVERTING', 'REVERTED'
    )),
    scope_path TEXT NOT NULL,
    author TEXT NOT NULL CHECK (author <> ''),
    message TEXT NOT NULL CHECK (message <> ''),
    ignore_blob_id TEXT NOT NULL CHECK (
        length(ignore_blob_id) = 71
        AND substr(ignore_blob_id, 1, 7) = 'sha256:'
        AND substr(ignore_blob_id, 8) NOT GLOB '*[^0-9a-f]*'
    ),
    source_device_id BLOB NOT NULL CHECK (length(source_device_id) = 8),
    source_file_id BLOB NOT NULL CHECK (length(source_file_id) = 8),
    source_birth_seconds INTEGER,
    source_birth_nanos INTEGER,
    destination_item_id TEXT,
    root_parent_item_id TEXT,
    discovery_complete INTEGER NOT NULL CHECK (discovery_complete IN (0, 1)),
    next_ordinal INTEGER NOT NULL CHECK (next_ordinal >= 1),
    entry_count INTEGER NOT NULL CHECK (entry_count >= 0),
    file_count INTEGER NOT NULL CHECK (file_count >= 0),
    folder_count INTEGER NOT NULL CHECK (folder_count >= 0),
    accepted_entry_count INTEGER NOT NULL CHECK (accepted_entry_count >= 0),
    accepted_commit_count INTEGER NOT NULL CHECK (accepted_commit_count >= 0),
    remaining_change_count INTEGER CHECK (remaining_change_count >= 0),
    revert_message TEXT CHECK (revert_message IS NULL OR revert_message <> ''),
    last_error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL, hidden_files TEXT NOT NULL DEFAULT 'IGNORE'
CHECK (hidden_files IN ('IGNORE', 'TRACK')),
    CHECK (
        (source_birth_seconds IS NULL AND source_birth_nanos IS NULL)
        OR (source_birth_seconds IS NOT NULL
            AND source_birth_nanos IS NOT NULL
            AND source_birth_nanos BETWEEN 0 AND 999999999)
    ),
    CHECK (accepted_entry_count <= entry_count),
    CHECK (file_count + folder_count = entry_count),
    CHECK (discovery_complete = 0 OR state <> 'DISCOVERING'),
    PRIMARY KEY (repo_id, job_id)
) STRICT;

CREATE TABLE local_import_entries (
    repo_id TEXT NOT NULL,
    job_id TEXT NOT NULL,
    entry_id INTEGER NOT NULL CHECK (entry_id >= 1),
    parent_entry_id INTEGER CHECK (parent_entry_id >= 1),
    ordinal INTEGER CHECK (ordinal >= 1),
    parent_ordinal INTEGER CHECK (parent_ordinal >= 1),
    depth INTEGER NOT NULL CHECK (depth >= 1),
    sort_key TEXT NOT NULL CHECK (sort_key <> ''),
    path TEXT NOT NULL CHECK (path <> ''),
    name TEXT NOT NULL CHECK (name <> ''),
    name_key TEXT NOT NULL CHECK (name_key <> ''),
    node_kind TEXT NOT NULL CHECK (node_kind IN ('FILE', 'FOLDER')),
    item_id TEXT NOT NULL,
    device_id BLOB NOT NULL CHECK (length(device_id) = 8),
    file_id BLOB NOT NULL CHECK (length(file_id) = 8),
    birth_seconds INTEGER,
    birth_nanos INTEGER,
    size_bytes INTEGER,
    mtime_seconds INTEGER,
    mtime_nanos INTEGER,
    change_seconds INTEGER,
    change_nanos INTEGER,
    prepared_blob_id TEXT,
    accepted_item_seq INTEGER CHECK (accepted_item_seq >= 1),
    accepted_commit_id TEXT,
    CHECK (parent_entry_id IS NULL OR parent_entry_id < entry_id),
    CHECK (ordinal IS NULL OR parent_ordinal IS NULL OR parent_ordinal < ordinal),
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
            AND ((change_seconds IS NULL AND change_nanos IS NULL)
                OR (change_seconds IS NOT NULL
                    AND change_nanos IS NOT NULL
                    AND change_nanos BETWEEN 0 AND 999999999)))
        OR (node_kind = 'FOLDER'
            AND size_bytes IS NULL
            AND mtime_seconds IS NULL
            AND mtime_nanos IS NULL
            AND change_seconds IS NULL
            AND change_nanos IS NULL
            AND prepared_blob_id IS NULL)
    ),
    CHECK (
        prepared_blob_id IS NULL
        OR (length(prepared_blob_id) = 71
            AND substr(prepared_blob_id, 1, 7) = 'sha256:'
            AND substr(prepared_blob_id, 8) NOT GLOB '*[^0-9a-f]*')
    ),
    CHECK (
        (accepted_commit_id IS NULL AND accepted_item_seq IS NULL)
        OR (accepted_commit_id IS NOT NULL AND accepted_item_seq IS NOT NULL)
    ),
    PRIMARY KEY (repo_id, job_id, entry_id),
    UNIQUE (repo_id, job_id, ordinal),
    UNIQUE (repo_id, job_id, path),
    UNIQUE (repo_id, job_id, item_id),
    FOREIGN KEY (repo_id, job_id)
        REFERENCES local_import_jobs(repo_id, job_id)
        ON DELETE CASCADE,
    FOREIGN KEY (repo_id, job_id, parent_entry_id)
        REFERENCES local_import_entries(repo_id, job_id, entry_id),
    FOREIGN KEY (repo_id, job_id, parent_ordinal)
        REFERENCES local_import_entries(repo_id, job_id, ordinal)
) STRICT;

CREATE INDEX local_import_entries_sibling_name_key
ON local_import_entries(repo_id, job_id, COALESCE(parent_entry_id, 0), name_key);

CREATE INDEX local_import_entries_execution_order
ON local_import_entries(repo_id, job_id, depth, sort_key);

CREATE TABLE local_import_commits (
    repo_id TEXT NOT NULL,
    job_id TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('IMPORT', 'REVERT')),
    chunk_index INTEGER NOT NULL CHECK (chunk_index >= 0),
    first_ordinal INTEGER NOT NULL CHECK (first_ordinal >= 1),
    last_ordinal INTEGER NOT NULL CHECK (last_ordinal >= first_ordinal),
    operation_id TEXT NOT NULL CHECK (operation_id <> ''),
    commit_id TEXT CHECK (commit_id IS NULL OR commit_id <> ''),
    published INTEGER NOT NULL CHECK (published IN (0, 1)),
    created_at TEXT NOT NULL,
    CHECK (published = 0 OR commit_id IS NOT NULL),
    PRIMARY KEY (repo_id, job_id, direction, chunk_index),
    UNIQUE (repo_id, operation_id),
    UNIQUE (repo_id, commit_id),
    FOREIGN KEY (repo_id, job_id)
        REFERENCES local_import_jobs(repo_id, job_id)
        ON DELETE CASCADE
) STRICT;

CREATE TABLE local_repository_settings (
    repo_id TEXT PRIMARY KEY,
    hidden_files TEXT NOT NULL CHECK (hidden_files IN ('IGNORE', 'TRACK'))
) STRICT;

CREATE TABLE local_email_import_sources (
    repo_id TEXT NOT NULL,
    path TEXT NOT NULL CHECK (path <> ''),
    item_id TEXT NOT NULL,
    source_blob_id TEXT NOT NULL,
    PRIMARY KEY (repo_id, path, item_id)
) STRICT;

INSERT INTO local_schema_meta(singleton, version) VALUES (1, 11);
