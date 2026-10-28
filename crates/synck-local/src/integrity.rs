use crate::error::{LocalError, Result};
use crate::repository::Repository;
use rusqlite::{OptionalExtension, params};
use std::collections::HashMap;
use synck_vcs::errors::StorageError;
use synck_vcs::types::{ObjectId, CommitPk, ItemId, RepoId};
use synck_vcs_sqlite::{SqliteStorage, parse_timestamp};

/// A bounded integrity report with an exact total finding count.
///
/// `findings` retains the first diagnostics suitable for display. Additional
/// diagnostics are counted in `findings_omitted` rather than silently lost.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct FsckReport {
    pub findings: Vec<String>,
    pub findings_omitted: usize,
    pub notices: Vec<String>,
}

impl FsckReport {
    /// Returns true only when the repository has no integrity findings.
    pub fn is_empty(&self) -> bool {
        self.findings.is_empty() && self.findings_omitted == 0
    }

    /// Returns the exact number of findings, including omitted diagnostics.
    pub fn len(&self) -> usize {
        self.findings.len() + self.findings_omitted
    }

    /// Returns the first retained diagnostic, if any.
    pub fn first(&self) -> Option<&String> {
        self.findings.first()
    }

    /// Iterates over retained diagnostics. Use [`Self::len`] for the exact total.
    pub fn iter(&self) -> impl Iterator<Item = &String> {
        self.findings.iter()
    }
}

const MAX_REPORTED_FINDINGS: usize = 100;

#[derive(Debug, Default)]
pub(crate) struct Findings {
    reported: Vec<String>,
    total: usize,
}

impl Findings {
    pub(crate) fn push(&mut self, finding: String) {
        self.total += 1;
        if self.reported.len() < MAX_REPORTED_FINDINGS {
            self.reported.push(finding);
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub(crate) fn summary(&self) -> String {
        let mut summary = self.reported.join("; ");
        let omitted = self.total - self.reported.len();
        if omitted != 0 {
            summary.push_str(&format!("; {omitted} additional finding(s) omitted"));
        }
        summary
    }
}

impl Repository {
    pub async fn fsck(&self) -> Result<FsckReport> {
        match self.verify_complete().await {
            Ok(_) => Ok(FsckReport::default()),
            Err(error) => Ok(FsckReport {
                findings: vec![error.to_string()],
                ..FsckReport::default()
            }),
        }
    }
}

pub(crate) fn physical_findings(storage: &SqliteStorage, repo_id: &RepoId) -> Result<Findings> {
    physical_findings_inner(storage, repo_id, true)
}

pub(crate) fn physical_core_findings(
    storage: &SqliteStorage,
    repo_id: &RepoId,
) -> Result<Findings> {
    physical_findings_inner(storage, repo_id, false)
}

fn physical_findings_inner(
    storage: &SqliteStorage,
    repo_id: &RepoId,
    include_local: bool,
) -> Result<Findings> {
    let connection = storage.open_readonly().map_err(verification_open_error)?;
    let mut findings = Findings::default();
    check_physical_database(&connection, &mut findings)?;
    if findings.is_empty() {
        validate_physical_rows(&connection, repo_id, &mut findings)?;
    }
    if include_local && findings.is_empty() {
        let local = crate::local_database::open_readonly(storage)?;
        check_physical_database(&local, &mut findings)?;
        if findings.is_empty() {
            validate_local_rows(&local, repo_id, &mut findings)?;
        }
    }
    Ok(findings)
}

fn check_physical_database(connection: &rusqlite::Connection, findings: &mut Findings) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA integrity_check")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        let message = row?;
        if message != "ok" {
            findings.push(format!("SQLite integrity check: {message}"));
        }
    }
    if !findings.is_empty() {
        return Ok(());
    }
    let mut foreign_keys = connection.prepare("PRAGMA foreign_key_check")?;
    let rows = foreign_keys.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (table, rowid, parent, constraint) = row?;
        findings.push(format!(
            "SQLite foreign key violation: table={table} rowid={rowid:?} parent={parent} constraint={constraint}"
        ));
    }
    Ok(())
}

