-- Core SQLite database: UTF-8, PRAGMA application_id = 1398361675 (0x53594E4B),
-- PRAGMA user_version = 8, PRAGMA journal_mode = WAL. See format.md.
CREATE TABLE vcs_metadata_objects (
    repo_id TEXT NOT NULL,
    object_id BLOB NOT NULL CHECK (length(object_id) = 32),
    payload BLOB NOT NULL CHECK (length(payload) BETWEEN 49 AND 1048642),
    PRIMARY KEY (repo_id, object_id)
) STRICT;

CREATE TABLE vcs_items (
    repo_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    item_type TEXT NOT NULL,
    parent_folder_id TEXT,
    name TEXT,
    name_key TEXT,
    record_key TEXT,
    link_folder_id TEXT,
    head_seq INTEGER NOT NULL CHECK (head_seq >= 1),
    lifecycle_state TEXT NOT NULL CHECK (lifecycle_state IN ('LIVE','TRASH','TOMBSTONED')),
    created_at TEXT NOT NULL,
    created_by TEXT NOT NULL,
    sequencing_mode TEXT NOT NULL CHECK (sequencing_mode IN ('LOCAL','FEDERATED')),
    federation_stream_id TEXT, ref_head_seq INTEGER NOT NULL DEFAULT 0 CHECK (ref_head_seq >= 0),
    PRIMARY KEY (repo_id, item_id),
    CHECK (
        (name IS NOT NULL AND name_key IS NOT NULL AND record_key IS NULL AND link_folder_id IS NULL)
        OR
        (name IS NULL AND name_key IS NULL AND parent_folder_id IS NULL)
    )
) STRICT;

CREATE TABLE vcs_commits (
    repo_id TEXT NOT NULL,
    commit_pk TEXT NOT NULL,
    commit_id BLOB,
    repo_seq INTEGER,
    operation_id TEXT,
    author_id TEXT NOT NULL,
    committer_id TEXT NOT NULL,
    message TEXT,
    occurred_at TEXT,
    created_at TEXT NOT NULL,
    event_count INTEGER,
    checkpoint BLOB,
    PRIMARY KEY (repo_id, commit_pk),
    UNIQUE (repo_id, commit_id),
    CHECK (repo_seq IS NULL OR repo_seq >= 1),
    CHECK (operation_id IS NULL OR trim(operation_id) <> ''),
    CHECK (commit_id IS NULL OR length(commit_id) = 32),
    CHECK ((commit_id IS NULL) = (event_count IS NULL) AND (event_count IS NULL OR event_count >= 0)),
    CHECK (checkpoint IS NULL OR length(checkpoint) = 32),
    -- Exactly the sealed, sequenced commits are published.
    CHECK ((checkpoint IS NULL) = (commit_id IS NULL OR repo_seq IS NULL))
) STRICT;

CREATE TABLE vcs_repo_activity_heads (
    repo_id TEXT PRIMARY KEY,
    next_repo_seq INTEGER NOT NULL CHECK (next_repo_seq >= 1),
    updated_at TEXT NOT NULL
) STRICT;

CREATE TABLE vcs_federation_domains (
    repo_id TEXT NOT NULL,
    domain_id TEXT NOT NULL,
    last_domain_seq INTEGER NOT NULL CHECK (last_domain_seq >= 0),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (repo_id, domain_id)
) STRICT;

CREATE UNIQUE INDEX idx_vcs_commits_repo_repo_seq
ON vcs_commits (repo_id, repo_seq) WHERE repo_seq IS NOT NULL;

CREATE INDEX idx_vcs_commits_repo_unsequenced_created_at_commit_pk
ON vcs_commits (repo_id, created_at, commit_pk)
WHERE repo_seq IS NULL AND commit_id IS NOT NULL;

CREATE UNIQUE INDEX idx_vcs_items_unique_name_key
ON vcs_items (repo_id, COALESCE(parent_folder_id, ''), name_key)
WHERE lifecycle_state = 'LIVE' AND name_key IS NOT NULL;

CREATE UNIQUE INDEX idx_vcs_items_unique_record_key
ON vcs_items (repo_id, item_type, record_key)
WHERE lifecycle_state = 'LIVE' AND record_key IS NOT NULL;

CREATE INDEX idx_vcs_items_link_folder
ON vcs_items (repo_id, link_folder_id)
WHERE link_folder_id IS NOT NULL;

CREATE UNIQUE INDEX idx_vcs_items_federated_unique
ON vcs_items (repo_id, federation_stream_id)
WHERE sequencing_mode = 'FEDERATED' AND federation_stream_id IS NOT NULL;

CREATE INDEX idx_vcs_items_repo_created_at_item_id
ON vcs_items (repo_id, created_at, item_id);

CREATE INDEX idx_vcs_items_repo_lifecycle_created_at_item_id
ON vcs_items (repo_id, lifecycle_state, created_at, item_id);

CREATE UNIQUE INDEX idx_vcs_commits_repo_operation_id
ON vcs_commits (repo_id, operation_id) WHERE operation_id IS NOT NULL;

CREATE TABLE "vcs_events" (
    repo_id TEXT NOT NULL,
    commit_pk TEXT NOT NULL,
    commit_event_idx INTEGER NOT NULL CHECK (commit_event_idx >= 0),
    item_id TEXT NOT NULL,
    item_seq INTEGER NOT NULL CHECK (item_seq >= 1),
    event_type TEXT NOT NULL,
    payload BLOB NOT NULL,
    created_at TEXT NOT NULL,
    parent_folder_id TEXT,
    parent_folder_id_before TEXT,
    PRIMARY KEY (repo_id, item_id, item_seq),
    UNIQUE (repo_id, commit_pk, commit_event_idx),
    FOREIGN KEY (repo_id, commit_pk) REFERENCES vcs_commits (repo_id, commit_pk)
) STRICT;

CREATE TABLE vcs_ref_events (
    repo_id TEXT NOT NULL,
    commit_pk TEXT NOT NULL,
    commit_event_idx INTEGER NOT NULL CHECK (commit_event_idx >= 0),
    owner_item_id TEXT NOT NULL,
    ref_seq INTEGER NOT NULL CHECK (ref_seq >= 1),
    event_type TEXT NOT NULL CHECK (trim(event_type) <> ''),
    payload BLOB NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (repo_id, owner_item_id, ref_seq),
    UNIQUE (repo_id, commit_pk, commit_event_idx),
    FOREIGN KEY (repo_id, commit_pk) REFERENCES vcs_commits (repo_id, commit_pk)
) STRICT;

CREATE INDEX idx_vcs_ref_events_repo_owner_event_type_ref_seq
ON vcs_ref_events (repo_id, owner_item_id, event_type, ref_seq DESC);

CREATE TABLE vcs_history_generations (
    repo_id TEXT PRIMARY KEY,
    generation TEXT NOT NULL
) STRICT;
