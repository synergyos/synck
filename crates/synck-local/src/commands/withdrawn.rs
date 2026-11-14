//! Withdrawn saves: held commits a pull took back out of the history.
//!
//! A held commit names the item heads it was made from. When the origin has
//! moved one of them, the commit cannot be accepted there with its ID, and a
//! pull withdraws it: its changes go back into the working files and the
//! commit itself is kept here, in `withdrawn.json`, exactly as it would be
//! proposed to a server. Each keeps its commit ID and gets a label, `w1`,
//! `w2`, ..., that is never reused. Its content stays stored and protected
//! from `gc` for `WITHDRAWN_DAYS` after it was withdrawn, or until
//! `synck withdrawn discard`.

use super::*;
use synck_sync::{CommitMetadata, ProposalRecord};
use synck_vcs::metadata::Metadata;

pub(crate) const WITHDRAWN_FILE: &str = "withdrawn.json";
/// How long a withdrawn save is kept, as Git keeps reflog entries.
pub const WITHDRAWN_DAYS: i64 = 90;

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WithdrawnFile {
    /// The number the next withdrawn save is labelled with.
    pub(crate) next: u64,
    pub(crate) saves: Vec<WithdrawnSave>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WithdrawnSave {
    pub label: u64,
    pub commit_id: CommitId,
    pub metadata: CommitMetadata,
    /// When the commit was made.
    pub saved_at: DateTime<Utc>,
    pub withdrawn_at: DateTime<Utc>,
    /// Item events, then ref events, as a proposal to a server carries them.
    pub records: Vec<ProposalRecord>,
    /// Each metadata object the events name, as hexadecimal stored bytes.
    pub metadata_objects: BTreeMap<ObjectId, String>,
    /// What the commit changed, as `show` listed it before it was withdrawn.
    pub changes: Vec<Change>,
    /// The files whose bytes it saved.
    pub files: Vec<WithdrawnFileVersion>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WithdrawnFileVersion {
    pub path: String,
    pub blob_id: ObjectId,
    pub size: u64,
}

impl WithdrawnSave {
    pub fn name(&self) -> String {
        format!("w{}", self.label)
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.withdrawn_at + chrono::TimeDelta::days(WITHDRAWN_DAYS)
    }

    fn live(&self, now: DateTime<Utc>) -> bool {
        self.expires_at() > now
    }

    /// Content objects the commit's events reference.
    pub(crate) fn objects(&self) -> Result<BTreeSet<ObjectId>> {
        let mut objects = BTreeSet::new();
        for record in &self.records {
            let references = match record {
                ProposalRecord::Item(event) => synck_items::references::event_references(event),
                ProposalRecord::Ref(event) => synck_items::references::ref_event_references(event),
                ProposalRecord::Guard(_) | ProposalRecord::SubtreeGuard(_) => continue,
            }
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
            objects.extend(references.objects);
        }
        Ok(objects)
    }

    /// The commit ID its header and events hash to, and each stored metadata
    /// object against its ID.
    fn verify(&self) -> Result<()> {
        let mut digest = EventsDigest::default();
        let mut count = 0usize;
        for record in &self.records {
            let (family, bytes) = match record {
                ProposalRecord::Item(event) => (
                    COMMIT_EVENT_FAMILY_ITEM,
                    canonical_event_proposal_bytes(event)?,
                ),
                ProposalRecord::Ref(event) => (
                    COMMIT_EVENT_FAMILY_REF,
                    canonical_ref_event_proposal_bytes(event)?,
                ),
                ProposalRecord::Guard(_) | ProposalRecord::SubtreeGuard(_) => {
                    return Err(LocalError::Integrity(format!(
                        "withdrawn {} holds a head guard",
                        self.name()
                    )));
                }
            };
            digest.push(family, &compute_sha256_digest_bytes(&bytes));
            count += 1;
        }
        let commit_id = compute_commit_id(&canonical_commit_bytes_for_hash(
            &self.metadata.author_id,
            &self.metadata.committer_id,
            self.metadata.occurred_at,
            self.metadata.message.as_deref(),
            count,
            &digest.finish(None),
        )?);
        if commit_id != self.commit_id {
            return Err(LocalError::Integrity(format!(
                "withdrawn {} hashes to {commit_id}, not {}",
                self.name(),
                self.commit_id
            )));
        }
        for (id, payload) in &self.metadata_objects {
            let bytes = hex::decode(payload).map_err(|_| {
                LocalError::Integrity(format!("withdrawn {} has unreadable metadata", self.name()))
            })?;
            Metadata::verify_any(id, bytes).map_err(|error| {
                LocalError::Integrity(format!("withdrawn {} metadata {id}: {error}", self.name()))
            })?;
        }
        Ok(())
    }
}

pub(crate) fn read_withdrawn(metadata: &Path) -> Result<WithdrawnFile> {
    let path = metadata.join(WITHDRAWN_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(WithdrawnFile {
            next: 1,
            saves: Vec::new(),
        }),
        Err(source) => Err(LocalError::Io {
            action: "read",
            path,
            source,
        }),
    }
}

fn write_withdrawn(metadata: &Path, file: &WithdrawnFile) -> Result<()> {
    let path = metadata
        .join("tmp")
        .join(format!("withdrawn-{}.tmp", Uuid::new_v4()));
    std::fs::write(&path, serde_json::to_vec(file)?).at("write", &path)?;
    crate::repository::sync_file(&path)?;
    crate::repository::replace_file(&path, &metadata.join(WITHDRAWN_FILE))
        .at("publish withdrawn saves", &path)?;
    sync_directory(metadata)
}

/// Objects of the withdrawn saves in `metadata` that have not expired, for a
/// `gc` of the store they are in.
pub(crate) fn live_withdrawn_objects(metadata: &Path) -> Result<BTreeSet<String>> {
    let now = Utc::now();
    let mut objects = BTreeSet::new();
    for save in read_withdrawn(metadata)?
        .saves
        .iter()
        .filter(|save| save.live(now))
    {
        objects.extend(
            save.objects()?
                .iter()
                .map(|id| id.as_str()[7..].to_string()),
        );
    }
    Ok(objects)
}

/// One withdrawn save named by its label or its complete commit ID.
fn select<'a>(saves: &'a [WithdrawnSave], input: &str) -> Option<&'a WithdrawnSave> {
    saves
        .iter()
        .find(|save| save.name() == input || save.commit_id.as_str() == input)
}

