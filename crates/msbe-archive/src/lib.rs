//! Hardened archive extraction.
//!
//! The only crate permitted to read archive input. Every entry is validated before its
//! bytes are read: zip-slip, absolute paths, platform-unsafe names, symlinks, encryption,
//! decompression bombs, oversized archives and case collisions are rejected here, so no
//! other crate has to think about them. Nothing is written anywhere except the store.
//!
//! See `docs/11-security.md` §11.2.
#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
};

use msbe_fsops::{Digest, RelPath, Store};
use thiserror::Error;
use zip::{CompressionMethod, ZipArchive, result::ZipError};

/// Below this many decompressed bytes a high compression ratio is harmless and not checked.
const RATIO_FLOOR_BYTES: u64 = 1 << 20;

/// Bounds that stop a hostile archive from exhausting disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most entries an archive may contain.
    pub max_entries: usize,
    /// The most bytes one file may decompress to.
    pub max_file_bytes: u64,
    /// The most bytes an archive may decompress to in total.
    pub max_total_bytes: u64,
    /// The highest decompressed-to-compressed ratio a file larger than a mebibyte may have.
    pub max_ratio: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_file_bytes: 4 << 30,
            max_total_bytes: 16 << 30,
            max_ratio: 1_000,
        }
    }
}

/// One file from an artifact, now in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestedFile {
    /// The path inside the artifact. A bare file is named after itself.
    pub source: RelPath,
    /// The stored contents.
    pub blob: Digest,
    /// The number of bytes actually stored.
    pub size: u64,
}

/// Why an artifact was refused.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ArchiveError {
    /// The artifact could not be opened or read.
    #[error("cannot read {}: {source}", .path.display())]
    Io {
        /// The artifact.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// The artifact is not a readable zip archive.
    #[error("{} is not a readable zip archive: {source}", .path.display())]
    Malformed {
        /// The artifact.
        path: PathBuf,
        /// The decoder's error.
        #[source]
        source: ZipError,
    },

    /// An entry name could escape the extraction root, or is invalid on some platform.
    #[error("entry {entry:?} has an unsafe path: {source}")]
    UnsafePath {
        /// The entry name as stored in the archive.
        entry: String,
        /// The validation failure.
        #[source]
        source: msbe_fsops::Error,
    },

    /// An entry is a symbolic link. Links are never extracted.
    #[error("entry {entry:?} is a symbolic link")]
    Symlink {
        /// The entry name.
        entry: String,
    },

    /// An entry is encrypted.
    #[error("entry {entry:?} is encrypted")]
    Encrypted {
        /// The entry name.
        entry: String,
    },

    /// An entry uses a compression method this build cannot read.
    #[error("entry {entry:?} uses unsupported compression {method}")]
    UnsupportedCompression {
        /// The entry name.
        entry: String,
        /// The compression method.
        method: String,
    },

    /// The archive has more entries than allowed.
    #[error("the archive has more than {limit} entries")]
    TooManyEntries {
        /// The limit.
        limit: usize,
    },

    /// A file, or the archive as a whole, decompresses past a size limit.
    #[error("entry {entry:?} exceeds the {limit}-byte limit")]
    TooLarge {
        /// The entry name.
        entry: String,
        /// The limit that applied.
        limit: u64,
    },

    /// A file expands far more than real content does: the signature of a decompression bomb.
    #[error("entry {entry:?} expands more than {limit}:1")]
    RatioExceeded {
        /// The entry name.
        entry: String,
        /// The ratio limit.
        limit: u64,
    },

    /// Two entries differ only in letter case, so they collide on case-insensitive volumes.
    #[error("entries {first:?} and {second:?} differ only in case")]
    CaseCollision {
        /// The earlier entry.
        first: String,
        /// The later entry.
        second: String,
    },

    /// The same path appears twice.
    #[error("entry {entry:?} appears more than once")]
    Duplicate {
        /// The entry name.
        entry: String,
    },

    /// Writing to the store failed.
    #[error(transparent)]
    Store(msbe_fsops::Error),
}

/// Adds a local artifact to the store and returns its files.
///
/// A `.zip` archive is extracted entry by entry. Any other file, including a `.jar` that
/// happens to be zip-formatted, is stored whole under its own name: whether a container is
/// unpacked is a property of the artifact's kind, not of its bytes.
///
/// Sizes are enforced on the bytes actually decompressed, never on the sizes an archive
/// claims, so a lying header cannot smuggle a bomb past the limits.
///
/// # Errors
///
/// Returns an [`ArchiveError`] naming the entry and the rule it broke. Nothing is added to
/// the store for a refused entry.
pub fn ingest(
    store: &Store,
    path: &Path,
    limits: &Limits,
) -> Result<Vec<IngestedFile>, ArchiveError> {
    if is_zip(path) {
        extract_zip(store, path, limits)
    } else {
        ingest_file(store, path, limits)
    }
}

fn is_zip(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
}

