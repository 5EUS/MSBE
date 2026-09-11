//! Shared verified artifact transfer primitives for reviewed provider adapters.

use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{
    hashing::HashingWriter,
    http::{HttpClient, HttpError},
};

/// The bytes and digests written during one completed artifact transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DownloadedArtifact {
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
    pub(crate) sha512: String,
}

/// Streams an artifact into a newly created path, syncing it before returning its digests.
pub(crate) fn download(
    http: &dyn HttpClient,
    url: &str,
    path: &Path,
    limit: u64,
) -> Result<DownloadedArtifact, ArtifactError> {
    let mut sink =
        HashingWriter::new(File::create_new(path).map_err(|source| ArtifactError::Io {
            path: path.to_owned(),
            source,
        })?);
    http.download(url, &mut sink, limit)?;
    sink.inner()
        .sync_all()
        .map_err(|source| ArtifactError::Io {
            path: path.to_owned(),
            source,
        })?;
    Ok(DownloadedArtifact {
        bytes: sink.written(),
        sha256: sink.sha256_hex(),
        sha512: sink.sha512_hex(),
    })
}

/// Why an artifact could not be safely transferred.
#[derive(Debug, Error)]
pub(crate) enum ArtifactError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("cannot write {}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::{ArtifactError, download};
    use crate::{HttpClient, HttpError, hashing::hex};
    use sha2::{Digest as _, Sha512};

    struct Serves(&'static [u8]);

    impl HttpClient for Serves {
        fn get(&self, url: &str, _: &[(&str, &str)], _: u64) -> Result<Vec<u8>, HttpError> {
            Err(HttpError::Status {
                url: url.to_owned(),
                status: 404,
            })
        }

        fn post_json(&self, url: &str, _: &[u8], _: u64) -> Result<Vec<u8>, HttpError> {
            Err(HttpError::Status {
                url: url.to_owned(),
                status: 404,
            })
        }

        fn download(&self, _: &str, sink: &mut dyn Write, _: u64) -> Result<u64, HttpError> {
            sink.write_all(self.0)
                .map_err(|error| HttpError::Transport {
                    url: "test".to_owned(),
                    message: error.to_string(),
                })?;
            Ok(u64::try_from(self.0.len()).unwrap_or_default())
        }
    }

    #[test]
    fn creates_a_new_synced_hashed_artifact() -> Result<(), ArtifactError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact.jar");
        let downloaded = download(&Serves(b"payload"), "https://example.test/a", &path, 1024)?;
        assert_eq!(downloaded.bytes, 7);
        assert_eq!(downloaded.sha512, hex(&Sha512::digest(b"payload")));
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
        assert!(matches!(
            download(&Serves(b"other"), "https://example.test/b", &path, 1024),
            Err(ArtifactError::Io { .. })
        ));
        Ok(())
    }
}
