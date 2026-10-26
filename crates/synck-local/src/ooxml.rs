use std::collections::BTreeSet;
use std::fs::File;
use std::path::Path;
#[cfg(windows)]
use std::{io, io::Read as _, io::Seek as _, sync::Mutex};

#[cfg(not(windows))]
use rawzip::FileReader;
use rawzip::extra_fields::{ExtraFieldId, ExtraFields};
use rawzip::{CompressionMethod, RECOMMENDED_BUFFER_SIZE, ReaderAt, ZipLocator};

use crate::chunk_store::MIN_CHUNK;

const MAX_CENTRAL_DIRECTORY: u64 = 8 * 1024 * 1024;
const MAX_MEMBERS: u64 = u16::MAX as u64;
const COMMON_FLAGS: u16 = 0x0808;
const DEFLATE_OPTION_FLAGS: u16 = 0x0006;
const EOCD_SIZE: usize = 22;

#[cfg(windows)]
struct CursorPreservingFile(Mutex<File>);

#[cfg(windows)]
impl ReaderAt for CursorPreservingFile {
    fn read_at(&self, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
        let mut file = self.0.lock().unwrap();
        let original = file.stream_position()?;
        file.seek(io::SeekFrom::Start(offset))?;
        let result = file.read(bytes);
        file.seek(io::SeekFrom::Start(original))?;
        result
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OoxmlKind {
    Docx,
    Xlsx,
    Pptx,
}

impl OoxmlKind {
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?;
        if extension.eq_ignore_ascii_case("docx") {
            Some(Self::Docx)
        } else if extension.eq_ignore_ascii_case("xlsx") {
            Some(Self::Xlsx)
        } else if extension.eq_ignore_ascii_case("pptx") {
            Some(Self::Pptx)
        } else {
            None
        }
    }

    fn core_member(self) -> &'static [u8] {
        match self {
            Self::Docx => b"word/document.xml",
            Self::Xlsx => b"xl/workbook.xml",
            Self::Pptx => b"ppt/presentation.xml",
        }
    }
}

#[derive(Debug)]
struct Section {
    start: u64,
    payload_start: u64,
    payload_end: u64,
    descriptor_end: Option<u64>,
    end: u64,
}