impl Repository {
    /// Withdrawn saves that have not expired, newest first.
    pub fn withdrawn(&self) -> Result<Vec<WithdrawnSave>> {
        let now = Utc::now();
        let mut saves: Vec<WithdrawnSave> = read_withdrawn(self.metadata_path())?
            .saves
            .into_iter()
            .filter(|save| save.live(now))
            .collect();
        saves.reverse();
        Ok(saves)
    }

    pub fn withdrawn_save(&self, input: &str) -> Result<WithdrawnSave> {
        select(&self.withdrawn()?, input).cloned().ok_or_else(|| {
            LocalError::Invalid(format!(
                "no withdrawn save {input}; synck withdrawn lists them"
            ))
        })
    }

    /// The label a commit ID was withdrawn as, for pointing `show` there.
    pub(crate) fn withdrawn_label(&self, commit_id: &str) -> Result<Option<String>> {
        Ok(select(&self.withdrawn()?, commit_id).map(WithdrawnSave::name))
    }

    /// Copy the bytes a withdrawn save gave `input` to a new file.
    pub async fn export_withdrawn(
        &self,
        caller: &Path,
        save: &str,
        input: &Path,
        destination: &Path,
    ) -> Result<()> {
        let save = self.withdrawn_save(save)?;
        let path = normalize_input_path(self.root_path(), caller, input)?;
        let file = save
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or_else(|| {
                LocalError::Invalid(format!(
                    "withdrawn {} did not save the bytes of {path}",
                    save.name()
                ))
            })?;
        let size = i64::try_from(file.size)
            .map_err(|_| LocalError::Integrity("file size overflow".into()))?;
        self.export_object(caller, destination, &file.blob_id, size)
    }

