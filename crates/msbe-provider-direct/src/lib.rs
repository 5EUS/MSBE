//! Direct downloads: an `https` URL, optionally pinned to a checksum.
//!
//! The manual escape hatch from `docs/06-providers-and-policy.md`. A direct URL has no metadata
//! to trust, so the URL is the mod's identity and the file it names is the whole request: nothing
//! is resolved. A checksum pinned in the fragment (`#sha256=...` or `#sha512=...`) is verified
//! when present, and the SHA-512 of what was fetched is recorded either way.

use std::collections::BTreeMap;

use msbe_core::instance::NativeExtensionIdentity;
use msbe_fsops::RelPath;
use msbe_provider_api::{
    AcquiredArtifact, Adapter, AdapterError, Availability, PackageId, Provenance, Registration,
    model::{Channel, Project, Release, ReleaseFile, Request, Selection},
};
use thiserror::Error;

/// The provider id the direct URL manifest declares.
pub const ID: &str = "url";

const IDENTITY: NativeExtensionIdentity = NativeExtensionIdentity {
    id: ID,
    version: env!("CARGO_PKG_VERSION"),
    host_api_minimum: 1,
    host_api_maximum: 1,
    signer: "msbe-build",
};

/// How direct URLs join MSBE.
pub const REGISTRATION: Registration = Registration {
    id: ID,
    identity: IDENTITY,
    manifest: include_str!("../manifest.toml"),
    overlay: &[],
    build: |_| Ok(Box::new(Direct)),
    pack_codecs: &[],
    exception_reason: "Legacy native adapter retained while the declarative direct-url runtime is introduced.",
};

/// The direct URL adapter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Direct;

impl Adapter for Direct {
    fn id(&self) -> &str {
        ID
    }

    fn request(&self, reference: &str) -> Result<Request, AdapterError> {
        let source = DirectSource::parse(reference).map_err(AdapterError::specific)?;
        Ok(Request::File(Box::new(source.selection())))
    }

    /// A URL has no release id, so the SHA-512 of what was fetched stands in for one.
    fn provenance(&self, release: &Release, acquired: &AcquiredArtifact) -> Provenance {
        Provenance {
            provider: ID.to_owned(),
            project: release.project.project.clone(),
            version: acquired.sha512.clone(),
            version_number: release.number.clone(),
            hashes: BTreeMap::from([
                ("sha256".to_owned(), acquired.sha256.clone()),
                ("sha512".to_owned(), acquired.sha512.clone()),
            ]),
        }
    }
}

/// A checksum pinned in a URL's fragment, as lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checksum {
    /// SHA-256.
    Sha256(String),
    /// SHA-512.
    Sha512(String),
}

/// A direct download, parsed from its URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectSource {
    /// The URL to fetch, without its fragment.
    pub url: String,
    /// The name the file is saved under: the last segment of the URL's path, percent-decoded.
    pub file_name: String,
    /// The checksum the download must match, if the URL pins one.
    pub checksum: Option<Checksum>,
}

impl DirectSource {
    /// Parses `https://host/path/file[?query][#sha256=<hex>|#sha512=<hex>]`.
    ///
    /// # Errors
    ///
    /// Returns [`DirectError`] for a URL that is not `https`, does not end in a usable file
    /// name, or pins a malformed checksum.
    pub fn parse(raw: &str) -> Result<Self, DirectError> {
        let (url, fragment) = match raw.split_once('#') {
            Some((url, fragment)) => (url, Some(fragment)),
            None => (raw, None),
        };
        let Some(rest) = url.strip_prefix("https://") else {
            return Err(DirectError::Insecure(raw.to_owned()));
        };
        let without_query = rest.split('?').next().unwrap_or_default();
        let file_name = without_query
            .split_once('/')
            .and_then(|(host, path)| (!host.is_empty()).then_some(path))
            .and_then(|path| path.rsplit('/').next())
            .and_then(percent_decode)
            .filter(|name| {
                !name.starts_with('.') && !name.contains('/') && RelPath::new(name).is_ok()
            })
            .ok_or_else(|| DirectError::NoFileName(raw.to_owned()))?;
        Ok(Self {
            url: url.to_owned(),
            file_name,
            checksum: fragment.map(parse_checksum).transpose()?,
        })
    }

    /// The file this source names, as a selection that needs no resolution.
    pub fn selection(&self) -> Selection {
        let package = PackageId {
            provider: ID.to_owned(),
            project: self.url.clone(),
        };
        let (sha256, sha512) = match &self.checksum {
            Some(Checksum::Sha256(digest)) => (Some(digest.clone()), None),
            Some(Checksum::Sha512(digest)) => (None, Some(digest.clone())),
            None => (None, None),
        };
        let file = ReleaseFile {
            url: self.url.clone(),
            name: self.file_name.clone(),
            size: None,
            sha256,
            sha512,
            primary: true,
        };
        Selection {
            // A URL carries no side metadata; the user who entered it vouches for it.
            project: Project {
                id: package.clone(),
                slug: None,
                title: self.file_name.clone(),
                client: Availability::Optional,
                server: Availability::Optional,
            },
            release: Release {
                id: self.url.clone(),
                project: package,
                number: self.file_name.clone(),
                channel: Channel::Unknown,
                published: String::new(),
                files: vec![file.clone()],
                dependencies: Vec::new(),
            },
            file,
            required_by: None,
        }
    }
}