fn verification_open_error(error: StorageError) -> LocalError {
    match error {
        StorageError::Io { detail } | StorageError::Overloaded { detail } => {
            LocalError::Invalid(format!("cannot read repository database: {detail}"))
        }
        other => LocalError::Integrity(format!("invalid repository database: {other}")),
    }
}

fn validate_physical_rows(
    connection: &rusqlite::Connection,
    repo_id: &RepoId,
    findings: &mut Findings,
) -> Result<()> {
    let mut items = connection.prepare(
        "SELECT item_id, parent_folder_id, created_at
         FROM vcs_items WHERE repo_id = ?1 ORDER BY item_id",
    )?;
    let rows = items.query_map([repo_id.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (item_id, parent_id, created_at) = row?;
        validate_item_id(&item_id, "item ID", findings);
        if let Some(parent_id) = parent_id {
            validate_item_id(&parent_id, &format!("item {item_id} parent ID"), findings);
        }
        validate_timestamp(&created_at, &format!("item {item_id} created_at"), findings);
    }
    drop(items);

    let mut commits = connection.prepare(
        "SELECT commit_pk, format_version, event_count, occurred_at, created_at
         FROM vcs_commits WHERE repo_id = ?1 ORDER BY commit_pk",
    )?;
    let rows = commits.query_map([repo_id.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (commit_pk, format_version, event_count, occurred_at, created_at) = row?;
        validate_commit_pk(&commit_pk, findings);
        validate_i32(
            format_version,
            &format!("commit {commit_pk} format_version"),
            findings,
        );
        if let Some(event_count) = event_count {
            validate_i32(
                event_count,
                &format!("commit {commit_pk} event_count"),
                findings,
            );
        }
        if let Some(occurred_at) = occurred_at {
            validate_timestamp(
                &occurred_at,
                &format!("commit {commit_pk} occurred_at"),
                findings,
            );
        }
        validate_timestamp(
            &created_at,
            &format!("commit {commit_pk} created_at"),
            findings,
        );
    }
    drop(commits);

    let mut events = connection.prepare(
        "SELECT commit_pk, commit_event_idx, item_id, created_at,
                parent_folder_id, parent_folder_id_before
         FROM vcs_events WHERE repo_id = ?1
         ORDER BY commit_pk, commit_event_idx",
    )?;
    let rows = events.query_map([repo_id.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
        ))
    })?;
    for row in rows {
        let (commit_pk, index, item_id, created_at, parent_id, parent_id_before) = row?;
        validate_i32(index, &format!("commit {commit_pk} event index"), findings);
        validate_item_id(
            &item_id,
            &format!("commit {commit_pk} event {index} item ID"),
            findings,
        );
        validate_timestamp(
            &created_at,
            &format!("commit {commit_pk} event {index} created_at"),
            findings,
        );
        if let Some(parent_id) = parent_id {
            validate_item_id(
                &parent_id,
                &format!("commit {commit_pk} event {index} parent ID"),
                findings,
            );
        }
        if let Some(parent_id) = parent_id_before {
            validate_item_id(
                &parent_id,
                &format!("commit {commit_pk} event {index} previous parent ID"),
                findings,
            );
        }
    }
    drop(events);

    let maximum_repo_seq: Option<i64> = connection.query_row(
        "SELECT max(repo_seq) FROM vcs_commits WHERE repo_id = ?1",
        [repo_id.as_str()],
        |row| row.get(0),
    )?;
    let activity_head: Option<(i64, String)> = connection
        .query_row(
            "SELECT next_repo_seq, updated_at FROM vcs_repo_activity_heads
             WHERE repo_id = ?1",
            [repo_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((_, updated_at)) = &activity_head {
        validate_timestamp(updated_at, "repository activity head updated_at", findings);
    }
    if let Some(maximum) = maximum_repo_seq {
        match activity_head {
            None => findings.push(format!(
                "repository activity head is missing after sequence {maximum}"
            )),
            Some((next, _)) if next <= maximum => findings.push(format!(
                "repository activity head next sequence {next} does not follow maximum committed sequence {maximum}"
            )),
            Some(_) => {}
        }
    }

    Ok(())
}

fn validate_local_rows(
    connection: &rusqlite::Connection,
    repo_id: &RepoId,
    findings: &mut Findings,
) -> Result<()> {
    let mut projection = connection.prepare(
        "SELECT item_id, blob_id FROM local_file_heads
         WHERE repo_id = ?1 ORDER BY item_id",
    )?;
    let rows = projection.query_map([repo_id.as_str()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (item_id, blob_id) = row?;
        validate_item_id(&item_id, "projected item ID", findings);
        match ObjectId::parse(&blob_id) {
            Ok(parsed) if parsed.as_str() == blob_id => {}
            Ok(_) => findings.push(format!(
                "projected item {item_id} has noncanonical blob ID {blob_id:?}"
            )),
            Err(error) => findings.push(format!(
                "projected item {item_id} has invalid blob ID {blob_id:?}: {error}"
            )),
        }
    }
    Ok(())
}

fn validate_commit_pk(value: &str, findings: &mut Findings) {
    match CommitPk::parse(value) {
        Ok(parsed) if parsed.as_str() == value => {}
        Ok(_) => findings.push(format!("noncanonical commit key {value:?}")),
        Err(error) => findings.push(format!("invalid commit key {value:?}: {error}")),
    }
}

fn validate_item_id(value: &str, context: &str, findings: &mut Findings) {
    match ItemId::parse(value) {
        Ok(parsed) if parsed.as_str() == value => {}
        Ok(_) => findings.push(format!("{context} is noncanonical: {value:?}")),
        Err(error) => findings.push(format!("{context} is invalid {value:?}: {error}")),
    }
}

fn validate_timestamp(value: &str, context: &str, findings: &mut Findings) {
    if let Err(error) = parse_timestamp(value) {
        findings.push(format!("{context} is invalid: {error}"));
    }
}

fn validate_i32(value: i64, context: &str, findings: &mut Findings) {
    if i32::try_from(value).is_err() {
        findings.push(format!(
            "{context} is outside the signed 32-bit range: {value}"
        ));
    }
}

pub(crate) fn verify_projection(
    connection: &rusqlite::Connection,
    repo_id: &str,
    expected: &HashMap<String, (i64, String, i64)>,
    findings: &mut Findings,
) -> Result<()> {
    let maximum: i64 = connection.query_row(
        "SELECT COALESCE(max(repo_seq), 0) FROM vcs_commits WHERE repo_id = ?1",
        [repo_id],
        |row| row.get(0),
    )?;
    let applied: Option<i64> = connection
        .query_row(
            "SELECT applied_repo_seq FROM local_projection_state WHERE repo_id = ?1",
            [repo_id],
            |row| row.get(0),
        )
        .optional()?;
    if applied != Some(maximum) {
        findings.push(format!(
            "local projection sequence is {applied:?}, expected {maximum}"
        ));
    }
    let actual_count: i64 = connection.query_row(
        "SELECT count(*) FROM local_file_heads WHERE repo_id = ?1",
        [repo_id],
        |row| row.get(0),
    )?;
    if usize::try_from(actual_count).ok() != Some(expected.len()) {
        findings.push("local file-head projection row count mismatch".to_string());
    }
    for (item_id, (item_seq, blob_id, size)) in expected {
        let actual: Option<(i64, String, i64)> = connection
            .query_row(
                "SELECT item_seq, blob_id, size_bytes FROM local_file_heads
                 WHERE repo_id = ?1 AND item_id = ?2",
                params![repo_id, item_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if actual.as_ref() != Some(&(*item_seq, blob_id.clone(), *size)) {
            findings.push(format!("local projection mismatch for item {item_id}"));
        }
    }
    Ok(())
}