    /// Remove the named withdrawn saves, or all of them, and release their
    /// content. Returns the labels removed.
    pub async fn discard_withdrawn(&self, inputs: &[String], all: bool) -> Result<Vec<String>> {
        let mut file = read_withdrawn(self.metadata_path())?;
        let now = Utc::now();
        let mut chosen = BTreeSet::new();
        for input in inputs {
            let save = select(&file.saves, input)
                .filter(|save| save.live(now))
                .ok_or_else(|| {
                    LocalError::Invalid(format!(
                        "no withdrawn save {input}; synck withdrawn lists them"
                    ))
                })?;
            chosen.insert(save.label);
        }
        let removed: Vec<String> = file
            .saves
            .iter()
            .filter(|save| (all || chosen.contains(&save.label)) && save.live(now))
            .map(WithdrawnSave::name)
            .collect();
        file.saves
            .retain(|save| !(all || chosen.contains(&save.label)));
        self.publish_withdrawn(&file).await?;
        Ok(removed)
    }

    /// Drop the withdrawn saves that have expired. Returns their labels.
    pub(crate) async fn expire_withdrawn(&self) -> Result<Vec<String>> {
        let mut file = read_withdrawn(self.metadata_path())?;
        let now = Utc::now();
        let expired: Vec<String> = file
            .saves
            .iter()
            .filter(|save| !save.live(now))
            .map(WithdrawnSave::name)
            .collect();
        if !expired.is_empty() {
            file.saves.retain(|save| save.live(now));
            self.publish_withdrawn(&file).await?;
        }
        Ok(expired)
    }

    async fn publish_withdrawn(&self, file: &WithdrawnFile) -> Result<()> {
        write_withdrawn(self.metadata_path(), file)?;
        // An HTTPS clone's own `gc` reads the file; the origin's reads the pin.
        if self.remote().is_none() {
            self.pin_held_objects(&[]).await?;
        }
        Ok(())
    }

    /// Pinned until each save expires, in the origin's store.
    pub(super) fn withdrawn_pins(&self) -> Result<Vec<PinnedUntil>> {
        let now = Utc::now();
        read_withdrawn(self.metadata_path())?
            .saves
            .iter()
            .filter(|save| save.live(now))
            .map(|save| {
                Ok(PinnedUntil {
                    until: save.expires_at(),
                    objects: save
                        .objects()?
                        .iter()
                        .map(|id| id.as_str()[7..].to_string())
                        .collect(),
                })
            })
            .collect()
    }

