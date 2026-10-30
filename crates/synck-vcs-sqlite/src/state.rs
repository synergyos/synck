use async_trait::async_trait;
use rusqlite::{Connection, MAIN_DB, OptionalExtension, params};
use synck_vcs::state::{
    MAX_INDEX_NODE_BYTES, NamespaceKey, ProjectionItem, StagedNamespaceMutation, StateError,
    StateIndexStore, StateNode, StateNodeId, StateNodeStore, StateProjectionHead,
    StateProjectorStore, StateRootId, StateRootRecord, StateVerificationBatch, decode_node,
    hash_node_bytes,
};
use synck_vcs::types::{ItemId, RepoId, commit_id_from_digest_bytes, commit_id_to_digest_bytes};

use crate::SqliteStorage;
use crate::storage::STATE_SCHEMA;

const PROJECTOR_TEMP_SCHEMA: &str = "CREATE TEMP TABLE IF NOT EXISTS temp_state_nodes (
         node_hash BLOB PRIMARY KEY CHECK (length(node_hash) = 32),
         payload BLOB NOT NULL
     ) STRICT;
     CREATE TEMP TABLE IF NOT EXISTS temp_state_reachable (
         node_hash BLOB PRIMARY KEY CHECK (length(node_hash) = 32),
         visited INTEGER NOT NULL CHECK (visited IN (0, 1))
     ) STRICT;
     CREATE INDEX IF NOT EXISTS temp_state_reachable_pending
         ON temp_state_reachable(visited, node_hash);
     CREATE TEMP TABLE IF NOT EXISTS temp_projection_items (
         item_id TEXT PRIMARY KEY,
         payload BLOB NOT NULL
     ) STRICT;
     CREATE TEMP TABLE IF NOT EXISTS temp_namespace_mutations (
         sort_key BLOB PRIMARY KEY,
         parent_folder_id TEXT,
         name_key TEXT NOT NULL,
         mutation_kind INTEGER NOT NULL CHECK (mutation_kind IN (0, 1)),
         item_id TEXT,
         CHECK ((mutation_kind = 0 AND item_id IS NULL)
             OR (mutation_kind = 1 AND item_id IS NOT NULL))
     ) STRICT;";

pub struct SqliteStateStore {
    connection: Connection,
    repo_id: RepoId,
    batch_active: bool,
    projection_active: bool,
    projection_transaction_active: bool,
}

impl SqliteStateStore {
    /// Borrow the cache connection while holding its repository lock.
    pub fn authority_connection(&self) -> &Connection {
        &self.connection
    }

    /// Mutably borrow the cache connection while holding its repository lock.
    pub fn authority_connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
}

impl SqliteStorage {
    /// The historical index is disposable and never stored in canonical history.
    pub fn state_index_path(&self) -> std::path::PathBuf {
        self.path().with_extension("state.sqlite3")
    }

