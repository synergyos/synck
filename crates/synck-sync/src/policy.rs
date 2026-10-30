//! Server admission policy is separate from format validity and history reads.
use serde::{Deserialize, Serialize};
use synck_vcs::types::{CommitId, CommitRow};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerPolicy {
    pub max_events_per_commit: Option<u64>,
    pub max_object_bytes: Option<u64>,
    pub quota_bytes: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyLimitName {
    EventsPerCommit,
    ObjectBytes,
    QuotaBytes,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(deny_unknown_fields)]
#[error("{limit:?}: actual={actual}, maximum={maximum}")]
pub struct PolicyLimit {
    pub limit: PolicyLimitName,
    pub maximum: u64,
    pub actual: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationViolation {
    pub commit_id: CommitId,
    pub violation: PolicyLimit,
}
impl ServerPolicy {
    pub fn check_commit(&self, events: u64) -> Result<(), PolicyLimit> {
        check(
            PolicyLimitName::EventsPerCommit,
            self.max_events_per_commit,
            events,
        )
    }
    pub fn check_object(&self, bytes: u64) -> Result<(), PolicyLimit> {
        check(PolicyLimitName::ObjectBytes, self.max_object_bytes, bytes)
    }
    pub fn check_quota(&self, stored: u64, additional: u64) -> Result<(), PolicyLimit> {
        // Saturation is a refusal even when the arithmetic cannot fit u64.
        if let Some(maximum) = self.quota_bytes
            && stored.checked_add(additional).is_none() {
                return Err(PolicyLimit {
                    limit: PolicyLimitName::QuotaBytes,
                    maximum,
                    actual: u64::MAX,
                });
            }
        check(
            PolicyLimitName::QuotaBytes,
            self.quota_bytes,
            stored.saturating_add(additional),
        )
    }
    /// Preflight every existing commit before starting any upload. The caller
    /// also inventories object sizes/quota and retains that complete report.
    /// This never applies admission policy to downloading existing history.
    pub fn publication_violations<'a>(
        &self,
        commits: impl IntoIterator<Item = &'a CommitRow>,
    ) -> Vec<PublicationViolation> {
        commits
            .into_iter()
            .filter_map(|commit| {
                self.check_commit(commit.event_count as u64)
                    .err()
                    .map(|violation| PublicationViolation {
                        commit_id: commit.commit_id.clone(),
                        violation,
                    })
            })
            .collect()
    }
}
fn check(limit: PolicyLimitName, maximum: Option<u64>, actual: u64) -> Result<(), PolicyLimit> {
    if let Some(maximum) = maximum
        && actual > maximum {
            return Err(PolicyLimit {
                limit,
                maximum,
                actual,
            });
        }
    Ok(())
}