    /// Withdrawn-save records of `commits`, still in this clone's history
    /// copy, not yet labelled.
    pub(super) async fn build_withdrawn(&self, commits: &[String]) -> Result<Vec<WithdrawnSave>> {
        let now = Utc::now();
        let mut saves = Vec::new();
        for commit_id in commits {
            let id = CommitId::parse(commit_id)
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
            let commit = self
                .core()
                .get_commit(self.repository_id().clone(), id.clone())
                .await?
                .ok_or_else(|| {
                    LocalError::Integrity(format!("held commit {commit_id} is missing"))
                })?;
            let (proposal, ref_events) =
                stored_proposal(self.core(), self.repository_id(), &commit).await?;
            let mut metadata_objects = BTreeMap::new();
            for metadata in proposal
                .events
                .iter()
                .filter_map(|event| event.set_metadata.as_ref())
                .chain(
                    ref_events
                        .iter()
                        .filter_map(|event| event.set_metadata.as_ref()),
                )
            {
                let payload = metadata.payload().ok_or_else(|| {
                    LocalError::Integrity(format!(
                        "held commit {commit_id} lacks metadata {}",
                        metadata.id()
                    ))
                })?;
                metadata_objects.insert(metadata.id(), hex::encode(payload.payload()));
            }
            let content: BTreeSet<ItemId> = proposal
                .events
                .iter()
                .filter(|event| synck_file_folder::is_file_content_event(&event.event_type))
                .filter_map(|event| event.item_id.clone())
                .collect();
            let tree = self.historical_tree_at(commit_id).await?.1;
            let files = tree
                .into_iter()
                .filter(|(_, node)| content.contains(&node.item_id))
                .map(|(path, node)| {
                    Ok(WithdrawnFileVersion {
                        path,
                        blob_id: node
                            .blob_id
                            .ok_or_else(|| LocalError::Integrity("FILE has no object".into()))?,
                        size: node
                            .size
                            .and_then(|size| u64::try_from(size).ok())
                            .ok_or_else(|| LocalError::Integrity("FILE has no size".into()))?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let changes = self.show_commit(commit.clone()).await?.changes;
            let save = WithdrawnSave {
                label: 0,
                commit_id: id,
                metadata: CommitMetadata {
                    author_id: commit.author_id.clone(),
                    committer_id: commit.committer_id.clone(),
                    message: commit.message.clone(),
                    occurred_at: commit.occurred_at,
                },
                saved_at: commit.created_at,
                withdrawn_at: now,
                records: proposal
                    .events
                    .into_iter()
                    .map(ProposalRecord::Item)
                    .chain(ref_events.into_iter().map(ProposalRecord::Ref))
                    .collect(),
                metadata_objects,
                changes,
                files,
            };
            save.verify()?;
            saves.push(save);
        }
        Ok(saves)
    }

    /// Label and record `saves`. A commit recorded already keeps its record.
    pub(super) fn add_withdrawn(&self, saves: Vec<WithdrawnSave>) -> Result<Vec<String>> {
        let mut file = read_withdrawn(self.metadata_path())?;
        let mut labels = Vec::new();
        for mut save in saves {
            if let Some(recorded) = select(&file.saves, save.commit_id.as_str()) {
                labels.push(recorded.name());
                continue;
            }
            save.label = file.next;
            file.next += 1;
            labels.push(save.name());
            file.saves.push(save);
        }
        write_withdrawn(self.metadata_path(), &file)?;
        Ok(labels)
    }

    /// Forget the records of `commits`, which are still held: the withdrawal
    /// that wrote them, and labelled them last, did not take them out of the
    /// history. Their labels are free again.
    pub(super) fn forget_withdrawn(&self, commits: &[String]) -> Result<()> {
        let mut file = read_withdrawn(self.metadata_path())?;
        let forgotten: Vec<u64> = file
            .saves
            .iter()
            .filter(|save| commits.iter().any(|id| id == save.commit_id.as_str()))
            .map(|save| save.label)
            .collect();
        let Some(first) = forgotten.iter().min() else {
            return Ok(());
        };
        if forgotten
            .iter()
            .max()
            .is_some_and(|last| last + 1 == file.next)
        {
            file.next = *first;
        }
        file.saves.retain(|save| !forgotten.contains(&save.label));
        write_withdrawn(self.metadata_path(), &file)
    }

    /// Each withdrawn save hashes to its ID and its saved bytes are stored.
    pub(crate) fn verify_withdrawn(&self) -> Result<()> {
        let mut findings = crate::integrity::Findings::default();
        for save in self.withdrawn()? {
            if let Err(error) = save.verify() {
                findings.push(error.to_string());
                continue;
            }
            for file in &save.files {
                if let Err(error) = self.copy_blob(&file.blob_id, file.size, &mut std::io::sink()) {
                    findings.push(format!("withdrawn {} {}: {error}", save.name(), file.path));
                }
            }
        }
        if findings.is_empty() {
            Ok(())
        } else {
            Err(LocalError::Integrity(findings.summary()))
        }
    }
}