fn ingest_file(
    store: &Store,
    path: &Path,
    limits: &Limits,
) -> Result<Vec<IngestedFile>, ArchiveError> {
    let io_error = |source: io::Error| ArchiveError::Io {
        path: path.to_path_buf(),
        source,
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let source = RelPath::new(&name).map_err(|source| ArchiveError::UnsafePath {
        entry: name.clone(),
        source,
    })?;
    let file = File::open(path).map_err(io_error)?;
    let mut reader = Limited::new(BufReader::new(file), limits.max_file_bytes);
    let blob = match store.put_reader(&mut reader) {
        Ok(blob) => blob,
        Err(_) if reader.tripped => {
            return Err(ArchiveError::TooLarge {
                entry: name,
                limit: limits.max_file_bytes,
            });
        }
        Err(error) => return Err(ArchiveError::Store(error)),
    };
    Ok(vec![IngestedFile {
        source,
        blob,
        size: reader.consumed,
    }])
}

fn extract_zip(
    store: &Store,
    path: &Path,
    limits: &Limits,
) -> Result<Vec<IngestedFile>, ArchiveError> {
    let malformed = |source: ZipError| ArchiveError::Malformed {
        path: path.to_path_buf(),
        source,
    };
    let file = File::open(path).map_err(|source| ArchiveError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut archive = ZipArchive::new(BufReader::new(file)).map_err(malformed)?;
    if archive.len() > limits.max_entries {
        return Err(ArchiveError::TooManyEntries {
            limit: limits.max_entries,
        });
    }

    let mut files = Vec::new();
    // Case-folded path to the path as first seen, for collision detection.
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut total = 0_u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(malformed)?;
        let raw = entry.name().to_owned();

        if entry.is_symlink() {
            return Err(ArchiveError::Symlink { entry: raw });
        }
        if entry.is_dir() {
            let trimmed = raw.trim_end_matches('/');
            if !trimmed.is_empty() && trimmed != "." {
                RelPath::new(trimmed).map_err(|source| ArchiveError::UnsafePath {
                    entry: raw.clone(),
                    source,
                })?;
            }
            continue;
        }
        if entry.encrypted() {
            return Err(ArchiveError::Encrypted { entry: raw });
        }
        let method = entry.compression();
        if !matches!(
            method,
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            return Err(ArchiveError::UnsupportedCompression {
                entry: raw,
                method: format!("{method:?}"),
            });
        }

        let source = RelPath::new(&raw).map_err(|source| ArchiveError::UnsafePath {
            entry: raw.clone(),
            source,
        })?;
        let folded = source.as_str().to_lowercase();
        if let Some(first) = seen.get(&folded) {
            return Err(if first == source.as_str() {
                ArchiveError::Duplicate { entry: raw }
            } else {
                ArchiveError::CaseCollision {
                    first: first.clone(),
                    second: raw,
                }
            });
        }
        seen.insert(folded, source.as_str().to_owned());

        let size_cap = limits
            .max_file_bytes
            .min(limits.max_total_bytes.saturating_sub(total));
        let ratio_cap = entry
            .compressed_size()
            .saturating_mul(limits.max_ratio)
            .max(RATIO_FLOOR_BYTES);
        let cap = size_cap.min(ratio_cap);
        let over_limit = || {
            if ratio_cap < size_cap {
                ArchiveError::RatioExceeded {
                    entry: raw.clone(),
                    limit: limits.max_ratio,
                }
            } else {
                ArchiveError::TooLarge {
                    entry: raw.clone(),
                    limit: size_cap,
                }
            }
        };
        if entry.size() > cap {
            return Err(over_limit());
        }

        let mut reader = Limited::new(entry, cap);
        let blob = match store.put_reader(&mut reader) {
            Ok(blob) => blob,
            Err(_) if reader.tripped => return Err(over_limit()),
            Err(error) => return Err(ArchiveError::Store(error)),
        };
        total = total.saturating_add(reader.consumed);
        files.push(IngestedFile {
            source,
            blob,
            size: reader.consumed,
        });
    }
    Ok(files)
}

/// A reader that fails once it has produced more than `remaining` bytes.
struct Limited<R> {
    inner: R,
    remaining: u64,
    consumed: u64,
    tripped: bool,
}

impl<R> Limited<R> {
    const fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
            consumed: 0,
            tripped: false,
        }
    }
}