    /// Open the cache under the caller's repository lock, creating it if absent.
    pub fn state_index_store(&self, repo_id: RepoId) -> Result<SqliteStateStore, StateError> {
        let cache = SqliteStorage::new(self.state_index_path());
        let connection = cache
            .open_writable_unchecked(true)
            .map_err(|error| StateError::Storage(error.to_string()))?;
        let empty: bool = connection.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%')",
            [], |row| row.get(0),
        ).map_err(sql_error)?;
        if empty {
            // A killed process must leave no partial schema behind.
            connection
                .execute_batch(&format!("BEGIN IMMEDIATE;{STATE_SCHEMA}COMMIT;"))
                .map_err(sql_error)?;
        }
        let expected = Connection::open_in_memory().map_err(sql_error)?;
        expected.execute_batch(STATE_SCHEMA).map_err(sql_error)?;
        if crate::schema::inspect(&connection, "").map_err(sql_error)?
            != crate::schema::inspect(&expected, "").map_err(sql_error)? {
            return Err(StateError::Storage("invalid historical-state cache schema; remove the cache and rebuild it".into()));
        }
        connection.execute_batch(PROJECTOR_TEMP_SCHEMA).map_err(sql_error)?;
        Ok(SqliteStateStore {
            connection,
            repo_id,
            batch_active: false,
            projection_active: false,
            projection_transaction_active: false,
        })
    }

    /// Create a disposable, disk-backed state store for independent fsck replay.
    ///
    /// The database path must be dedicated to this verifier. Canonical commit
    /// rows deliberately do not exist here.
    pub fn verification_state_store(
        &self,
        repo_id: RepoId,
    ) -> Result<SqliteStateStore, StateError> {
        let connection = self
            .open_writable_unchecked(true)
            .map_err(|error| StateError::Storage(error.to_string()))?;
        connection
            .pragma_update(None, "foreign_keys", "OFF")
            .map_err(sql_error)?;
        connection
            .pragma_update(None, "journal_mode", "MEMORY")
            .map_err(sql_error)?;
        connection
            .pragma_update(None, "synchronous", "OFF")
            .map_err(sql_error)?;
        connection
            .pragma_update(None, "temp_store", "FILE")
            .map_err(sql_error)?;
        connection.execute_batch(STATE_SCHEMA).map_err(sql_error)?;
        connection
            .execute_batch(PROJECTOR_TEMP_SCHEMA)
            .map_err(sql_error)?;
        Ok(SqliteStateStore {
            connection,
            repo_id,
            batch_active: false,
            projection_active: false,
            projection_transaction_active: false,
        })
    }
}

fn encode_projection_item(item: &ProjectionItem) -> Result<Vec<u8>, StateError> {
    serde_json::to_vec(item).map_err(|error| StateError::Storage(error.to_string()))
}

fn decode_projection_item(bytes: &[u8]) -> Result<ProjectionItem, StateError> {
    serde_json::from_slice(bytes)
        .map_err(|error| StateError::Storage(format!("invalid projector scratch item: {error}")))
}

fn decode_item_id(value: String) -> Result<ItemId, StateError> {
    ItemId::parse(value)
        .map_err(|error| StateError::Storage(format!("invalid projector item ID: {error}")))
}

fn sql_error(error: rusqlite::Error) -> StateError {
    StateError::Storage(error.to_string())
}

fn decode_node_id(bytes: Vec<u8>) -> Result<StateNodeId, StateError> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| StateError::InvalidTree("stored state-node ID is not 32 bytes".to_string()))?;
    Ok(StateNodeId::from_bytes(bytes))
}

fn checked_limit(limit: usize) -> Result<i64, StateError> {
    if limit == 0 {
        return Err(StateError::Storage(
            "state batch limit must be positive".to_string(),
        ));
    }
    i64::try_from(limit)
        .map_err(|_| StateError::Storage("state batch limit is too large".to_string()))
}

