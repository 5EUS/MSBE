//! Host-owned ZIP input and deterministic ZIP layout writing.
//!
//! Codecs never open or write containers (§17.4). Every limit, path rule and digest check lives
//! here, so it is enforced once rather than trusted to each format.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, BufReader, Read, Seek, Write},
    path::Path,
    sync::Mutex,
};

use msbe_archive::Limits;
use msbe_fsops::{Digest, RelPath, Store};
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
    digest: Digest,
}

impl ZipPackInput {
    /// Opens and validates a ZIP before any codec sees its contents.
    ///
    /// # Errors
    ///
    /// Returns a codec error when the input is malformed, holds an unsafe or duplicate path or a
    /// symlink, or exceeds shared limits.
    pub fn open(path: &Path, limits: &Limits) -> Result<Self, PackCodecError> {
        let digest = Digest::of_reader(BufReader::new(File::open(path)?))?;
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
            if entry.is_symlink() {
                return Err(PackCodecError::UnsafePath(format!(
                    "{} is a symbolic link",
                    entry.name()
                )));
            }
            let path = RelPath::new(entry.name()).map_err(|error| {
                PackCodecError::UnsafePath(format!("{}: {error}", entry.name()))
            })?;
            if !seen.insert(path.as_str().to_ascii_lowercase()) {
                return Err(PackCodecError::UnsafePath(format!(
                    "{path} appears more than once after normalization"
                )));
            }
            if entry.size() > limits.max_file_bytes {
                return Err(PackCodecError::Limit(format!(
                    "{path} exceeds the entry limit"
                )));
            }
            if entry.size() > (1 << 20)
                && (entry.compressed_size() == 0
                    || entry.compressed_size().saturating_mul(limits.max_ratio) < entry.size())
            {
                return Err(PackCodecError::Limit(format!(
                    "{path} exceeds the compression ratio limit"
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
            digest,
        })
    }

    /// The digest of the input file as read.
    pub const fn digest(&self) -> Digest {
        self.digest
    }

    /// Whether the input holds an entry at `path`.
    pub fn contains(&self, path: &RelPath) -> bool {
        self.entries
            .binary_search_by(|entry| entry.path.cmp(path))
            .is_ok()
    }

    /// Streams one entry to `read`, failing once more than `limit` bytes (or the host entry
    /// limit) are decompressed. Blob bytes take this path so they never pass through a codec.
    ///
    /// # Errors
    ///
    /// Returns a codec error when the entry is missing or exceeds its limit, or `read` fails.
    pub fn with_entry<T>(
        &self,
        path: &RelPath,
        limit: u64,
        read: impl FnOnce(&mut dyn Read) -> io::Result<T>,
    ) -> Result<T, PackCodecError> {
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
                "{path} exceeds the read limit"
            )));
        }
        let mut bounded = Bounded {
            inner: entry,
            remaining: cap,
        };
        read(&mut bounded).map_err(|error| {
            if error.kind() == io::ErrorKind::FileTooLarge {
                PackCodecError::Limit(format!("{path} exceeds the read limit"))
            } else {
                PackCodecError::Io(error)
            }
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
        self.with_entry(path, limit, |reader| {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            Ok(bytes)
        })
    }
}

/// A reader that fails instead of yielding more than `remaining` bytes.
struct Bounded<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        let length = u64::try_from(read).unwrap_or(u64::MAX);
        self.remaining = self.remaining.checked_sub(length).ok_or_else(|| {
            io::Error::new(io::ErrorKind::FileTooLarge, "entry exceeds the read limit")
        })?;
        Ok(read)
    }
}

/// Entry compression selected by normalized options. It changes bytes, never logical content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// Deflate at a fixed level.
    Deflate,
    /// No compression.
    Store,
}

/// Whether a layout was written completely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    /// Every entry was written and the archive finished.
    Complete,
    /// The progress callback asked to stop; the output is incomplete and must be discarded.
    Cancelled,
}

/// Writes a validated ZIP layout with deterministic metadata and store-backed blobs.
///
/// Every blob is hashed while it streams into the archive, so a corrupted store fails the export
/// rather than shipping bytes that do not match the digest the layout names. `progress` receives
/// the entries written and the total before each entry, and returns `false` to cancel.
///
/// # Errors
///
/// Returns a codec error when the layout is invalid, a blob is missing or corrupt, or writing
/// fails.
pub fn write_zip_layout(
    layout: &PackLayout,
    store: &Store,
    compression: Compression,
    output: &mut dyn WriteSeek,
    progress: &mut dyn FnMut(usize, usize) -> bool,
) -> Result<Written, PackCodecError> {
    if layout.container != ContainerKind::Zip {
        return Err(PackCodecError::Codec(
            "ZIP host cannot write this container kind".to_owned(),
        ));
    }
    let mut entries: BTreeMap<&RelPath, &LayoutEntry> = BTreeMap::new();
    for entry in &layout.entries {
        if entries.insert(&entry.path, entry).is_some() {
            return Err(PackCodecError::UnsafePath(format!(
                "layout has duplicate path {}",
                entry.path
            )));
        }
    }
    let options = match compression {
        Compression::Deflate => SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(9)),
        Compression::Store => {
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)
        }
    }
    .last_modified_time(DateTime::default())
    .unix_permissions(0o644)
    .large_file(true);
    let total = entries.len();
    let mut archive = ZipWriter::new(output);
    for (index, entry) in entries.values().enumerate() {
        if !progress(index, total) {
            return Ok(Written::Cancelled);
        }
        archive
            .start_file(entry.path.as_str(), options)
            .map_err(codec_error)?;
        match &entry.content {
            EntryContent::Inline(bytes) => archive.write_all(bytes)?,
            EntryContent::Blob(digest) => {
                let blob = store
                    .open_blob(digest)
                    .map_err(|_| PackCodecError::MissingBlob(*digest))?;
                let found = Digest::of_reader(Tee {
                    inner: BufReader::new(blob),
                    sink: &mut archive,
                })?;
                if found != *digest {
                    return Err(PackCodecError::Codec(format!(
                        "store blob {digest} hashes to {found}"
                    )));
                }
            }
        }
    }
    progress(total, total);
    archive.finish().map_err(codec_error)?;
    Ok(Written::Complete)
}

/// Copies everything read from `inner` into `sink`.
struct Tee<'a, R> {
    inner: R,
    sink: &'a mut dyn Write,
}

impl<R: Read> Read for Tee<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.sink.write_all(buf.get(..read).unwrap_or_default())?;
        Ok(read)
    }
}

fn codec_error(error: impl std::fmt::Display) -> PackCodecError {
    PackCodecError::Codec(error.to_string())
}
