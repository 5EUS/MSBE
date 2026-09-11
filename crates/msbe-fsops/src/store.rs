//! One content-addressed store shard.

use std::{
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
};

use crate::{
    atomic,
    digest::{Digest, StreamHasher},
    error::{Error, IoResultExt, Result},
    sys,
};

/// A content-addressed store shard.
///
/// Hardlinks and reflinks cannot cross volumes, so each volume that holds a managed
/// instance gets its own shard (see `docs/04-deployment-engine.md` §4.1). A blob is written
/// once, made read-only, and never modified.
#[derive(Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Opens the shard at `root`, creating its layout if needed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the directories cannot be created.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for dir in [root.join("blobs").join("sha256"), root.join("tmp")] {
            fs::create_dir_all(&dir).at("create directory", &dir)?;
        }
        Ok(Self { root })
    }

    /// The shard's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Scratch space on the shard's volume, for ingest and probes.
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Where a blob lives, whether or not it is present.
    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        let (head, tail) = digest.fanout();
        self.root.join("blobs").join("sha256").join(head).join(tail)
    }

    /// Whether the blob is present.
    pub fn contains(&self, digest: &Digest) -> bool {
        self.blob_path(digest).is_file()
    }

    /// Adds `bytes` to the store and returns their digest. Adding existing content is a
    /// no-op.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the blob cannot be written.
    pub fn put_bytes(&self, bytes: &[u8]) -> Result<Digest> {
        self.ingest(|out, hasher| {
            out.write_all(bytes)?;
            hasher.update(bytes);
            Ok(())
        })
    }

    /// Streams the file at `src` into the store, hashing as it copies. The source is only
    /// read, never moved or linked, so the blob cannot share an inode with it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the source cannot be read or the blob cannot be written.
    pub fn put_file(&self, src: &Path) -> Result<Digest> {
        let input = File::open(src).at("open", src)?;
        self.put_reader(input)
    }

    /// Streams everything `reader` yields into the store, hashing as it copies.
    ///
    /// A reader error aborts the ingest and leaves the store unchanged. Archive extraction
    /// depends on this: its size-limiting reader fails on purpose to stop a decompression
    /// bomb mid-stream.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if reading or writing fails.
    pub fn put_reader(&self, mut reader: impl Read) -> Result<Digest> {
        self.ingest(|out, hasher| {
            let mut buf = vec![0_u8; 64 * 1024];
            loop {
                let read = match reader.read(&mut buf) {
                    Ok(0) => return Ok(()),
                    Ok(read) => read,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                };
                let chunk = buf.get(..read).unwrap_or_default();
                out.write_all(chunk)?;
                hasher.update(chunk);
            }
        })
    }

    /// Opens a blob for reading.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingBlob`] if the blob is absent, or [`Error::Io`].
    pub fn open_blob(&self, digest: &Digest) -> Result<File> {
        let path = self.blob_path(digest);
        File::open(&path).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                Error::MissingBlob(*digest)
            } else {
                Error::Io {
                    op: "open",
                    path,
                    source,
                }
            }
        })
    }

    /// Re-hashes a blob and confirms it still matches its digest.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Corrupt`] on a mismatch, [`Error::MissingBlob`] if absent, or
    /// [`Error::Io`].
    pub fn verify(&self, digest: &Digest) -> Result<()> {
        let file = self.open_blob(digest)?;
        let actual = Digest::of_reader(BufReader::new(file)).at("read", &self.blob_path(digest))?;
        if actual == *digest {
            Ok(())
        } else {
            Err(Error::Corrupt {
                expected: *digest,
                actual,
            })
        }
    }

    fn ingest(
        &self,
        fill: impl FnOnce(&mut File, &mut StreamHasher) -> io::Result<()>,
    ) -> Result<Digest> {
        let tmp = self.tmp_dir().join(sys::unique_name("ingest"));
        let mut out = File::create_new(&tmp).at("create", &tmp)?;
        let mut hasher = StreamHasher::new();
        let filled = fill(&mut out, &mut hasher).and_then(|()| out.sync_all());
        drop(out);
        if let Err(source) = filled {
            sys::remove_file_if_exists(&tmp)?;
            return Err(Error::Io {
                op: "ingest",
                path: tmp,
                source,
            });
        }
        let digest = hasher.finish();
        let dest = self.blob_path(&digest);
        if dest.is_file() {
            match self.verify(&digest) {
                Ok(()) => {
                    sys::remove_file_if_exists(&tmp)?;
                    return Ok(digest);
                }
                // A damaged blob, or one removed since the check, is replaced by the bytes
                // just hashed. This is how adding a file again repairs the store.
                Err(Error::Corrupt { .. } | Error::MissingBlob(_)) => {}
                Err(error) => {
                    sys::remove_file_if_exists(&tmp)?;
                    return Err(error);
                }
            }
        }
        sys::make_read_only(&tmp)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).at("create directory", parent)?;
        }
        atomic::rename_displacing(&tmp, &dest)?;
        Ok(digest)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{self, Read},
    };

    use super::Store;
    use crate::{Digest, Error};

    #[test]
    fn put_bytes_deduplicates_and_makes_blobs_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let first = store.put_bytes(b"payload").unwrap();
        let second = store.put_bytes(b"payload").unwrap();
        assert_eq!(first, second);
        assert_eq!(first, Digest::of_bytes(b"payload"));
        let path = store.blob_path(&first);
        assert_eq!(fs::read(&path).unwrap(), b"payload");
        assert!(fs::metadata(&path).unwrap().permissions().readonly());
        assert_eq!(
            fs::read_dir(store.tmp_dir()).unwrap().count(),
            0,
            "ingest must not leave temp files"
        );
    }

    #[test]
    fn put_file_streams_files_larger_than_the_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let data: Vec<u8> = (0..300_000_u32)
            .map(|i| u8::try_from(i % 253).unwrap())
            .collect();
        let src = dir.path().join("big.bin");
        fs::write(&src, &data).unwrap();
        let digest = store.put_file(&src).unwrap();
        assert_eq!(digest, Digest::of_bytes(&data));
        store.verify(&digest).unwrap();
    }

    /// A reader that always fails, like a size limit tripping mid-stream.
    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("limit exceeded"))
        }
    }

    #[test]
    fn put_reader_ingests_streams_and_leaves_nothing_behind_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            store.put_reader(&b"streamed"[..]).unwrap(),
            Digest::of_bytes(b"streamed")
        );
        assert!(store.put_reader(FailingReader).is_err());
        assert_eq!(fs::read_dir(store.tmp_dir()).unwrap().count(), 0);
    }

    #[test]
    fn missing_blobs_are_reported_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let absent = Digest::of_bytes(b"never stored");
        assert!(matches!(store.verify(&absent), Err(Error::MissingBlob(d)) if d == absent));
    }

    #[cfg(unix)]
    #[test]
    fn verify_detects_corruption() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let digest = store.put_bytes(b"original").unwrap();
        let path = store.blob_path(&digest);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&path, b"tampered").unwrap();
        assert!(matches!(
            store.verify(&digest),
            Err(Error::Corrupt { expected, .. }) if expected == digest
        ));

        // Adding the original content again repairs the blob in place.
        assert_eq!(store.put_bytes(b"original").unwrap(), digest);
        store.verify(&digest).unwrap();
        assert!(fs::metadata(&path).unwrap().permissions().readonly());
        assert_eq!(fs::read_dir(store.tmp_dir()).unwrap().count(), 0);
    }
}
