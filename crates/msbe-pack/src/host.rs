//! Host-owned ZIP input and deterministic ZIP layout writing.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufReader, Read, Seek, Write},
    path::Path,
    sync::Mutex,
};

use msbe_archive::Limits;
use msbe_fsops::{RelPath, Store};
use msbe_provider_api::{
    ContainerKind, EntryContent, LayoutEntry, PackCodecError, PackEntry, PackInput, PackLayout,
};
use zip::{DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

/// Output accepted by the ZIP host writer.
pub trait WriteSeek: Write + Seek {}

impl<T: Write + Seek + ?Sized> WriteSeek for T {}

/// A host-validated ZIP input exposed to codecs through [`PackInput`].
#[derive(Debug)]
pub struct ZipPackInput {
    entries: Vec<PackEntry>,
    archive: Mutex<ZipArchive<BufReader<File>>>,
    limit: u64,
}

impl ZipPackInput {
    /// Opens and validates a ZIP before any codec sees its contents.
    ///
    /// # Errors
    ///
    /// Returns a codec error when the input is malformed or exceeds shared limits.
    pub fn open(path: &Path, limits: &Limits) -> Result<Self, PackCodecError> {
        let file = File::open(path)?;
        let mut archive = ZipArchive::new(BufReader::new(file)).map_err(codec_error)?;
        if archive.len() > limits.max_entries {
            return Err(PackCodecError::Limit(
                "archive has too many entries".to_owned(),
            ));
        }
        let mut total = 0_u64;
        let mut seen = BTreeSet::new();
        let mut entries = Vec::new();
        for index in 0..archive.len() {
            let entry = archive.by_index(index).map_err(codec_error)?;
            if entry.is_dir() {
                continue;
            }
            let path = RelPath::new(entry.name())
                .map_err(|error| PackCodecError::Codec(error.to_string()))?;
            if !seen.insert(path.as_str().to_ascii_lowercase()) {
                return Err(PackCodecError::Codec(
                    "archive has duplicate normalized paths".to_owned(),
                ));
            }
            if entry.size() > limits.max_file_bytes {
                return Err(PackCodecError::Limit(format!(
                    "{} exceeds the entry limit",
                    path
                )));
            }
            if entry.size() > (1 << 20)
                && (entry.compressed_size() == 0
                    || entry.compressed_size().saturating_mul(limits.max_ratio) < entry.size())
            {
                return Err(PackCodecError::Limit(format!(
                    "{} exceeds the compression ratio limit",
                    path
                )));
            }
            total = total
                .checked_add(entry.size())
                .ok_or_else(|| PackCodecError::Limit("archive size overflow".to_owned()))?;
            if total > limits.max_total_bytes {
                return Err(PackCodecError::Limit(
                    "archive exceeds the total size limit".to_owned(),
                ));
            }
            entries.push(PackEntry {
                path,
                size: entry.size(),
            });
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(Self {
            entries,
            archive: Mutex::new(archive),
            limit: limits.max_file_bytes,
        })
    }
}

impl PackInput for ZipPackInput {
    fn container(&self) -> ContainerKind {
        ContainerKind::Zip
    }

    fn entries(&self) -> &[PackEntry] {
        &self.entries
    }

    fn read(&self, path: &RelPath, limit: u64) -> Result<Vec<u8>, PackCodecError> {
        let cap = limit.min(self.limit);
        let mut archive = self
            .archive
            .lock()
            .map_err(|_| PackCodecError::Codec("pack input lock poisoned".to_owned()))?;
        let entry = archive
            .by_name(path.as_str())
            .map_err(|_| PackCodecError::FormatMismatch)?;
        if entry.size() > cap {
            return Err(PackCodecError::Limit(format!(
                "{} exceeds the read limit",
                path
            )));
        }
        let mut bytes = Vec::new();
        entry.take(cap.saturating_add(1)).read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > cap {
            return Err(PackCodecError::Limit(format!(
                "{} exceeds the read limit",
                path
            )));
        }
        Ok(bytes)
    }
}

/// Writes a validated ZIP layout with deterministic metadata and store-backed blobs.
///
/// # Errors
///
/// Returns a codec error when the layout is invalid, a blob is missing, or writing fails.
pub fn write_zip_layout(
    layout: &PackLayout,
    store: &Store,
    output: &mut dyn WriteSeek,
) -> Result<(), PackCodecError> {
    if layout.container != ContainerKind::Zip {
        return Err(PackCodecError::Codec(
            "ZIP host cannot write this container kind".to_owned(),
        ));
    }
    let mut entries: BTreeMap<&RelPath, &LayoutEntry> = BTreeMap::new();
    for entry in &layout.entries {
        if entries.insert(&entry.path, entry).is_some() {
            return Err(PackCodecError::Codec(
                "layout has duplicate paths".to_owned(),
            ));
        }
    }
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(9))
        .last_modified_time(DateTime::default())
        .unix_permissions(0o644);
    let mut archive = ZipWriter::new(output);
    for entry in entries.values() {
        archive
            .start_file(entry.path.as_str(), options)
            .map_err(codec_error)?;
        match &entry.content {
            EntryContent::Inline(bytes) => archive.write_all(bytes)?,
            EntryContent::Blob(digest) => {
                let mut blob = store
                    .open_blob(digest)
                    .map_err(|_| PackCodecError::MissingBlob(*digest))?;
                std::io::copy(&mut blob, &mut archive)?;
            }
        }
    }
    archive.finish().map_err(codec_error)?;
    Ok(())
}

fn codec_error(error: impl std::fmt::Display) -> PackCodecError {
    PackCodecError::Codec(error.to_string())
}