pub(crate) fn chunk_region_lengths(
    file: File,
    source_size: u64,
    kind: OoxmlKind,
) -> Option<Vec<u64>> {
    let mut buffer = vec![0_u8; RECOMMENDED_BUFFER_SIZE];
    #[cfg(not(windows))]
    let reader = FileReader::from(file);
    // Windows positioned file reads advance the shared file cursor. `File::try_clone` shares that
    // cursor with the capture handle, so rawzip must restore it after every validation read.
    #[cfg(windows)]
    let reader = CursorPreservingFile(Mutex::new(file));
    let archive = ZipLocator::new()
        .locate_in_reader(reader, &mut buffer, source_size)
        .ok()?;
    let member_hint = archive.entries_hint();
    let central_offset = archive.directory_offset();
    let eocd_offset = archive.eocd_offset();
    let archive_end = archive.end_offset();
    let central_size = eocd_offset.checked_sub(central_offset)?;
    let eocd = read_exact_at::<EOCD_SIZE>(archive.get_ref(), eocd_offset)?;
    if zip_u32(&eocd, 0)? != 0x0605_4b50
        || zip_u16(&eocd, 4)? != 0
        || zip_u16(&eocd, 6)? != 0
        || zip_u16(&eocd, 8)? != zip_u16(&eocd, 10)?
        || u64::from(zip_u16(&eocd, 10)?) != member_hint
        || u64::from(zip_u32(&eocd, 12)?) != central_size
        || u64::from(zip_u32(&eocd, 16)?) != central_offset
        || member_hint == 0
        || member_hint > MAX_MEMBERS
        || central_offset == 0
        || central_size > MAX_CENTRAL_DIRECTORY
        || archive_end != source_size
    {
        return None;
    }

    let capacity = usize::try_from(member_hint).ok()?;
    let mut sections = Vec::with_capacity(capacity);
    let mut names = BTreeSet::new();
    let mut content_types = 0_usize;
    let mut core_members = 0_usize;
    let mut local_buffer = vec![0_u8; RECOMMENDED_BUFFER_SIZE];
    let mut entries = archive.entries(&mut buffer);
    while let Some(entry) = entries.next_entry().ok()? {
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
        let name = entry.file_path().as_ref().to_vec();
        if name.is_empty() || std::str::from_utf8(&name).is_err() || !names.insert(name.clone()) {
            return None;
        }
        content_types += usize::from(name == b"[Content_Types].xml");
        core_members += usize::from(name == kind.core_member());

        let local_entry = archive.get_entry(entry.wayfinder()).ok()?;
        let local_header = local_entry.local_header(&mut local_buffer).ok()?;
        if local_header.file_path().as_ref() != name
            || local_header.flags() != flags
            || local_header.compression_method() != method
            || !supported_extra_fields(local_header.extra_fields())
        {
            return None;
        }
        let (payload_start, payload_end) = local_entry.compressed_data_range();
        let descriptor_end = if flags.has_data_descriptor() {
            if local_header.crc32() != 0
                || local_header.compressed_size_hint() != 0
                || local_header.uncompressed_size_hint() != 0
            {
                return None;
            }
            let descriptor = local_entry.reader().data_descriptor().ok()??;
            if descriptor.crc32() != entry.crc32()
                || descriptor.compressed_size() != entry.compressed_size_hint()
                || descriptor.uncompressed_size() != entry.uncompressed_size_hint()
            {
                return None;
            }
            let signature = read_exact_at::<4>(archive.get_ref(), payload_end)?;
            Some(
                payload_end.checked_add(if signature == 0x0807_4b50_u32.to_le_bytes() {
                    16
                } else {
                    12
                })?,
            )
        } else {
            if local_header.crc32() != entry.crc32()
                || local_header.compressed_size_hint() != entry.compressed_size_hint()
                || local_header.uncompressed_size_hint() != entry.uncompressed_size_hint()
                || local_entry.reader().data_descriptor().ok()?.is_some()
            {
                return None;
            }
            None
        };
        sections.push(Section {
            start: entry.local_header_offset(),
            payload_start,
            payload_end,
            descriptor_end,
            end: 0,
        });
    }
    if sections.len() != capacity || content_types != 1 || core_members != 1 {
        return None;
    }

    sections.sort_unstable_by_key(|section| section.start);
    if sections.first()?.start != 0 {
        return None;
    }
    for index in 0..sections.len() {
        let end = sections
            .get(index + 1)
            .map_or(central_offset, |section| section.start);
        let section = &mut sections[index];
        let framing_end = section.descriptor_end.unwrap_or(section.payload_end);
        if section.start >= end
            || section.payload_start < section.start
            || section.payload_start > section.payload_end
            || section.payload_end > framing_end
            || framing_end > end
        {
            return None;
        }
        section.end = end;
    }
    region_lengths(&sections, central_offset, source_size)
}

fn region_lengths(sections: &[Section], central_offset: u64, source_size: u64) -> Option<Vec<u64>> {
    let mut next = 0_u64;
    for section in sections {
        if section.start != next || section.start >= section.end || section.end > central_offset {
            return None;
        }
        next = section.end;
    }
    if next != central_offset || central_offset >= source_size {
        return None;
    }

    let mut ends = Vec::new();
    let mut small_start = None;
    for section in sections {
        if section.end.checked_sub(section.start)? >= u64::from(MIN_CHUNK) {
            if small_start.take().is_some() {
                ends.push(section.start);
            }
            ends.push(section.end);
        } else if small_start.is_none() {
            small_start = Some(section.start);
        }
    }
    if small_start.is_some() {
        ends.push(central_offset);
    }
    ends.push(source_size);
    let mut previous = 0_u64;
    let mut lengths = Vec::with_capacity(ends.len());
    for end in ends {
        let length = end.checked_sub(previous)?;
        if length == 0 {
            return None;
        }
        lengths.push(length);
        previous = end;
    }
    (previous == source_size).then_some(lengths)
}