/// Decodes `%XX` escapes. Returns `None` for a malformed escape or a result that is not UTF-8.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let escape = bytes
                .get(index + 1..index + 3)
                .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))?;
            decoded.push(u8::from_str_radix(std::str::from_utf8(escape).ok()?, 16).ok()?);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn parse_checksum(fragment: &str) -> Result<Checksum, DirectError> {
    let invalid = || DirectError::InvalidChecksum(fragment.to_owned());
    let (algorithm, digest) = fragment.split_once('=').ok_or_else(invalid)?;
    let hex_of_length =
        |length| digest.len() == length && digest.chars().all(|c| c.is_ascii_hexdigit());
    match algorithm {
        "sha256" if hex_of_length(64) => Ok(Checksum::Sha256(digest.to_ascii_lowercase())),
        "sha512" if hex_of_length(128) => Ok(Checksum::Sha512(digest.to_ascii_lowercase())),
        _ => Err(invalid()),
    }
}

/// Why a direct URL was refused before anything was fetched.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DirectError {
    /// The URL is not `https`.
    #[error("refusing to download over an insecure URL: {0}")]
    Insecure(String),

    /// The URL does not end in a file name.
    #[error("{0} does not end in a file name to save the download as")]
    NoFileName(String),

    /// The fragment is not a checksum this client understands.
    #[error(
        "{0:?} is not a checksum; pin one as #sha256=<64 hex digits> or #sha512=<128 hex digits>"
    )]
    InvalidChecksum(String),
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use msbe_provider_api::{AcquisitionError, Adapter, AdapterError, HttpClient, HttpError, hex};
    use sha2::{Digest as _, Sha256, Sha512};

    use super::{Checksum, Direct, DirectError, DirectSource};

    /// Serves the same bytes for every download.
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

        fn download(
            &self,
            _url: &str,
            sink: &mut dyn Write,
            _limit: u64,
        ) -> Result<u64, HttpError> {
            sink.write_all(self.0).unwrap();
            Ok(u64::try_from(self.0.len()).unwrap())
        }
    }

    #[test]
    fn urls_must_be_https_name_a_file_and_pin_well_formed_checksums() {
        let raw = format!(
            "https://cdn.example/mods/extra-1.0.jar?download=1#sha256={}",
            "AB".repeat(32)
        );
        assert_eq!(
            DirectSource::parse(&raw).unwrap(),
            DirectSource {
                url: "https://cdn.example/mods/extra-1.0.jar?download=1".to_owned(),
                file_name: "extra-1.0.jar".to_owned(),
                checksum: Some(Checksum::Sha256("ab".repeat(32))),
            }
        );
        // Found against the Modrinth CDN, whose file names escape `+`.
        let encoded =
            DirectSource::parse("https://cdn.example/v/sodium-fabric-0.8.13%2Bmc1.21.1.jar")
                .unwrap();
        assert_eq!(encoded.file_name, "sodium-fabric-0.8.13+mc1.21.1.jar");
        assert_eq!(
            encoded.url,
            "https://cdn.example/v/sodium-fabric-0.8.13%2Bmc1.21.1.jar"
        );
        assert!(matches!(
            DirectSource::parse("http://cdn.example/a.jar"),
            Err(DirectError::Insecure(_))
        ));
        for no_name in [
            "https://cdn.example",
            "https://cdn.example/dir/",
            "https://cdn.example/.hidden",
            "https:///a.jar",
            "https://cdn.example/escape%2F..%2Fa.jar",
            "https://cdn.example/%2e%2e",
            "https://cdn.example/bad%zz.jar",
            "https://cdn.example/truncated%2",
        ] {
            assert!(
                matches!(
                    DirectSource::parse(no_name),
                    Err(DirectError::NoFileName(_))
                ),
                "{no_name}"
            );
        }
        let short = format!("https://cdn.example/a.jar#sha256={}", "a".repeat(63));
        for bad in ["https://cdn.example/a.jar#md5=abc", short.as_str()] {
            assert!(
                matches!(
                    DirectSource::parse(bad),
                    Err(DirectError::InvalidChecksum(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_pinned_checksum_is_verified_and_all_computed_hashes_are_recorded() {
        let good = format!(
            "https://cdn.example/extra.jar#sha256={}",
            hex(&Sha256::digest(b"payload"))
        );
        let selection = DirectSource::parse(&good).unwrap().selection();
        let dir = tempfile::tempdir().unwrap();
        let acquired = Direct
            .acquire(&Serves(b"payload"), &selection.file, dir.path())
            .unwrap();
        assert_eq!(std::fs::read(&acquired.path).unwrap(), b"payload");
        assert_eq!(acquired.size, 7);
        let provenance = Direct.provenance(&selection.release, &acquired);
        assert_eq!(
            provenance.hashes.get("sha512"),
            Some(&hex(&Sha512::digest(b"payload")))
        );
        assert_eq!(provenance.version, acquired.sha512);
        assert_eq!(
            (
                provenance.project.as_str(),
                provenance.version_number.as_str()
            ),
            ("https://cdn.example/extra.jar", "extra.jar")
        );

        let bad = format!("https://cdn.example/extra.jar#sha512={}", "0".repeat(128));
        let other = tempfile::tempdir().unwrap();
        let result = Direct.acquire(
            &Serves(b"payload"),
            &DirectSource::parse(&bad).unwrap().selection().file,
            other.path(),
        );
        assert!(
            matches!(
                result,
                Err(AdapterError::Acquisition(AcquisitionError::HashMismatch {
                    algorithm: "SHA-512",
                    ..
                }))
            ),
            "{result:?}"
        );
    }
}