impl<R: Read> Read for Limited<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        let produced = read as u64;
        if produced > self.remaining {
            self.tripped = true;
            return Err(io::Error::other("size limit exceeded"));
        }
        self.remaining -= produced;
        self.consumed += produced;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write,
        path::{Path, PathBuf},
    };

    use msbe_fsops::{Digest, Store};
    use tempfile::TempDir;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use super::{ArchiveError, Limits, ingest};

    struct Fixture {
        _dir: TempDir,
        store: Store,
        inputs: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let inputs = dir.path().join("inputs");
        fs::create_dir_all(&inputs).unwrap();
        Fixture {
            _dir: dir,
            store,
            inputs,
        }
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8])], method: CompressionMethod) {
        let mut writer = ZipWriter::new(fs::File::create(path).unwrap());
        for (name, bytes) in entries {
            let options = SimpleFileOptions::default().compression_method(method);
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn a_bare_file_is_stored_whole_even_when_it_is_zip_formatted() {
        let fx = fixture();
        let jar = fx.inputs.join("example-1.0.jar");
        write_zip(
            &jar,
            &[("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\n")],
            CompressionMethod::Stored,
        );
        let files = ingest(&fx.store, &jar, &Limits::default()).unwrap();
        let [file] = files.as_slice() else {
            panic!("expected one file, got {files:?}");
        };
        assert_eq!(file.source.as_str(), "example-1.0.jar");
        assert_eq!(file.blob, Digest::of_bytes(&fs::read(&jar).unwrap()));
    }

    #[test]
    fn a_zip_is_extracted_with_both_supported_compression_methods() {
        let fx = fixture();
        let stored = fx.inputs.join("stored.zip");
        let deflated = fx.inputs.join("deflated.zip");
        let entries: [(&str, &[u8]); 2] =
            [("readme.txt", b"hello"), ("nested/mod.jar", b"jar bytes")];
        write_zip(&stored, &entries, CompressionMethod::Stored);
        write_zip(&deflated, &entries, CompressionMethod::Deflated);

        for archive in [stored, deflated] {
            let files = ingest(&fx.store, &archive, &Limits::default()).unwrap();
            let sources: Vec<&str> = files.iter().map(|file| file.source.as_str()).collect();
            assert_eq!(sources, ["readme.txt", "nested/mod.jar"]);
            assert!(files.iter().all(|file| fx.store.contains(&file.blob)));
        }
    }

    #[test]
    fn traversal_absolute_and_platform_unsafe_names_are_refused() {
        let fx = fixture();
        for (index, bad) in ["../escape.txt", "/etc/passwd", "C:/boot.ini", "a\\b.txt"]
            .into_iter()
            .enumerate()
        {
            let archive = fx.inputs.join(format!("unsafe-{index}.zip"));
            write_zip(&archive, &[(bad, b"x")], CompressionMethod::Stored);
            let result = ingest(&fx.store, &archive, &Limits::default());
            assert!(
                matches!(result, Err(ArchiveError::UnsafePath { .. })),
                "{bad:?} gave {result:?}"
            );
        }
    }

    #[test]
    fn names_that_differ_only_in_case_are_refused() {
        let fx = fixture();
        let archive = fx.inputs.join("case.zip");
        write_zip(
            &archive,
            &[("Data/Texture.png", b"1"), ("data/texture.png", b"2")],
            CompressionMethod::Stored,
        );
        assert!(matches!(
            ingest(&fx.store, &archive, &Limits::default()),
            Err(ArchiveError::CaseCollision { .. })
        ));
    }

    #[test]
    fn a_decompression_bomb_stops_at_the_ratio_limit_and_stores_nothing() {
        let fx = fixture();
        let archive = fx.inputs.join("bomb.zip");
        let zeros = vec![0_u8; 8 << 20];
        write_zip(
            &archive,
            &[("zeros.bin", &zeros)],
            CompressionMethod::Deflated,
        );
        let limits = Limits {
            max_ratio: 10,
            ..Limits::default()
        };
        let result = ingest(&fx.store, &archive, &limits);
        assert!(
            matches!(result, Err(ArchiveError::RatioExceeded { .. })),
            "{result:?}"
        );
        assert!(!fx.store.contains(&Digest::of_bytes(&zeros)));
    }

    #[test]
    fn files_past_the_size_limit_are_refused_in_archives_and_as_bare_files() {
        let fx = fixture();
        let limits = Limits {
            max_file_bytes: 16,
            ..Limits::default()
        };
        let archive = fx.inputs.join("big.zip");
        write_zip(
            &archive,
            &[("big.bin", &[7_u8; 64])],
            CompressionMethod::Stored,
        );
        assert!(matches!(
            ingest(&fx.store, &archive, &limits),
            Err(ArchiveError::TooLarge { .. })
        ));

        let bare = fx.inputs.join("big.jar");
        fs::write(&bare, [7_u8; 64]).unwrap();
        assert!(matches!(
            ingest(&fx.store, &bare, &limits),
            Err(ArchiveError::TooLarge { .. })
        ));
    }

    #[test]
    fn too_many_entries_are_refused_before_any_are_read() {
        let fx = fixture();
        let archive = fx.inputs.join("many.zip");
        write_zip(
            &archive,
            &[("a", b"1"), ("b", b"2"), ("c", b"3")],
            CompressionMethod::Stored,
        );
        let limits = Limits {
            max_entries: 2,
            ..Limits::default()
        };
        assert!(matches!(
            ingest(&fx.store, &archive, &limits),
            Err(ArchiveError::TooManyEntries { limit: 2 })
        ));
    }
}
