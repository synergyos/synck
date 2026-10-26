use std::collections::BTreeMap;
use std::fs;
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use fastcdc::v2020::{FastCDC, Normalization};
use rawzip::extra_fields::{ExtraFieldId, ExtraFields};
use rawzip::{CompressionMethod, ZipArchive};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use synck_vcs::types::{BlobId, RepoId};

use super::{AVG_CHUNK, ChunkStore, MAX_CHUNK, MIN_CHUNK};
use crate::error::{LocalError, Result};

const MAX_CENTRAL_DIRECTORY: usize = 8 * 1024 * 1024;
const MAX_MEMBERS: usize = u16::MAX as usize;
const COMMON_FLAGS: u16 = 0x0808;
const DEFLATE_OPTION_FLAGS: u16 = 0x0006;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
enum OoxmlKind {
    Docx,
    Xlsx,
    Pptx,
}

impl OoxmlKind {
    fn core_member(self) -> &'static [u8] {
        match self {
            Self::Docx => b"word/document.xml",
            Self::Xlsx => b"xl/workbook.xml",
            Self::Pptx => b"ppt/presentation.xml",
        }
    }

    fn production(self) -> crate::ooxml::OoxmlKind {
        match self {
            Self::Docx => crate::ooxml::OoxmlKind::Docx,
            Self::Xlsx => crate::ooxml::OoxmlKind::Xlsx,
            Self::Pptx => crate::ooxml::OoxmlKind::Pptx,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SectionKind {
    LocalEntry,
    CentralDirectory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ArchiveSection {
    kind: SectionKind,
    member_name: Option<String>,
    start: usize,
    payload_start: Option<usize>,
    payload_end: Option<usize>,
    descriptor_end: Option<usize>,
    end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct SectionResult {
    kind: SectionKind,
    member_name: Option<String>,
    start: usize,
    payload_start: Option<usize>,
    payload_end: Option<usize>,
    descriptor_end: Option<usize>,
    end: usize,
    sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RegionKind {
    LargeMember,
    SmallMemberRun,
    CentralDirectory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ChunkRegion {
    kind: RegionKind,
    member_names: Vec<String>,
    start: usize,
    end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ChunkResult {
    start: usize,
    end: usize,
    sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RegionResult {
    kind: RegionKind,
    member_names: Vec<String>,
    start: usize,
    end: usize,
    sha256: String,
    chunk_count: usize,
    chunk_bytes: usize,
    chunks: Vec<ChunkResult>,
}

#[derive(Debug)]
struct OoxmlLayout {
    sections: Vec<ArchiveSection>,
    regions: Vec<ChunkRegion>,
    central_directory_bytes: usize,
    member_count: usize,
    layout_owned_bytes: usize,
}

fn ooxml_layout(bytes: &[u8], kind: OoxmlKind) -> Option<OoxmlLayout> {
    let archive = ZipArchive::from_slice(bytes).ok()?;
    let member_hint = usize::try_from(archive.entries_hint()).ok()?;
    let central_offset = usize::try_from(archive.directory_offset()).ok()?;
    let eocd_offset = usize::try_from(archive.eocd_offset()).ok()?;
    let archive_end = usize::try_from(archive.end_offset()).ok()?;
    let central_directory_bytes = eocd_offset.checked_sub(central_offset)?;
    let disk = zip_u16(bytes, eocd_offset.checked_add(4)?)?;
    let central_disk = zip_u16(bytes, eocd_offset.checked_add(6)?)?;
    let entries_on_disk = usize::from(zip_u16(bytes, eocd_offset.checked_add(8)?)?);
    let entries_total = usize::from(zip_u16(bytes, eocd_offset.checked_add(10)?)?);
    let declared_central_size =
        usize::try_from(zip_u32(bytes, eocd_offset.checked_add(12)?)?).ok()?;
    let declared_central_offset =
        usize::try_from(zip_u32(bytes, eocd_offset.checked_add(16)?)?).ok()?;
    if disk != 0
        || central_disk != 0
        || entries_on_disk != entries_total
        || entries_total != member_hint
        || declared_central_size != central_directory_bytes
        || declared_central_offset != central_offset
        || member_hint == 0
        || member_hint > MAX_MEMBERS
        || central_offset == 0
        || central_directory_bytes > MAX_CENTRAL_DIRECTORY
        || archive_end != bytes.len()
    {
        return None;
    }

    let mut entries = Vec::with_capacity(member_hint);
    let mut names = std::collections::BTreeSet::new();
    let mut content_types = 0_usize;
    let mut core_members = 0_usize;
    for result in archive.entries() {
        let entry = result.ok()?;
        let flags = entry.flags();
        let method = entry.compression_method();
        let allowed_flags = match method {
            CompressionMethod::STORE => COMMON_FLAGS,
            CompressionMethod::DEFLATE => COMMON_FLAGS | DEFLATE_OPTION_FLAGS,
            _ => return None,
        };
        if flags.bits() & !allowed_flags != 0
            || flags.is_encrypted()
            || flags.has_strong_encryption()
            || flags.is_masked()
            || !supported_extra_fields(entry.extra_fields())
        {
            return None;
        }
        let file_path = entry.file_path();
        let raw_name: &[u8] = file_path.as_ref();
        if raw_name.is_empty() || !names.insert(raw_name.to_vec()) {
            return None;
        }
        let member_name = std::str::from_utf8(raw_name).ok()?.to_string();
        content_types += usize::from(raw_name == b"[Content_Types].xml");
        core_members += usize::from(raw_name == kind.core_member());

        let local_entry = archive.get_entry(entry.wayfinder()).ok()?;
        let local_header = local_entry.local_header();
        if local_header.file_path().as_ref() != raw_name
            || local_header.flags() != flags
            || local_header.compression_method() != method
            || !supported_extra_fields(local_header.extra_fields())
        {
            return None;
        }
        let (payload_start, payload_end) = local_entry.compressed_data_range();
        let payload_start = usize::try_from(payload_start).ok()?;
        let payload_end = usize::try_from(payload_end).ok()?;
        if local_entry.data() != bytes.get(payload_start..payload_end)? {
            return None;
        }
        let descriptor_end = if flags.has_data_descriptor() {
            if local_header.crc32() != 0
                || local_header.compressed_size_hint() != 0
                || local_header.uncompressed_size_hint() != 0
            {
                return None;
            }
            let descriptor = local_entry.data_descriptor().ok()??;
            if descriptor.crc32() != entry.crc32()
                || descriptor.compressed_size() != entry.compressed_size_hint()
                || descriptor.uncompressed_size() != entry.uncompressed_size_hint()
            {
                return None;
            }
            let signature_bytes = 0x0807_4b50_u32.to_le_bytes();
            let has_signature =
                bytes.get(payload_end..payload_end.checked_add(4)?)? == signature_bytes.as_slice();
            Some(payload_end.checked_add(if has_signature { 16 } else { 12 })?)
        } else {
            if local_header.crc32() != entry.crc32()
                || local_header.compressed_size_hint() != entry.compressed_size_hint()
                || local_header.uncompressed_size_hint() != entry.uncompressed_size_hint()
                || local_entry.data_descriptor().ok()?.is_some()
            {
                return None;
            }
            None
        };
        entries.push(ArchiveSection {
            kind: SectionKind::LocalEntry,
            member_name: Some(member_name),
            start: usize::try_from(entry.local_header_offset()).ok()?,
            payload_start: Some(payload_start),
            payload_end: Some(payload_end),
            descriptor_end,
            end: 0,
        });
    }
    if entries.len() != member_hint || content_types != 1 || core_members != 1 {
        return None;
    }

    entries.sort_unstable_by_key(|entry| entry.start);
    if entries.first()?.start != 0 {
        return None;
    }
    for index in 0..entries.len() {
        let section_end = entries
            .get(index + 1)
            .map_or(central_offset, |entry| entry.start);
        let entry = &mut entries[index];
        let payload_start = entry.payload_start?;
        let payload_end = entry.payload_end?;
        let framing_end = entry.descriptor_end.unwrap_or(payload_end);
        if entry.start >= section_end
            || payload_start < entry.start
            || payload_start > payload_end
            || payload_end > framing_end
            || framing_end > section_end
        {
            return None;
        }
        entry.end = section_end;
    }
    entries.push(ArchiveSection {
        kind: SectionKind::CentralDirectory,
        member_name: None,
        start: central_offset,
        payload_start: None,
        payload_end: None,
        descriptor_end: None,
        end: bytes.len(),
    });
    if !sections_cover_source(&entries, bytes.len()) {
        return None;
    }
    let regions = chunk_regions(&entries)?;
    if !regions_cover_source(&regions, bytes.len()) {
        return None;
    }
    let layout_owned_bytes = entries
        .capacity()
        .checked_mul(size_of::<ArchiveSection>())?
        .checked_add(
            entries
                .iter()
                .filter_map(|entry| entry.member_name.as_ref())
                .map(String::capacity)
                .try_fold(0_usize, usize::checked_add)?,
        )?
        .checked_add(regions.capacity().checked_mul(size_of::<ChunkRegion>())?)?
        .checked_add(
            regions
                .iter()
                .flat_map(|region| &region.member_names)
                .map(String::capacity)
                .try_fold(0_usize, usize::checked_add)?,
        )?;
    Some(OoxmlLayout {
        member_count: entries.len() - 1,
        sections: entries,
        regions,
        central_directory_bytes,
        layout_owned_bytes,
    })
}

fn chunk_regions(sections: &[ArchiveSection]) -> Option<Vec<ChunkRegion>> {
    let (directory, locals) = sections.split_last()?;
    if directory.kind != SectionKind::CentralDirectory {
        return None;
    }
    let mut regions = Vec::new();
    let mut small: Option<ChunkRegion> = None;
    for section in locals {
        let member_name = section.member_name.clone()?;
        if section.end.checked_sub(section.start)? >= MIN_CHUNK as usize {
            if let Some(region) = small.take() {
                regions.push(region);
            }
            regions.push(ChunkRegion {
                kind: RegionKind::LargeMember,
                member_names: vec![member_name],
                start: section.start,
                end: section.end,
            });
        } else if let Some(region) = &mut small {
            if region.end != section.start {
                return None;
            }
            region.member_names.push(member_name);
            region.end = section.end;
        } else {
            small = Some(ChunkRegion {
                kind: RegionKind::SmallMemberRun,
                member_names: vec![member_name],
                start: section.start,
                end: section.end,
            });
        }
    }
    if let Some(region) = small {
        regions.push(region);
    }
    regions.push(ChunkRegion {
        kind: RegionKind::CentralDirectory,
        member_names: Vec::new(),
        start: directory.start,
        end: directory.end,
    });
    Some(regions)
}

fn zip_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn zip_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn supported_extra_fields(mut fields: ExtraFields<'_>) -> bool {
    if fields.by_ref().any(|(kind, _)| kind == ExtraFieldId::ZIP64) {
        return false;
    }
    fields.remaining_bytes().is_empty()
}

fn sections_cover_source(sections: &[ArchiveSection], source_size: usize) -> bool {
    let mut next = 0_usize;
    for section in sections {
        if section.start != next || section.start >= section.end || section.end > source_size {
            return false;
        }
        next = section.end;
    }
    next == source_size
}

fn section_results(bytes: &[u8], sections: &[ArchiveSection]) -> Vec<SectionResult> {
    sections
        .iter()
        .map(|section| SectionResult {
            kind: section.kind,
            member_name: section.member_name.clone(),
            start: section.start,
            payload_start: section.payload_start,
            payload_end: section.payload_end,
            descriptor_end: section.descriptor_end,
            end: section.end,
            sha256: hex::encode(Sha256::digest(&bytes[section.start..section.end])),
        })
        .collect()
}

fn regions_cover_source(regions: &[ChunkRegion], source_size: usize) -> bool {
    let mut next = 0_usize;
    for region in regions {
        if region.start != next || region.start >= region.end || region.end > source_size {
            return false;
        }
        next = region.end;
    }
    next == source_size
}

fn region_results_cover_source(regions: &[RegionResult], source_size: usize) -> bool {
    let mut next = 0_usize;
    for region in regions {
        if region.start != next
            || region.start >= region.end
            || region.end > source_size
            || region.chunk_count == 0
            || region.chunk_bytes != region.end - region.start
            || region.chunks.first().map(|chunk| chunk.start) != Some(region.start)
            || region.chunks.last().map(|chunk| chunk.end) != Some(region.end)
            || region
                .chunks
                .windows(2)
                .any(|chunks| chunks[0].end != chunks[1].start)
        {
            return false;
        }
        next = region.end;
    }
    next == source_size
}

struct RegionChunker<'a> {
    bytes: &'a [u8],
    regions: &'a [ChunkRegion],
    region_index: usize,
    position: usize,
}

impl<'a> RegionChunker<'a> {
    fn new(bytes: &'a [u8], regions: &'a [ChunkRegion]) -> Self {
        Self {
            bytes,
            regions,
            region_index: 0,
            position: 0,
        }
    }

    fn next_chunk(&mut self) -> Option<Vec<u8>> {
        while self
            .regions
            .get(self.region_index)
            .is_some_and(|region| self.position == region.end)
        {
            self.region_index += 1;
        }
        let region = self.regions.get(self.region_index)?;
        if self.position != region.start && self.position >= region.end {
            return None;
        }
        let chunk = FastCDC::with_level(
            &self.bytes[self.position..region.end],
            MIN_CHUNK,
            AVG_CHUNK,
            MAX_CHUNK as u32,
            Normalization::Level1,
        )
        .next()
        .expect("a nonempty section remainder emits one chunk");
        let start = self.position;
        self.position = self
            .position
            .checked_add(chunk.length)
            .expect("chunk end fits in source range");
        Some(self.bytes[start..self.position].to_vec())
    }
}

fn region_results(bytes: &[u8], regions: &[ChunkRegion]) -> Vec<RegionResult> {
    regions
        .iter()
        .map(|region| {
            let mut position = region.start;
            let mut chunks = Vec::new();
            while position < region.end {
                let chunk = FastCDC::with_level(
                    &bytes[position..region.end],
                    MIN_CHUNK,
                    AVG_CHUNK,
                    MAX_CHUNK as u32,
                    Normalization::Level1,
                )
                .next()
                .expect("a nonempty region remainder emits one chunk");
                let end = position
                    .checked_add(chunk.length)
                    .expect("chunk end fits in source range");
                chunks.push(ChunkResult {
                    start: position,
                    end,
                    sha256: hex::encode(Sha256::digest(&bytes[position..end])),
                });
                position = end;
            }
            RegionResult {
                kind: region.kind,
                member_names: region.member_names.clone(),
                start: region.start,
                end: region.end,
                sha256: hex::encode(Sha256::digest(&bytes[region.start..region.end])),
                chunk_count: chunks.len(),
                chunk_bytes: region.end - region.start,
                chunks,
            }
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct StoreMetrics {
    new_raw_chunk_bytes: u64,
    new_stored_payload_bytes: u64,
    new_distinct_chunks: u64,
    new_mapping_rows: u64,
    cumulative_stored_payload_bytes: u64,
    cumulative_page_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
struct VersionResult {
    path: String,
    scenario: String,
    size_bytes: u64,
    sha256: String,
    metrics: StoreMetrics,
    median_elapsed_ns: u64,
    median_validator_ns: u64,
    central_directory_bytes: usize,
    member_count: usize,
    candidate_layout_owned_bytes: usize,
    sections: Vec<SectionResult>,
    chunk_regions: Vec<RegionResult>,
}

#[derive(Debug, Deserialize)]
struct CorpusManifest {
    schema_version: u64,
    source_os: String,
    office_version: String,
    sequences: Vec<SequenceSpec>,
}

#[derive(Debug, Deserialize)]
struct SequenceSpec {
    name: String,
    format: OoxmlKind,
    versions: Vec<VersionSpec>,
}

#[derive(Debug, Deserialize)]
struct VersionSpec {
    path: String,
    scenario: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, Serialize)]
struct SequenceResult {
    name: String,
    format: OoxmlKind,
    generic: Vec<VersionResult>,
    candidate: Vec<VersionResult>,
}

#[derive(Debug, Serialize)]
struct BenchmarkResult {
    schema_version: u64,
    git_commit: String,
    runtime_os: String,
    runtime_arch: String,
    cpu: String,
    memory: String,
    filesystem: String,
    source_os: String,
    office_version: String,
    corpus_manifest_sha256: String,
    repetitions: usize,
    invocation: String,
    summaries: Vec<FormatSummary>,
    sequences: Vec<SequenceResult>,
}

#[derive(Debug, Serialize)]
struct FormatSummary {
    format: OoxmlKind,
    measured_transitions: usize,
    generic_incremental_payload_bytes: u64,
    candidate_incremental_payload_bytes: u64,
    aggregate_saving_basis_points: i64,
    median_transition_saving_basis_points: i64,
    generic_mapping_rows: u64,
    candidate_mapping_rows: u64,
    median_time_saving_basis_points: i64,
    materially_worse_transitions: usize,
    maximum_candidate_layout_owned_bytes: usize,
}

#[derive(Clone, Copy)]
enum Policy {
    Generic,
    Candidate(OoxmlKind),
}

struct MeasuredVersion {
    result: VersionResult,
    elapsed: Duration,
    validator_elapsed: Duration,
}

fn run_policy(
    corpus_root: &Path,
    versions: &[VersionSpec],
    policy: Policy,
) -> Result<Vec<MeasuredVersion>> {
    let temporary = tempfile::tempdir().map_err(|source| LocalError::Io {
        action: "create OOXML benchmark directory",
        path: PathBuf::from("<temporary directory>"),
        source,
    })?;
    let store_path = temporary.path().join("objects.sqlite3");
    let repo_id = RepoId::new();
    ChunkStore::create(&store_path, &repo_id)?;
    let store = ChunkStore::new(store_path, &repo_id);
    let mut results = Vec::with_capacity(versions.len());
    for version in versions {
        let path = checked_corpus_path(corpus_root, &version.path)?;
        let bytes = fs::read(&path).map_err(|source| LocalError::Io {
            action: "read OOXML benchmark file",
            path: path.clone(),
            source,
        })?;
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if bytes.len() as u64 != version.size_bytes || hex::encode(digest) != version.sha256 {
            return Err(LocalError::Invalid(format!(
                "OOXML benchmark manifest mismatch: {}",
                version.path
            )));
        }
        let before = store_metrics(&store)?;
        let started = Instant::now();
        let validator_started = Instant::now();
        let production_regions = match policy {
            Policy::Generic => None,
            Policy::Candidate(kind) => Some(
                crate::ooxml::chunk_region_lengths(
                    fs::File::open(&path).map_err(|source| LocalError::Io {
                        action: "open OOXML benchmark file",
                        path: path.clone(),
                        source,
                    })?,
                    bytes.len() as u64,
                    kind.production(),
                )
                .ok_or_else(|| {
                    LocalError::Invalid(format!(
                        "OOXML benchmark package is ineligible: {}",
                        version.path
                    ))
                })?,
            ),
        };
        let validator_elapsed = validator_started.elapsed();
        let complete = store.contains_complete(&digest, bytes.len() as u64)?;
        match &production_regions {
            None => {
                if !complete {
                    store.stage_blob(digest, bytes.len() as u64, bytes.as_slice())?;
                }
            }
            Some(regions) => {
                if !complete {
                    store.stage_blob_regions(
                        digest,
                        bytes.len() as u64,
                        bytes.as_slice(),
                        regions,
                    )?;
                }
            }
        }
        if !complete {
            store.complete_blob(&digest, bytes.len() as u64)?;
        }
        let elapsed = started.elapsed();
        let layout = match policy {
            Policy::Generic => None,
            Policy::Candidate(kind) => Some(ooxml_layout(&bytes, kind).ok_or_else(|| {
                LocalError::Invalid(format!(
                    "OOXML diagnostic layout rejected production-eligible package: {}",
                    version.path
                ))
            })?),
        };
        if let (Some(regions), Some(layout)) = (&production_regions, &layout) {
            let diagnostic = layout
                .regions
                .iter()
                .map(|region| (region.end - region.start) as u64)
                .collect::<Vec<_>>();
            if *regions != diagnostic {
                return Err(LocalError::Integrity(format!(
                    "OOXML production and diagnostic regions differ: {}",
                    version.path
                )));
            }
        }
        let (sections, chunk_regions) = layout
            .as_ref()
            .map(|layout| {
                (
                    section_results(&bytes, &layout.sections),
                    region_results(&bytes, &layout.regions),
                )
            })
            .unwrap_or_default();
        if !chunk_regions.is_empty() && !region_results_cover_source(&chunk_regions, bytes.len()) {
            return Err(LocalError::Integrity(format!(
                "OOXML benchmark chunk regions do not cover source: {}",
                version.path
            )));
        }
        let blob_id = BlobId::parse(format!("sha256:{}", version.sha256))
            .map_err(|error| LocalError::Invalid(error.to_string()))?;
        let mut reconstructed = Vec::with_capacity(bytes.len());
        store.copy_blob(&blob_id, bytes.len() as u64, &mut reconstructed)?;
        if reconstructed != bytes {
            return Err(LocalError::Integrity(format!(
                "OOXML benchmark reconstruction differs: {}",
                version.path
            )));
        }
        let after = store_metrics(&store)?;
        let metrics = StoreMetrics {
            new_raw_chunk_bytes: after
                .new_raw_chunk_bytes
                .checked_sub(before.new_raw_chunk_bytes)
                .ok_or_else(|| LocalError::Integrity("chunk bytes decreased".to_string()))?,
            new_stored_payload_bytes: after
                .new_stored_payload_bytes
                .checked_sub(before.new_stored_payload_bytes)
                .ok_or_else(|| LocalError::Integrity("payload bytes decreased".to_string()))?,
            new_distinct_chunks: after
                .new_distinct_chunks
                .checked_sub(before.new_distinct_chunks)
                .ok_or_else(|| LocalError::Integrity("chunk count decreased".to_string()))?,
            new_mapping_rows: after
                .new_mapping_rows
                .checked_sub(before.new_mapping_rows)
                .ok_or_else(|| LocalError::Integrity("mapping count decreased".to_string()))?,
            cumulative_stored_payload_bytes: after.cumulative_stored_payload_bytes,
            cumulative_page_bytes: after.cumulative_page_bytes,
        };
        let (central_directory_bytes, member_count, candidate_layout_owned_bytes) = layout
            .map(|layout| {
                (
                    layout.central_directory_bytes,
                    layout.member_count,
                    layout.layout_owned_bytes,
                )
            })
            .unwrap_or_default();
        results.push(MeasuredVersion {
            result: VersionResult {
                path: version.path.clone(),
                scenario: version.scenario.clone(),
                size_bytes: bytes.len() as u64,
                sha256: version.sha256.clone(),
                metrics,
                median_elapsed_ns: 0,
                median_validator_ns: 0,
                central_directory_bytes,
                member_count,
                candidate_layout_owned_bytes,
                sections,
                chunk_regions,
            },
            elapsed,
            validator_elapsed,
        });
    }
    Ok(results)
}

fn store_metrics(store: &ChunkStore) -> Result<StoreMetrics> {
    let connection = store.open_readonly()?;
    let (chunks, raw, payload): (i64, i64, i64) = connection.query_row(
        "SELECT count(*), coalesce(sum(raw_size), 0), coalesce(sum(length(payload)), 0)
         FROM chunks",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let mappings: i64 =
        connection.query_row("SELECT count(*) FROM blob_chunks", [], |row| row.get(0))?;
    let page_count: i64 = connection.pragma_query_value(None, "page_count", |row| row.get(0))?;
    let page_size: i64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
    Ok(StoreMetrics {
        new_raw_chunk_bytes: u64::try_from(raw)
            .map_err(|_| LocalError::Integrity("negative chunk bytes".to_string()))?,
        new_stored_payload_bytes: u64::try_from(payload)
            .map_err(|_| LocalError::Integrity("negative payload bytes".to_string()))?,
        new_distinct_chunks: u64::try_from(chunks)
            .map_err(|_| LocalError::Integrity("negative chunk count".to_string()))?,
        new_mapping_rows: u64::try_from(mappings)
            .map_err(|_| LocalError::Integrity("negative mapping count".to_string()))?,
        cumulative_stored_payload_bytes: u64::try_from(payload)
            .map_err(|_| LocalError::Integrity("negative payload bytes".to_string()))?,
        cumulative_page_bytes: u64::try_from(
            page_count
                .checked_mul(page_size)
                .ok_or_else(|| LocalError::Integrity("page byte overflow".to_string()))?,
        )
        .map_err(|_| LocalError::Integrity("negative page bytes".to_string()))?,
    })
}

fn checked_corpus_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(LocalError::Invalid(format!(
            "invalid OOXML benchmark path: {}",
            relative.display()
        )));
    }
    Ok(root.join(relative))
}

fn median(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        let sum = u128::from(values[middle - 1]) + u128::from(values[middle]);
        u64::try_from(sum / 2).expect("median fits in u64")
    }
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).expect("one benchmark run fits in u64 nanoseconds")
}

fn median_i64(values: &mut [i64]) -> i64 {
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        let sum = i128::from(values[middle - 1]) + i128::from(values[middle]);
        i64::try_from(sum / 2).expect("median fits in i64")
    }
}

fn saving_basis_points(generic: u64, candidate: u64) -> Option<i64> {
    if generic == 0 {
        return None;
    }
    let saving = i128::from(generic) - i128::from(candidate);
    let value = saving.checked_mul(10_000)? / i128::from(generic);
    i64::try_from(value).ok()
}

fn checked_sum(mut values: impl Iterator<Item = u64>) -> u64 {
    values
        .try_fold(0_u64, u64::checked_add)
        .expect("benchmark aggregate fits in u64")
}

fn summarize(sequences: &[SequenceResult]) -> Vec<FormatSummary> {
    let mut by_format = BTreeMap::<OoxmlKind, Vec<(&VersionResult, &VersionResult)>>::new();
    for sequence in sequences {
        assert_eq!(sequence.generic.len(), sequence.candidate.len());
        for index in 1..sequence.generic.len() {
            by_format
                .entry(sequence.format)
                .or_default()
                .push((&sequence.generic[index], &sequence.candidate[index]));
        }
    }
    by_format
        .into_iter()
        .map(|(format, transitions)| {
            let generic_payload = checked_sum(
                transitions
                    .iter()
                    .map(|(generic, _)| generic.metrics.new_stored_payload_bytes),
            );
            let candidate_payload = checked_sum(
                transitions
                    .iter()
                    .map(|(_, candidate)| candidate.metrics.new_stored_payload_bytes),
            );
            let generic_rows = checked_sum(
                transitions
                    .iter()
                    .map(|(generic, _)| generic.metrics.new_mapping_rows),
            );
            let candidate_rows = checked_sum(
                transitions
                    .iter()
                    .map(|(_, candidate)| candidate.metrics.new_mapping_rows),
            );
            let mut transition_savings = transitions
                .iter()
                .filter_map(|(generic, candidate)| {
                    saving_basis_points(
                        generic.metrics.new_stored_payload_bytes,
                        candidate.metrics.new_stored_payload_bytes,
                    )
                })
                .collect::<Vec<_>>();
            let mut time_savings = transitions
                .iter()
                .filter_map(|(generic, candidate)| {
                    saving_basis_points(generic.median_elapsed_ns, candidate.median_elapsed_ns)
                })
                .collect::<Vec<_>>();
            let aggregate_saving =
                saving_basis_points(generic_payload, candidate_payload).unwrap_or_default();
            let median_transition_saving = if transition_savings.is_empty() {
                0
            } else {
                median_i64(&mut transition_savings)
            };
            let median_time_saving = if time_savings.is_empty() {
                0
            } else {
                median_i64(&mut time_savings)
            };
            let materially_worse = transitions
                .iter()
                .filter(|(generic, candidate)| {
                    let generic = generic.metrics.new_stored_payload_bytes;
                    let candidate = candidate.metrics.new_stored_payload_bytes;
                    candidate > generic.saturating_add(64 * 1024)
                        && u128::from(candidate) * 100 > u128::from(generic) * 105
                })
                .count();
            let maximum_owned = transitions
                .iter()
                .map(|(_, candidate)| candidate.candidate_layout_owned_bytes)
                .max()
                .unwrap_or_default();
            FormatSummary {
                format,
                measured_transitions: transitions.len(),
                generic_incremental_payload_bytes: generic_payload,
                candidate_incremental_payload_bytes: candidate_payload,
                aggregate_saving_basis_points: aggregate_saving,
                median_transition_saving_basis_points: median_transition_saving,
                generic_mapping_rows: generic_rows,
                candidate_mapping_rows: candidate_rows,
                median_time_saving_basis_points: median_time_saving,
                materially_worse_transitions: materially_worse,
                maximum_candidate_layout_owned_bytes: maximum_owned,
            }
        })
        .collect()
}

fn command_stdout(program: &str, arguments: &[&str]) -> String {
    Command::new(program)
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "UNCONFIRMED".to_string())
}

fn clean_git_commit() -> String {
    let status = Command::new("git")
        .args(["status", "--porcelain=v1", "--untracked-files=all"])
        .output()
        .expect("run git status for OOXML benchmark");
    assert!(status.status.success(), "git status failed");
    assert!(
        status.stdout.is_empty(),
        "OOXML release evidence requires a clean Git worktree"
    );
    let commit = command_stdout("git", &["rev-parse", "HEAD"]);
    assert_ne!(
        commit, "UNCONFIRMED",
        "cannot identify benchmark Git commit"
    );
    commit
}

const MAX_ZIP_COMMENT: usize = u16::MAX as usize;
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const DATA_DESCRIPTOR_SIGNATURE: u32 = 0x0807_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const EOCD_SIGNATURE: u32 = 0x0605_4b50;
const CENTRAL_HEADER_SIZE: usize = 46;

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn set_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn set_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn test_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn last_signature(bytes: &[u8], signature: u32) -> usize {
    let signature = signature.to_le_bytes();
    bytes
        .windows(signature.len())
        .rposition(|window| window == signature)
        .unwrap()
}

fn build_stored_zip(entries: &[(&[u8], &[u8])], comment: &[u8]) -> Vec<u8> {
    build_stored_zip_with_descriptors(entries, comment, false)
}

fn build_stored_zip_with_descriptors(
    entries: &[(&[u8], &[u8])],
    comment: &[u8],
    descriptors: bool,
) -> Vec<u8> {
    assert!(entries.len() <= MAX_MEMBERS);
    assert!(comment.len() <= MAX_ZIP_COMMENT);
    let mut bytes = Vec::new();
    let mut central = Vec::new();
    for (name, payload) in entries {
        let local_offset = u32::try_from(bytes.len()).unwrap();
        let name_length = u16::try_from(name.len()).unwrap();
        let payload_size = u32::try_from(payload.len()).unwrap();
        let flags = if descriptors { 0x0008 } else { 0 };
        push_u32(&mut bytes, LOCAL_SIGNATURE);
        push_u16(&mut bytes, 20);
        push_u16(&mut bytes, flags);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u32(&mut bytes, 0);
        push_u32(&mut bytes, if descriptors { 0 } else { payload_size });
        push_u32(&mut bytes, if descriptors { 0 } else { payload_size });
        push_u16(&mut bytes, name_length);
        push_u16(&mut bytes, 0);
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(payload);
        if descriptors {
            push_u32(&mut bytes, DATA_DESCRIPTOR_SIGNATURE);
            push_u32(&mut bytes, 0);
            push_u32(&mut bytes, payload_size);
            push_u32(&mut bytes, payload_size);
        }

        push_u32(&mut central, CENTRAL_SIGNATURE);
        push_u16(&mut central, 20);
        push_u16(&mut central, 20);
        push_u16(&mut central, flags);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, 0);
        push_u32(&mut central, payload_size);
        push_u32(&mut central, payload_size);
        push_u16(&mut central, name_length);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, 0);
        push_u32(&mut central, local_offset);
        central.extend_from_slice(name);
    }
    let central_offset = u32::try_from(bytes.len()).unwrap();
    let central_size = u32::try_from(central.len()).unwrap();
    bytes.extend_from_slice(&central);
    push_u32(&mut bytes, EOCD_SIGNATURE);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, entries.len() as u16);
    push_u16(&mut bytes, entries.len() as u16);
    push_u32(&mut bytes, central_size);
    push_u32(&mut bytes, central_offset);
    push_u16(&mut bytes, comment.len() as u16);
    bytes.extend_from_slice(comment);
    bytes
}

#[test]
fn ooxml_layout_accepts_real_empty_office_packages() {
    let fixtures = [
        (
            OoxmlKind::Docx,
            include_bytes!("../../assets/templates/empty.docx").as_slice(),
        ),
        (
            OoxmlKind::Xlsx,
            include_bytes!("../../assets/templates/empty.xlsx").as_slice(),
        ),
        (
            OoxmlKind::Pptx,
            include_bytes!("../../assets/templates/empty.pptx").as_slice(),
        ),
    ];
    for (kind, bytes) in fixtures {
        let layout = ooxml_layout(bytes, kind).expect("valid Office package");
        assert!(layout.member_count > 1);
        assert!(layout.central_directory_bytes <= MAX_CENTRAL_DIRECTORY);
        assert_eq!(layout.sections.len(), layout.member_count + 1);
        assert!(sections_cover_source(&layout.sections, bytes.len()));
        assert!(regions_cover_source(&layout.regions, bytes.len()));
        let mut chunker = RegionChunker::new(bytes, &layout.regions);
        let mut reconstructed = Vec::new();
        while let Some(chunk) = chunker.next_chunk() {
            assert!(chunk.len() <= MAX_CHUNK);
            reconstructed.extend_from_slice(&chunk);
        }
        let results = region_results(bytes, &layout.regions);
        assert!(region_results_cover_source(&results, bytes.len()));
        assert_eq!(reconstructed, bytes);
    }
}

fn production_region_lengths(bytes: &[u8], kind: OoxmlKind) -> Option<Vec<u64>> {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("package");
    fs::write(&path, bytes).unwrap();
    crate::ooxml::chunk_region_lengths(
        fs::File::open(path).unwrap(),
        bytes.len() as u64,
        kind.production(),
    )
}

fn assert_rejected_by_both(bytes: &[u8], kind: OoxmlKind) {
    assert!(ooxml_layout(bytes, kind).is_none());
    assert!(production_region_lengths(bytes, kind).is_none());
}

#[test]
fn production_regions_match_the_benchmark_policy() {
    let fixtures = [
        (
            OoxmlKind::Docx,
            include_bytes!("../../assets/templates/empty.docx").as_slice(),
        ),
        (
            OoxmlKind::Xlsx,
            include_bytes!("../../assets/templates/empty.xlsx").as_slice(),
        ),
        (
            OoxmlKind::Pptx,
            include_bytes!("../../assets/templates/empty.pptx").as_slice(),
        ),
    ];
    for (kind, bytes) in fixtures {
        let production = production_region_lengths(bytes, kind).unwrap();
        let benchmark = ooxml_layout(bytes, kind)
            .unwrap()
            .regions
            .into_iter()
            .map(|region| (region.end - region.start) as u64)
            .collect::<Vec<_>>();
        assert_eq!(production, benchmark, "{kind:?}");
    }
}

#[test]
fn ooxml_layout_requires_matching_package_kind() {
    let docx = include_bytes!("../../assets/templates/empty.docx");
    assert!(ooxml_layout(docx, OoxmlKind::Docx).is_some());
    assert!(ooxml_layout(docx, OoxmlKind::Xlsx).is_none());
    assert!(ooxml_layout(docx, OoxmlKind::Pptx).is_none());
}

#[test]
fn ooxml_layout_rejects_truncation_and_encryption() {
    let docx = include_bytes!("../../assets/templates/empty.docx");
    for removed in [1, 22, docx.len() / 2] {
        assert_rejected_by_both(&docx[..docx.len() - removed], OoxmlKind::Docx);
    }

    let mut encrypted = docx.to_vec();
    let eocd = last_signature(&encrypted, EOCD_SIGNATURE);
    let central = test_u32(&encrypted, eocd + 16) as usize;
    encrypted[central + 8] |= 1;
    assert_rejected_by_both(&encrypted, OoxmlKind::Docx);
}

#[test]
fn rawzip_sections_cover_every_source_byte_and_own_their_payloads() {
    let large = vec![b'x'; 2 * MAX_CHUNK + 17];
    let valid = build_stored_zip(
        &[
            (b"[Content_Types].xml", b"types"),
            (b"word/document.xml", &large),
            (b"docProps/core.xml", b"properties"),
        ],
        b"ordinary comment",
    );
    let layout = ooxml_layout(&valid, OoxmlKind::Docx).expect("valid synthetic package");
    assert_eq!(layout.member_count, 3);
    assert_eq!(layout.sections.len(), 4);
    assert!(sections_cover_source(&layout.sections, valid.len()));
    assert_eq!(layout.regions.len(), 4);
    assert!(regions_cover_source(&layout.regions, valid.len()));
    assert_eq!(layout.regions[0].kind, RegionKind::SmallMemberRun);
    assert_eq!(layout.regions[1].kind, RegionKind::LargeMember);
    assert_eq!(layout.regions[2].kind, RegionKind::SmallMemberRun);
    assert_eq!(layout.regions[3].kind, RegionKind::CentralDirectory);
    assert_eq!(
        layout.sections.last().unwrap().kind,
        SectionKind::CentralDirectory
    );

    let archive = ZipArchive::from_slice(&valid).unwrap();
    for result in archive.entries() {
        let entry = result.unwrap();
        let file_path = entry.file_path();
        let name = std::str::from_utf8(file_path.as_ref()).unwrap();
        let rawzip_range = archive
            .get_entry(entry.wayfinder())
            .unwrap()
            .compressed_data_range();
        let section = layout
            .sections
            .iter()
            .find(|section| section.member_name.as_deref() == Some(name))
            .unwrap();
        assert_eq!(section.payload_start, Some(rawzip_range.0 as usize));
        assert_eq!(section.payload_end, Some(rawzip_range.1 as usize));
        assert!(section.start <= rawzip_range.0 as usize);
        assert!(rawzip_range.1 as usize <= section.end);
    }

    let mut chunker = RegionChunker::new(&valid, &layout.regions);
    let mut reconstructed = Vec::new();
    while let Some(chunk) = chunker.next_chunk() {
        reconstructed.extend_from_slice(&chunk);
        assert!(chunk.len() <= MAX_CHUNK);
        if chunk.len() < MIN_CHUNK as usize {
            assert!(
                layout
                    .regions
                    .iter()
                    .any(|region| region.end == reconstructed.len())
            );
        }
    }
    let results = region_results(&valid, &layout.regions);
    assert!(region_results_cover_source(&results, valid.len()));
    for result in &results {
        assert_eq!(
            result.sha256,
            hex::encode(Sha256::digest(&valid[result.start..result.end]))
        );
        for chunk in &result.chunks {
            assert_eq!(
                chunk.sha256,
                hex::encode(Sha256::digest(&valid[chunk.start..chunk.end]))
            );
        }
    }
    for result in section_results(&valid, &layout.sections) {
        assert_eq!(
            result.sha256,
            hex::encode(Sha256::digest(&valid[result.start..result.end]))
        );
    }
    assert_eq!(reconstructed, valid);
    assert_eq!(results[0].chunk_count, 1);
    assert!(results[1].chunk_count > 1);
    assert_eq!(results[2].chunk_count, 1);
    assert_eq!(results[3].chunk_count, 1);
}

#[test]
fn rawzip_sections_include_data_descriptors() {
    let valid = build_stored_zip_with_descriptors(
        &[
            (b"[Content_Types].xml", b"types"),
            (b"word/document.xml", b"document"),
        ],
        b"",
        true,
    );
    let layout = ooxml_layout(&valid, OoxmlKind::Docx).expect("descriptor ZIP is valid");
    assert!(sections_cover_source(&layout.sections, valid.len()));
    for section in &layout.sections[..layout.member_count] {
        let payload_end = section.payload_end.unwrap();
        assert_eq!(section.descriptor_end, Some(payload_end + 16));
        assert!(section.end >= payload_end + 16);
    }

    let mut chunker = RegionChunker::new(&valid, &layout.regions);
    let mut reconstructed = Vec::new();
    while let Some(chunk) = chunker.next_chunk() {
        reconstructed.extend_from_slice(&chunk);
    }
    assert_eq!(reconstructed, valid);
    assert!(region_results_cover_source(
        &region_results(&valid, &layout.regions),
        valid.len()
    ));

    for offset in [14, 18, 22] {
        let mut inconsistent = valid.clone();
        set_u32(&mut inconsistent, offset, 1);
        assert_rejected_by_both(&inconsistent, OoxmlKind::Docx);
    }
}

#[test]
fn subminimum_members_share_one_region_but_directory_stays_separate() {
    let valid = build_stored_zip(
        &[
            (b"[Content_Types].xml", b"types"),
            (b"word/document.xml", b"document"),
            (b"word/styles.xml", b"styles"),
        ],
        b"",
    );
    let layout = ooxml_layout(&valid, OoxmlKind::Docx).unwrap();
    assert_eq!(layout.regions.len(), 2);
    assert_eq!(layout.regions[0].kind, RegionKind::SmallMemberRun);
    assert_eq!(layout.regions[0].member_names.len(), 3);
    assert_eq!(layout.regions[1].kind, RegionKind::CentralDirectory);

    let mut chunker = RegionChunker::new(&valid, &layout.regions);
    while chunker.next_chunk().is_some() {}
    let results = region_results(&valid, &layout.regions);
    assert!(region_results_cover_source(&results, valid.len()));
    assert_eq!(results[0].chunk_count, 1);
    assert_eq!(results[1].chunk_count, 1);
}

#[test]
fn ooxml_layout_rejects_ambiguous_or_noncanonical_archives() {
    let large = vec![b'x'; 2 * MAX_CHUNK + 17];
    let valid = build_stored_zip(
        &[
            (b"[Content_Types].xml", b"types"),
            (b"word/document.xml", &large),
            (b"docProps/core.xml", b"properties"),
        ],
        b"ordinary comment",
    );

    let missing_types = build_stored_zip(&[(b"word/document.xml", &large)], b"");
    assert_rejected_by_both(&missing_types, OoxmlKind::Docx);
    let duplicate_core = build_stored_zip(
        &[
            (b"[Content_Types].xml", b"types"),
            (b"word/document.xml", b"one"),
            (b"word/document.xml", b"two"),
        ],
        b"",
    );
    assert_rejected_by_both(&duplicate_core, OoxmlKind::Docx);

    let duplicate_other = build_stored_zip(
        &[
            (b"[Content_Types].xml", b"types"),
            (b"word/document.xml", b"document"),
            (b"word/styles.xml", b"one"),
            (b"word/styles.xml", b"two"),
        ],
        b"",
    );
    assert_rejected_by_both(&duplicate_other, OoxmlKind::Docx);

    let mut trailing = valid.clone();
    trailing.extend_from_slice(b"trailing data");
    assert_rejected_by_both(&trailing, OoxmlKind::Docx);

    let mutate_eocd = |mut bytes: Vec<u8>, field: usize, value: u16| {
        let eocd = last_signature(&bytes, EOCD_SIGNATURE);
        set_u16(&mut bytes, eocd + field, value);
        bytes
    };
    assert_rejected_by_both(&mutate_eocd(valid.clone(), 4, 1), OoxmlKind::Docx);
    assert_rejected_by_both(&mutate_eocd(valid.clone(), 10, u16::MAX), OoxmlKind::Docx);

    let mut excessive_central = valid.clone();
    let eocd = last_signature(&excessive_central, EOCD_SIGNATURE);
    set_u32(
        &mut excessive_central,
        eocd + 12,
        (MAX_CENTRAL_DIRECTORY + 1) as u32,
    );
    assert_rejected_by_both(&excessive_central, OoxmlKind::Docx);

    let mut unsupported_method = valid.clone();
    let eocd = last_signature(&unsupported_method, EOCD_SIGNATURE);
    let central = test_u32(&unsupported_method, eocd + 16) as usize;
    set_u16(&mut unsupported_method, 8, 99);
    set_u16(&mut unsupported_method, central + 10, 99);
    assert_rejected_by_both(&unsupported_method, OoxmlKind::Docx);

    let mut invalid_store_flags = valid.clone();
    let eocd = last_signature(&invalid_store_flags, EOCD_SIGNATURE);
    let central = test_u32(&invalid_store_flags, eocd + 16) as usize;
    set_u16(&mut invalid_store_flags, 6, 0x0002);
    set_u16(&mut invalid_store_flags, central + 8, 0x0002);
    assert_rejected_by_both(&invalid_store_flags, OoxmlKind::Docx);

    let mut overlapping = valid;
    let eocd = last_signature(&overlapping, EOCD_SIGNATURE);
    let central = test_u32(&overlapping, eocd + 16) as usize;
    let first_name =
        u16::from_le_bytes(overlapping[central + 28..central + 30].try_into().unwrap()) as usize;
    let first_extra =
        u16::from_le_bytes(overlapping[central + 30..central + 32].try_into().unwrap()) as usize;
    let first_comment =
        u16::from_le_bytes(overlapping[central + 32..central + 34].try_into().unwrap()) as usize;
    let second = central + CENTRAL_HEADER_SIZE + first_name + first_extra + first_comment;
    set_u32(&mut overlapping, second + 42, 0);
    assert_rejected_by_both(&overlapping, OoxmlKind::Docx);
}

#[test]
fn ooxml_layout_rejects_inconsistent_local_metadata() {
    let valid = build_stored_zip(
        &[
            (b"[Content_Types].xml", b"types"),
            (b"word/document.xml", b"document"),
        ],
        b"",
    );
    for offset in [14, 18, 22] {
        let mut inconsistent = valid.clone();
        let value = test_u32(&inconsistent, offset);
        set_u32(&mut inconsistent, offset, value.wrapping_add(1));
        assert_rejected_by_both(&inconsistent, OoxmlKind::Docx);
    }
}

#[test]
fn production_regions_use_the_existing_verified_store_writer() {
    let payload = (0..(3 * MAX_CHUNK + 101))
        .map(|index| ((index.wrapping_mul(71) + index / 53) & 0xff) as u8)
        .collect::<Vec<_>>();
    let bytes = build_stored_zip(
        &[
            (b"[Content_Types].xml", b"types"),
            (b"word/document.xml", &payload),
        ],
        b"",
    );
    let regions = production_region_lengths(&bytes, OoxmlKind::Docx).unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("objects.sqlite3");
    let repo_id = RepoId::new();
    ChunkStore::create(&path, &repo_id).unwrap();
    let store = ChunkStore::new(path, &repo_id);
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    assert_eq!(
        store
            .stage_blob_regions(digest, bytes.len() as u64, bytes.as_slice(), &regions)
            .unwrap(),
        (digest, bytes.len() as u64)
    );
    store.complete_blob(&digest, bytes.len() as u64).unwrap();
    assert_eq!(regions.iter().sum::<u64>(), bytes.len() as u64);
    let blob_id = BlobId::parse(format!("sha256:{}", hex::encode(digest))).unwrap();
    let mut reconstructed = Vec::new();
    store
        .copy_blob(&blob_id, bytes.len() as u64, &mut reconstructed)
        .unwrap();
    assert_eq!(reconstructed, bytes);
}

fn range_chunk_digests(bytes: &[u8], start: usize, end: usize) -> Vec<[u8; 32]> {
    let mut position = start;
    let mut digests = Vec::new();
    while position < end {
        let chunk = FastCDC::with_level(
            &bytes[position..end],
            MIN_CHUNK,
            AVG_CHUNK,
            MAX_CHUNK as u32,
            Normalization::Level1,
        )
        .next()
        .unwrap();
        digests.push(Sha256::digest(&bytes[position..position + chunk.length]).into());
        position += chunk.length;
    }
    digests
}

#[test]
fn unchanged_members_keep_chunk_identity_after_insertion_and_reorder() {
    let large = (0..(3 * MAX_CHUNK + 101))
        .map(|index| ((index.wrapping_mul(71) + index / 53) & 0xff) as u8)
        .collect::<Vec<_>>();
    let first = build_stored_zip(
        &[
            (b"[Content_Types].xml", b"types"),
            (b"word/document.xml", &large),
            (b"word/styles.xml", b"styles"),
        ],
        b"",
    );
    let second = build_stored_zip(
        &[
            (b"word/new.xml", b"new member"),
            (b"word/styles.xml", b"styles"),
            (b"[Content_Types].xml", b"types"),
            (b"word/document.xml", &large),
        ],
        b"",
    );
    let first_layout = ooxml_layout(&first, OoxmlKind::Docx).unwrap();
    let second_layout = ooxml_layout(&second, OoxmlKind::Docx).unwrap();
    for name in [
        "[Content_Types].xml",
        "word/document.xml",
        "word/styles.xml",
    ] {
        let first_section = first_layout
            .sections
            .iter()
            .find(|section| section.member_name.as_deref() == Some(name))
            .unwrap();
        let second_section = second_layout
            .sections
            .iter()
            .find(|section| section.member_name.as_deref() == Some(name))
            .unwrap();
        assert_eq!(
            &first[first_section.start..first_section.end],
            &second[second_section.start..second_section.end]
        );
    }
    let first_region = first_layout
        .regions
        .iter()
        .find(|region| region.member_names == ["word/document.xml"])
        .unwrap();
    let second_region = second_layout
        .regions
        .iter()
        .find(|region| region.member_names == ["word/document.xml"])
        .unwrap();
    assert_eq!(
        range_chunk_digests(&first, first_region.start, first_region.end),
        range_chunk_digests(&second, second_region.start, second_region.end)
    );
}

fn summary_version(payload: u64, rows: u64, time: u64) -> VersionResult {
    VersionResult {
        path: "fixture.docx".to_string(),
        scenario: "fixture".to_string(),
        size_bytes: payload,
        sha256: "00".repeat(32),
        metrics: StoreMetrics {
            new_raw_chunk_bytes: payload,
            new_stored_payload_bytes: payload,
            new_distinct_chunks: rows,
            new_mapping_rows: rows,
            cumulative_stored_payload_bytes: payload,
            cumulative_page_bytes: payload,
        },
        median_elapsed_ns: time,
        median_validator_ns: 0,
        central_directory_bytes: 0,
        member_count: 0,
        candidate_layout_owned_bytes: 0,
        sections: Vec::new(),
        chunk_regions: Vec::new(),
    }
}

#[test]
fn benchmark_summary_reports_fixed_metrics() {
    let sequence = |candidate_payload| SequenceResult {
        name: "fixture".to_string(),
        format: OoxmlKind::Docx,
        generic: vec![
            summary_version(100_000, 2, 1_000),
            summary_version(100_000, 2, 1_000),
            summary_version(100_000, 2, 1_000),
        ],
        candidate: vec![
            summary_version(70_000, 2, 1_000),
            summary_version(candidate_payload, 2, 1_000),
            summary_version(candidate_payload, 2, 1_000),
        ],
    };
    let lower = summarize(&[sequence(70_000)]);
    assert_eq!(lower.len(), 1);
    assert_eq!(lower[0].aggregate_saving_basis_points, 3_000);
    assert_eq!(lower[0].median_transition_saving_basis_points, 3_000);

    let smaller = summarize(&[sequence(90_000)]);
    assert_eq!(smaller[0].aggregate_saving_basis_points, 1_000);
    assert_eq!(smaller[0].median_transition_saving_basis_points, 1_000);
}

#[test]
fn median_averages_the_two_middle_observations() {
    assert_eq!(median(&mut [10, 20]), 15);
    assert_eq!(median_i64(&mut [-100, 200]), 50);
}

#[test]
#[ignore = "requires an explicit checksummed Office corpus; run with --ignored --nocapture"]
fn ooxml_chunk_benchmark() {
    const REPETITIONS: usize = 7;

    let git_commit = clean_git_commit();
    let corpus_root = PathBuf::from(
        std::env::var("SYNCK_OOXML_BENCH_CORPUS").expect("SYNCK_OOXML_BENCH_CORPUS is required"),
    );
    let output_path = PathBuf::from(
        std::env::var("SYNCK_OOXML_BENCH_OUTPUT").expect("SYNCK_OOXML_BENCH_OUTPUT is required"),
    );
    let cpu = std::env::var("SYNCK_OOXML_BENCH_CPU").expect("SYNCK_OOXML_BENCH_CPU is required");
    let memory =
        std::env::var("SYNCK_OOXML_BENCH_MEMORY").expect("SYNCK_OOXML_BENCH_MEMORY is required");
    let filesystem = std::env::var("SYNCK_OOXML_BENCH_FILESYSTEM")
        .expect("SYNCK_OOXML_BENCH_FILESYSTEM is required");
    let manifest_path = corpus_root.join("manifest.json");
    let manifest_bytes = fs::read(&manifest_path).expect("read corpus manifest");
    let manifest: CorpusManifest =
        serde_json::from_slice(&manifest_bytes).expect("parse corpus manifest");
    assert_eq!(manifest.schema_version, 1, "unsupported corpus manifest");
    assert!(!manifest.sequences.is_empty(), "empty corpus");

    let mut sequences = Vec::with_capacity(manifest.sequences.len());
    for sequence in &manifest.sequences {
        assert!(!sequence.versions.is_empty(), "empty sequence");
        let mut generic_runs = Vec::with_capacity(REPETITIONS);
        let mut candidate_runs = Vec::with_capacity(REPETITIONS);
        for repetition in 0..REPETITIONS {
            let (generic, candidate) = if repetition % 2 == 0 {
                (
                    run_policy(&corpus_root, &sequence.versions, Policy::Generic).unwrap(),
                    run_policy(
                        &corpus_root,
                        &sequence.versions,
                        Policy::Candidate(sequence.format),
                    )
                    .unwrap(),
                )
            } else {
                let candidate = run_policy(
                    &corpus_root,
                    &sequence.versions,
                    Policy::Candidate(sequence.format),
                )
                .unwrap();
                let generic =
                    run_policy(&corpus_root, &sequence.versions, Policy::Generic).unwrap();
                (generic, candidate)
            };
            generic_runs.push(generic);
            candidate_runs.push(candidate);
        }

        let collapse = |runs: Vec<Vec<MeasuredVersion>>| {
            let mut results = Vec::with_capacity(sequence.versions.len());
            for version_index in 0..sequence.versions.len() {
                let mut elapsed = runs
                    .iter()
                    .map(|run| duration_ns(run[version_index].elapsed))
                    .collect::<Vec<_>>();
                let mut validator = runs
                    .iter()
                    .map(|run| duration_ns(run[version_index].validator_elapsed))
                    .collect::<Vec<_>>();
                let mut result = runs[0][version_index].result.clone();
                for run in &runs[1..] {
                    assert_eq!(
                        run[version_index].result.metrics, result.metrics,
                        "nondeterministic storage metrics"
                    );
                }
                result.median_elapsed_ns = median(&mut elapsed);
                result.median_validator_ns = median(&mut validator);
                results.push(result);
            }
            results
        };
        let generic = collapse(generic_runs);
        let candidate = collapse(candidate_runs);
        sequences.push(SequenceResult {
            name: sequence.name.clone(),
            format: sequence.format,
            generic,
            candidate,
        });
    }

    let invocation = format!(
        "SYNCK_OOXML_BENCH_CORPUS={} SYNCK_OOXML_BENCH_OUTPUT={} \
SYNCK_OOXML_BENCH_CPU=<recorded> SYNCK_OOXML_BENCH_MEMORY=<recorded> \
SYNCK_OOXML_BENCH_FILESYSTEM=<recorded> cargo test -p synck-local --release \
ooxml_chunk_benchmark -- --ignored --nocapture",
        corpus_root.display(),
        output_path.display()
    );
    let summaries = summarize(&sequences);
    for summary in &summaries {
        println!(
            "{:?}: {} transitions, aggregate {:+.2}%, median {:+.2}%, rows {} -> {}, time {:+.2}%",
            summary.format,
            summary.measured_transitions,
            summary.aggregate_saving_basis_points as f64 / 100.0,
            summary.median_transition_saving_basis_points as f64 / 100.0,
            summary.generic_mapping_rows,
            summary.candidate_mapping_rows,
            summary.median_time_saving_basis_points as f64 / 100.0,
        );
    }
    let output = BenchmarkResult {
        schema_version: 4,
        git_commit,
        runtime_os: std::env::consts::OS.to_string(),
        runtime_arch: std::env::consts::ARCH.to_string(),
        cpu,
        memory,
        filesystem,
        source_os: manifest.source_os,
        office_version: manifest.office_version,
        corpus_manifest_sha256: hex::encode(Sha256::digest(&manifest_bytes)),
        repetitions: REPETITIONS,
        invocation,
        summaries,
        sequences,
    };
    let json = serde_json::to_string_pretty(&output).unwrap();
    fs::write(&output_path, format!("{json}\n")).expect("write benchmark output");
    println!("{json}");
}
