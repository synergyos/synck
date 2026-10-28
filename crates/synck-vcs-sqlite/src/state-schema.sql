CREATE TABLE vcs_state_nodes (
    repo_id TEXT NOT NULL,
    node_hash BLOB NOT NULL CHECK (length(node_hash) = 32),
    payload BLOB NOT NULL,
    PRIMARY KEY (repo_id, node_hash)
) STRICT;

CREATE TABLE vcs_state_roots (
    repo_id TEXT NOT NULL,
    repo_seq INTEGER NOT NULL CHECK (repo_seq >= 1),
    commit_id BLOB NOT NULL CHECK (length(commit_id) = 32),
    state_root BLOB NOT NULL CHECK (length(state_root) = 32),
    PRIMARY KEY (repo_id, repo_seq),
    UNIQUE (repo_id, commit_id),
    FOREIGN KEY (repo_id, commit_id)
        REFERENCES vcs_commits (repo_id, commit_id) ON DELETE RESTRICT,
    FOREIGN KEY (repo_id, state_root)
        REFERENCES vcs_state_nodes (repo_id, node_hash) ON DELETE RESTRICT
) STRICT;

CREATE TABLE vcs_state_projection_heads (
    repo_id TEXT PRIMARY KEY,
    applied_repo_seq INTEGER NOT NULL CHECK (applied_repo_seq >= 0),
    state_root BLOB NOT NULL CHECK (length(state_root) = 32),
    FOREIGN KEY (repo_id, state_root)
        REFERENCES vcs_state_nodes (repo_id, node_hash) ON DELETE RESTRICT
) STRICT;