fn supported_extra_fields(mut fields: ExtraFields<'_>) -> bool {
    if fields.by_ref().any(|(kind, _)| kind == ExtraFieldId::ZIP64) {
        return false;
    }
    fields.remaining_bytes().is_empty()
}

fn read_exact_at<const N: usize>(reader: &impl ReaderAt, offset: u64) -> Option<[u8; N]> {
    let mut bytes = [0_u8; N];
    reader.read_exact_at(&mut bytes, offset).ok()?;
    Some(bytes)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn real_office_templates_are_eligible_and_exactly_bounded() {
        let fixtures = [
            (OoxmlKind::Docx, "assets/templates/empty.docx"),
            (OoxmlKind::Xlsx, "assets/templates/empty.xlsx"),
            (OoxmlKind::Pptx, "assets/templates/empty.pptx"),
        ];
        for (kind, relative) in fixtures {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
            let file = File::open(&path).unwrap();
            let size = file.metadata().unwrap().len();
            let regions = chunk_region_lengths(file, size, kind).unwrap();
            assert!(!regions.is_empty());
            assert_eq!(regions.iter().sum::<u64>(), size);
        }
    }

    #[test]
    fn validation_uses_positioned_reads_only() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/templates/empty.docx");
        let mut source = File::open(&path).unwrap();
        let validation_source = source.try_clone().unwrap();
        let size = source.metadata().unwrap().len();

        chunk_region_lengths(validation_source, size, OoxmlKind::Docx).unwrap();

        let mut signature = [0_u8; 4];
        source.read_exact(&mut signature).unwrap();
        assert_eq!(signature, 0x0403_4b50_u32.to_le_bytes());
    }

    #[test]
    fn extension_selection_is_closed_and_case_insensitive() {
        assert_eq!(
            OoxmlKind::from_path(Path::new("a.DOCX")),
            Some(OoxmlKind::Docx)
        );
        assert_eq!(
            OoxmlKind::from_path(Path::new("a.xlsx")),
            Some(OoxmlKind::Xlsx)
        );
        assert_eq!(
            OoxmlKind::from_path(Path::new("a.PpTx")),
            Some(OoxmlKind::Pptx)
        );
        assert_eq!(OoxmlKind::from_path(Path::new("a.zip")), None);
        assert_eq!(OoxmlKind::from_path(Path::new("docx")), None);
    }

    #[test]
    fn truncation_and_kind_mismatch_are_ineligible() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/templates/empty.docx");
        let bytes = std::fs::read(source).unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("test.docx");
        std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        let file = File::open(&path).unwrap();
        assert!(chunk_region_lengths(file, (bytes.len() - 1) as u64, OoxmlKind::Docx).is_none());

        std::fs::write(&path, &bytes).unwrap();
        let file = File::open(&path).unwrap();
        assert!(chunk_region_lengths(file, bytes.len() as u64, OoxmlKind::Xlsx).is_none());
    }

    #[test]
    fn zip64_and_malformed_extra_fields_are_ineligible() {
        assert!(supported_extra_fields(ExtraFields::new(&[])));
        assert!(supported_extra_fields(ExtraFields::new(&[
            0x34, 0x12, 2, 0, 1, 2,
        ])));
        assert!(!supported_extra_fields(ExtraFields::new(&[1, 0, 0, 0])));
        assert!(!supported_extra_fields(ExtraFields::new(&[
            0x34, 0x12, 3, 0, 1, 2,
        ])));
        assert!(!supported_extra_fields(ExtraFields::new(&[1, 2, 3])));
    }
}