impl SqliteStateStore {
    fn load_optional(&self, id: StateNodeId) -> Result<Option<Vec<u8>>, StateError> {
        if self.batch_active
            && let Some(bytes) = self
                .connection
                .prepare_cached("SELECT payload FROM temp_state_nodes WHERE node_hash = ?1")
                .map_err(sql_error)?
                .query_row([id.as_bytes().as_slice()], |row| row.get(0))
                .optional()
                .map_err(sql_error)?
        {
            return Ok(Some(bytes));
        }
        self.connection
            .prepare_cached(
                "SELECT payload FROM vcs_state_nodes
                 WHERE repo_id = ?1 AND node_hash = ?2",
            )
            .map_err(sql_error)?
            .query_row(
                params![self.repo_id.as_str(), id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)
    }

    fn insert_durable(&self, id: StateNodeId, bytes: &[u8]) -> Result<(), StateError> {
        self.connection
            .execute(
                "INSERT INTO vcs_state_nodes(repo_id, node_hash, payload)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(repo_id, node_hash) DO NOTHING",
                params![self.repo_id.as_str(), id.as_bytes().as_slice(), bytes],
            )
            .map_err(sql_error)?;
        let stored: Vec<u8> = self
            .connection
            .query_row(
                "SELECT payload FROM vcs_state_nodes
                 WHERE repo_id = ?1 AND node_hash = ?2",
                params![self.repo_id.as_str(), id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if stored != bytes {
            return Err(StateError::InvalidTree(
                "same node hash has different durable bytes".to_string(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl StateNodeStore for SqliteStateStore {
    async fn load(&mut self, id: StateNodeId) -> Result<Vec<u8>, StateError> {
        self.load_optional(id)?.ok_or(StateError::MissingNode(id))
    }

    async fn store(&mut self, id: StateNodeId, bytes: Vec<u8>) -> Result<(), StateError> {
        let actual = hash_node_bytes(&bytes);
        if actual != id {
            return Err(StateError::HashMismatch {
                expected: id,
                actual,
            });
        }
        if let Some(existing) = self.load_optional(id)? {
            return if existing == bytes {
                Ok(())
            } else {
                Err(StateError::InvalidTree(
                    "same node hash has different bytes".to_string(),
                ))
            };
        }
        if self.batch_active {
            self.connection
                .execute(
                    "INSERT INTO temp_state_nodes(node_hash, payload) VALUES (?1, ?2)",
                    params![id.as_bytes().as_slice(), bytes],
                )
                .map_err(sql_error)?;
            Ok(())
        } else {
            self.insert_durable(id, &bytes)
        }
    }

    async fn begin_batch(&mut self) -> Result<(), StateError> {
        if self.batch_active {
            return Err(StateError::Storage(
                "state-node mutation batch is already active".to_string(),
            ));
        }
        self.connection
            .execute_batch("DELETE FROM temp_state_nodes; DELETE FROM temp_state_reachable;")
            .map_err(sql_error)?;
        self.batch_active = true;
        Ok(())
    }

    async fn finish_batch(&mut self, roots: &[StateNodeId]) -> Result<(), StateError> {
        if !self.batch_active {
            return Err(StateError::Storage(
                "state-node mutation batch is not active".to_string(),
            ));
        }
        for root in roots {
            self.connection
                .execute(
                    "INSERT OR IGNORE INTO temp_state_reachable(node_hash, visited)
                     VALUES (?1, 0)",
                    [root.as_bytes().as_slice()],
                )
                .map_err(sql_error)?;
        }
        loop {
            let pending: Option<Vec<u8>> = self
                .connection
                .query_row(
                    "SELECT node_hash FROM temp_state_reachable
                     WHERE visited = 0 ORDER BY node_hash LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            let Some(pending) = pending else { break };
            let id = decode_node_id(pending)?;
            let staged: Option<Vec<u8>> = self
                .connection
                .query_row(
                    "SELECT payload FROM temp_state_nodes WHERE node_hash = ?1",
                    [id.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            let Some(bytes) = staged else {
                let durable = self
                    .connection
                    .query_row(
                        "SELECT 1 FROM vcs_state_nodes
                         WHERE repo_id = ?1 AND node_hash = ?2",
                        params![self.repo_id.as_str(), id.as_bytes().as_slice()],
                        |_| Ok(()),
                    )
                    .optional()
                    .map_err(sql_error)?;
                if durable.is_none() {
                    return Err(StateError::MissingNode(id));
                }
                self.connection
                    .execute(
                        "UPDATE temp_state_reachable SET visited = 1 WHERE node_hash = ?1",
                        [id.as_bytes().as_slice()],
                    )
                    .map_err(sql_error)?;
                continue;
            };
            let children = match decode_node(&bytes)? {
                StateNode::ItemValue(_) | StateNode::NamespaceLeaf(_) => Vec::new(),
                StateNode::ItemLeaf(entries) | StateNode::ItemInternal(entries) => {
                    entries.into_iter().map(|entry| entry.1).collect()
                }
                StateNode::NamespaceInternal(entries) => {
                    entries.into_iter().map(|entry| entry.1).collect()
                }
                StateNode::Root(root) => vec![root.item_map_root, root.namespace_root],
            };
            for child in children {
                self.connection
                    .execute(
                        "INSERT OR IGNORE INTO temp_state_reachable(node_hash, visited)
                         VALUES (?1, 0)",
                        [child.as_bytes().as_slice()],
                    )
                    .map_err(sql_error)?;
            }
            self.connection
                .execute(
                    "UPDATE temp_state_reachable SET visited = 1 WHERE node_hash = ?1",
                    [id.as_bytes().as_slice()],
                )
                .map_err(sql_error)?;
        }

        self.connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(sql_error)?;
        let result = self.connection.execute(
            "INSERT INTO vcs_state_nodes(repo_id, node_hash, payload)
             SELECT ?1, staged.node_hash, staged.payload
             FROM temp_state_nodes staged
             JOIN temp_state_reachable reachable USING(node_hash)
             ON CONFLICT(repo_id, node_hash) DO NOTHING",
            [self.repo_id.as_str()],
        );
        match result {
            Ok(_) => self.connection.execute_batch("COMMIT").map_err(sql_error)?,
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                return Err(sql_error(error));
            }
        }
        self.connection
            .execute_batch("DELETE FROM temp_state_nodes; DELETE FROM temp_state_reachable;")
            .map_err(sql_error)?;
        self.batch_active = false;
        Ok(())
    }

    async fn abort_batch(&mut self) -> Result<(), StateError> {
        if !self.batch_active {
            return Err(StateError::Storage(
                "state-node mutation batch is not active".to_string(),
            ));
        }
        self.connection
            .execute_batch("DELETE FROM temp_state_nodes; DELETE FROM temp_state_reachable;")
            .map_err(sql_error)?;
        self.batch_active = false;
        Ok(())
    }
}

#[async_trait]
impl StateIndexStore for SqliteStateStore {
    async fn projection_head(&mut self) -> Result<Option<StateProjectionHead>, StateError> {
        let row: Option<(i64, Vec<u8>)> = self
            .connection
            .query_row(
                "SELECT applied_repo_seq, state_root FROM vcs_state_projection_heads
                 WHERE repo_id = ?1",
                [self.repo_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_error)?;
        row.map(|(applied_repo_seq, root)| {
            Ok(StateProjectionHead {
                applied_repo_seq,
                state_root: StateRootId::from_node_id(decode_node_id(root)?),
            })
        })
        .transpose()
    }

    async fn root_at(&mut self, repo_seq: i64) -> Result<Option<StateRootRecord>, StateError> {
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .connection
            .query_row(
                "SELECT commit_id, state_root FROM vcs_state_roots
                 WHERE repo_id = ?1 AND repo_seq = ?2",
                params![self.repo_id.as_str(), repo_seq],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_error)?;
        row.map(|(commit, root)| {
            Ok(StateRootRecord {
                repo_seq,
                commit_id: commit_id_from_digest_bytes(&commit)
                    .map_err(|error| StateError::InvalidTree(error.to_string()))?,
                state_root: StateRootId::from_node_id(decode_node_id(root)?),
            })
        })
        .transpose()
    }

    async fn list_roots(
        &mut self,
        after_repo_seq: i64,
        limit: usize,
    ) -> Result<Vec<StateRootRecord>, StateError> {
        let limit = checked_limit(limit)?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT repo_seq, commit_id, state_root FROM vcs_state_roots
                 WHERE repo_id = ?1 AND repo_seq > ?2
                 ORDER BY repo_seq LIMIT ?3",
            )
            .map_err(sql_error)?;
        statement
            .query_map(
                params![self.repo_id.as_str(), after_repo_seq, limit],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .map_err(sql_error)?
            .map(|row| {
                let (repo_seq, commit, root) = row.map_err(sql_error)?;
                Ok(StateRootRecord {
                    repo_seq,
                    commit_id: commit_id_from_digest_bytes(&commit)
                        .map_err(|error| StateError::InvalidTree(error.to_string()))?,
                    state_root: StateRootId::from_node_id(decode_node_id(root)?),
                })
            })
            .collect()
    }

    async fn install_empty_head(&mut self, state_root: StateRootId) -> Result<(), StateError> {
        self.connection
            .execute(
                "INSERT INTO vcs_state_projection_heads(repo_id, applied_repo_seq, state_root)
                 VALUES (?1, 0, ?2)",
                params![self.repo_id.as_str(), state_root.as_bytes().as_slice()],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    async fn publish_root(
        &mut self,
        expected_applied_repo_seq: i64,
        root: &StateRootRecord,
    ) -> Result<(), StateError> {
        if root.repo_seq < 1 {
            return Err(StateError::Storage(
                "commit state root must have a positive repository sequence".to_string(),
            ));
        }
        self.connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(sql_error)?;
        let result = (|| {
            let applied: i64 = self
                .connection
                .query_row(
                    "SELECT applied_repo_seq FROM vcs_state_projection_heads WHERE repo_id = ?1",
                    [self.repo_id.as_str()],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            if applied != expected_applied_repo_seq {
                return Err(StateError::Storage(format!(
                    "stale state projection head: expected {expected_applied_repo_seq}, found {applied}"
                )));
            }
            self.connection
                .execute(
                    "INSERT INTO vcs_state_roots(repo_id, repo_seq, commit_id, state_root)
                 VALUES (?1, ?2, ?3, ?4)",
                    params![
                        self.repo_id.as_str(),
                        root.repo_seq,
                        commit_id_to_digest_bytes(&root.commit_id).as_slice(),
                        root.state_root.as_bytes().as_slice(),
                    ],
                )
                .map_err(sql_error)?;
            let updated = self
                .connection
                .execute(
                    "UPDATE vcs_state_projection_heads
                 SET applied_repo_seq = ?2, state_root = ?3
                 WHERE repo_id = ?1 AND applied_repo_seq = ?4",
                    params![
                        self.repo_id.as_str(),
                        root.repo_seq,
                        root.state_root.as_bytes().as_slice(),
                        expected_applied_repo_seq,
                    ],
                )
                .map_err(sql_error)?;
            if updated != 1 {
                return Err(StateError::Storage(
                    "state projection head changed during publication".to_string(),
                ));
            }
            Ok(())
        })();
        match result {
            Ok(()) => self.connection.execute_batch("COMMIT").map_err(sql_error),
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    async fn remove_head(&mut self) -> Result<(), StateError> {
        self.connection
            .execute(
                "DELETE FROM vcs_state_projection_heads WHERE repo_id = ?1",
                [self.repo_id.as_str()],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    async fn delete_roots_batch(&mut self, limit: usize) -> Result<usize, StateError> {
        let limit = checked_limit(limit)?;
        self.connection
            .execute(
                "DELETE FROM vcs_state_roots WHERE rowid IN (
                     SELECT rowid FROM vcs_state_roots
                     WHERE repo_id = ?1 ORDER BY repo_seq LIMIT ?2
                 )",
                params![self.repo_id.as_str(), limit],
            )
            .map_err(sql_error)
    }

    async fn delete_nodes_batch(&mut self, limit: usize) -> Result<usize, StateError> {
        let limit = checked_limit(limit)?;
        self.connection
            .execute(
                "DELETE FROM vcs_state_nodes WHERE rowid IN (
                     SELECT rowid FROM vcs_state_nodes
                     WHERE repo_id = ?1 ORDER BY node_hash LIMIT ?2
                 )",
                params![self.repo_id.as_str(), limit],
            )
            .map_err(sql_error)
    }

    async fn state_row_counts(&mut self) -> Result<(u64, u64, u64), StateError> {
        let row: (i64, i64, i64) = self
            .connection
            .query_row(
                "SELECT
                    (SELECT count(*) FROM vcs_state_nodes WHERE repo_id = ?1),
                    (SELECT count(*) FROM vcs_state_roots WHERE repo_id = ?1),
                    (SELECT count(*) FROM vcs_state_projection_heads WHERE repo_id = ?1)",
                [self.repo_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(sql_error)?;
        Ok((row.0 as u64, row.1 as u64, row.2 as u64))
    }

    async fn begin_state_verification(&mut self) -> Result<(), StateError> {
        self.connection
            .execute_batch("DELETE FROM temp_state_reachable;")
            .map_err(sql_error)
    }

    async fn verify_root_closures(
        &mut self,
        roots: &[StateRootId],
    ) -> Result<StateVerificationBatch, StateError> {
        const NODE_PAGE: usize = 512;
        const NODE_BATCH_BYTES: usize = 16 * 1024 * 1024;

        let mut insert = self
            .connection
            .prepare_cached(
                "INSERT OR IGNORE INTO temp_state_reachable(node_hash, visited) VALUES (?1, 0)",
            )
            .map_err(sql_error)?;
        for root in roots {
            insert
                .execute([root.as_bytes().as_slice()])
                .map_err(sql_error)?;
        }
        drop(insert);
        let limit = checked_limit(NODE_PAGE)?;
        let pending = {
            let mut statement = self
                .connection
                .prepare_cached(
                    "SELECT r.node_hash, n.rowid
                     FROM temp_state_reachable r
                     LEFT JOIN vcs_state_nodes n
                       ON n.repo_id = ?1 AND n.node_hash = r.node_hash
                     WHERE r.visited = 0 ORDER BY r.node_hash LIMIT ?2",
                )
                .map_err(sql_error)?;
            statement
                .query_map(params![self.repo_id.as_str(), limit], |row| {
                    Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<i64>>(1)?))
                })
                .map_err(sql_error)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sql_error)?
        };
        if pending.is_empty() {
            return Ok(StateVerificationBatch {
                nodes_verified: 0,
                findings: Vec::new(),
                complete: true,
            });
        }

        let mut findings = Vec::new();
        let mut children = Vec::new();
        let mut nodes_verified = 0_u64;
        let mut batch_bytes = 0_usize;
        let mut processed = 0_usize;
        for (node_hash, row_id) in &pending {
            let id = match decode_node_id(node_hash.clone()) {
                Ok(id) => id,
                Err(error) => {
                    processed += 1;
                    findings.push(error);
                    continue;
                }
            };
            let Some(row_id) = row_id else {
                processed += 1;
                findings.push(StateError::MissingNode(id));
                continue;
            };
            // SQL substr() materializes the whole BLOB inside SQLite. Read the
            // header incrementally and keep the same read-only handle for the
            // payload, but only allocate that payload after the budget check.
            let blob = self
                .connection
                .blob_open(MAIN_DB, "vcs_state_nodes", "payload", *row_id, true)
                .map_err(sql_error)?;
            let size = blob.len();
            let mut header = [0; 2];
            blob.read_at(&mut header, 0).map_err(sql_error)?;
            let max_size = match header[1] {
                0x01 => usize::MAX,
                0x06 => 123,
                _ => MAX_INDEX_NODE_BYTES,
            };
            let oversized = size > max_size;
            if !oversized && processed != 0 && batch_bytes + size > NODE_BATCH_BYTES {
                break;
            }
            processed += 1;
            if oversized {
                findings.push(StateError::InvalidEncoding(format!(
                    "state node {id} is {size} bytes, exceeding its {max_size}-byte format limit"
                )));
                continue;
            }
            batch_bytes += size;
            let mut bytes = vec![0; size];
            blob.read_at_exact(&mut bytes, 0).map_err(sql_error)?;
            blob.close().map_err(sql_error)?;
            let actual = hash_node_bytes(&bytes);
            if actual != id {
                findings.push(StateError::HashMismatch {
                    expected: id,
                    actual,
                });
                continue;
            }
            let node = match decode_node(&bytes) {
                Ok(node) => node,
                Err(error) => {
                    findings.push(error);
                    continue;
                }
            };
            nodes_verified = nodes_verified
                .checked_add(1)
                .ok_or_else(|| StateError::Storage("verified node count overflow".to_string()))?;
            let node_children = match node {
                StateNode::ItemValue(_) | StateNode::NamespaceLeaf(_) => Vec::new(),
                StateNode::ItemLeaf(entries) | StateNode::ItemInternal(entries) => {
                    entries.into_iter().map(|entry| entry.1).collect()
                }
                StateNode::NamespaceInternal(entries) => {
                    entries.into_iter().map(|entry| entry.1).collect()
                }
                StateNode::Root(root) => vec![root.item_map_root, root.namespace_root],
            };
            children.extend(node_children);
        }

        let mut inserted_children = 0_usize;
        let mut insert = self
            .connection
            .prepare_cached(
                "INSERT OR IGNORE INTO temp_state_reachable(node_hash, visited) VALUES (?1, 0)",
            )
            .map_err(sql_error)?;
        for child in children {
            inserted_children = inserted_children
                .checked_add(
                    insert
                        .execute([child.as_bytes().as_slice()])
                        .map_err(sql_error)?,
                )
                .ok_or_else(|| StateError::Storage("inserted node count overflow".to_string()))?;
        }
        drop(insert);
        let mut retire = self
            .connection
            .prepare_cached("UPDATE temp_state_reachable SET visited = 1 WHERE node_hash = ?1")
            .map_err(sql_error)?;
        for (node_hash, _) in &pending[..processed] {
            retire.execute([node_hash]).map_err(sql_error)?;
        }
        drop(retire);

        Ok(StateVerificationBatch {
            nodes_verified,
            findings,
            complete: processed == pending.len()
                && pending.len() < NODE_PAGE
                && inserted_children == 0,
        })
    }

    async fn finish_state_verification(&mut self) -> Result<(), StateError> {
        self.connection
            .execute_batch("DELETE FROM temp_state_reachable;")
            .map_err(sql_error)
    }
}

#[async_trait]
impl StateProjectorStore for SqliteStateStore {
    async fn begin_projection_commit(&mut self) -> Result<(), StateError> {
        if self.projection_active {
            return Err(StateError::Storage(
                "state projection scratch is already active".to_string(),
            ));
        }
        self.connection
            .execute_batch(
                "DELETE FROM temp_projection_items; DELETE FROM temp_namespace_mutations;",
            )
            .map_err(sql_error)?;
        self.connection.execute_batch("BEGIN").map_err(sql_error)?;
        self.projection_active = true;
        self.projection_transaction_active = true;
        Ok(())
    }

    async fn projection_item(
        &mut self,
        item_id: &ItemId,
    ) -> Result<Option<ProjectionItem>, StateError> {
        let payload: Option<Vec<u8>> = self
            .connection
            .prepare_cached("SELECT payload FROM temp_projection_items WHERE item_id = ?1")
            .map_err(sql_error)?
            .query_row([item_id.as_str()], |row| row.get(0))
            .optional()
            .map_err(sql_error)?;
        payload.as_deref().map(decode_projection_item).transpose()
    }

    async fn put_projection_item(
        &mut self,
        item_id: &ItemId,
        item: &ProjectionItem,
    ) -> Result<(), StateError> {
        let payload = encode_projection_item(item)?;
        self.connection
            .prepare_cached(
                "INSERT INTO temp_projection_items(item_id, payload) VALUES (?1, ?2)
                 ON CONFLICT(item_id) DO UPDATE SET payload = excluded.payload",
            )
            .map_err(sql_error)?
            .execute(params![item_id.as_str(), payload])
            .map_err(sql_error)?;
        Ok(())
    }

    async fn list_projection_items(
        &mut self,
        after: Option<&ItemId>,
        limit: usize,
    ) -> Result<Vec<(ItemId, ProjectionItem)>, StateError> {
        let limit = checked_limit(limit)?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT item_id, payload FROM temp_projection_items
                 WHERE (?1 IS NULL OR item_id > ?1) ORDER BY item_id LIMIT ?2",
            )
            .map_err(sql_error)?;
        statement
            .query_map(params![after.map(ItemId::as_str), limit], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(sql_error)?
            .map(|row| {
                let (item_id, payload) = row.map_err(sql_error)?;
                Ok((decode_item_id(item_id)?, decode_projection_item(&payload)?))
            })
            .collect()
    }

    async fn put_namespace_mutation(
        &mut self,
        mutation: &StagedNamespaceMutation,
    ) -> Result<(), StateError> {
        let (key, item_id) = match mutation {
            StagedNamespaceMutation::Put(key, item_id) => (key, Some(item_id)),
            StagedNamespaceMutation::Remove(key) => (key, None),
        };
        let sort_key = key.canonical_sort_key();
        if let Some(item_id) = item_id {
            let existing: Option<(i64, Option<String>)> = self
                .connection
                .prepare_cached(
                    "SELECT mutation_kind, item_id FROM temp_namespace_mutations
                     WHERE sort_key = ?1",
                )
                .map_err(sql_error)?
                .query_row([sort_key.as_slice()], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()
                .map_err(sql_error)?;
            if let Some((1, Some(existing))) = existing
                && existing != item_id.as_str()
            {
                return Err(StateError::InvalidTree(format!(
                    "two live items claim namespace key {:?}",
                    key
                )));
            }
        }
        self.connection
            .prepare_cached(
                "INSERT INTO temp_namespace_mutations(
                     sort_key, parent_folder_id, name_key, mutation_kind, item_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(sort_key) DO UPDATE SET
                     parent_folder_id = excluded.parent_folder_id,
                     name_key = excluded.name_key,
                     mutation_kind = excluded.mutation_kind,
                     item_id = excluded.item_id",
            )
            .map_err(sql_error)?
            .execute(params![
                sort_key,
                key.parent_folder_id.as_ref().map(ItemId::as_str),
                key.name_key,
                i64::from(item_id.is_some()),
                item_id.map(ItemId::as_str),
            ])
            .map_err(sql_error)?;
        Ok(())
    }

    async fn list_namespace_mutations(
        &mut self,
        after_sort_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, StagedNamespaceMutation)>, StateError> {
        let limit = checked_limit(limit)?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT sort_key, parent_folder_id, name_key, mutation_kind, item_id
                 FROM temp_namespace_mutations
                 WHERE (?1 IS NULL OR sort_key > ?1) ORDER BY sort_key LIMIT ?2",
            )
            .map_err(sql_error)?;
        statement
            .query_map(params![after_sort_key, limit], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(sql_error)?
            .map(|row| {
                let (sort_key, parent, name_key, kind, item_id) = row.map_err(sql_error)?;
                let key = NamespaceKey::new(parent.map(decode_item_id).transpose()?, name_key);
                let mutation = match (kind, item_id) {
                    (0, None) => StagedNamespaceMutation::Remove(key),
                    (1, Some(item_id)) => {
                        StagedNamespaceMutation::Put(key, decode_item_id(item_id)?)
                    }
                    _ => {
                        return Err(StateError::Storage(
                            "invalid projector namespace scratch row".to_string(),
                        ));
                    }
                };
                Ok((sort_key, mutation))
            })
            .collect()
    }

    async fn finish_projection_commit(&mut self) -> Result<(), StateError> {
        if !self.projection_active {
            return Err(StateError::Storage(
                "state projection scratch is not active".to_string(),
            ));
        }
        if self.projection_transaction_active {
            self.connection
                .execute_batch("ROLLBACK")
                .map_err(sql_error)?;
            self.projection_transaction_active = false;
        }
        self.connection
            .execute_batch(
                "DELETE FROM temp_projection_items; DELETE FROM temp_namespace_mutations;",
            )
            .map_err(sql_error)?;
        self.projection_active = false;
        Ok(())
    }

    async fn seal_projection_commit(&mut self) -> Result<(), StateError> {
        if !self.projection_active || !self.projection_transaction_active {
            return Err(StateError::Storage(
                "state projection scratch transaction is not active".to_string(),
            ));
        }
        self.connection.execute_batch("COMMIT").map_err(sql_error)?;
        self.projection_transaction_active = false;
        Ok(())
    }
}

impl Drop for SqliteStateStore {
    fn drop(&mut self) {
        if self.batch_active {
            let _ = self
                .connection
                .execute_batch("DELETE FROM temp_state_nodes; DELETE FROM temp_state_reachable;");
        }
        if self.projection_active {
            if self.projection_transaction_active {
                let _ = self.connection.execute_batch("ROLLBACK");
            }
            let _ = self.connection.execute_batch(
                "DELETE FROM temp_projection_items; DELETE FROM temp_namespace_mutations;",
            );
        }
    }
}
