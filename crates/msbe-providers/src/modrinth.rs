//! The Modrinth provider.
//!
//! Modrinth serves project metadata without authentication, publishes a dependency graph, and
//! hashes every file, which is why it is the first provider (`docs/06-providers-and-policy.md`).
//! Its API requires a uniquely identifying `User-Agent`, which `msbe-http` sets.

use std::{
    collections::{BTreeSet, VecDeque},
    fmt::{self, Write as _},
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
};

use msbe_fsops::RelPath;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha512};
use thiserror::Error;

use crate::http::{HttpClient, HttpError};

/// Modrinth's production API.
pub const API_BASE: &str = "https://api.modrinth.com/v2";

/// The most bytes a metadata response may be.
const METADATA_LIMIT: u64 = 16 << 20;

/// The most search results Modrinth returns in one request.
const MAX_SEARCH_LIMIT: u8 = 100;

/// What a mod must be compatible with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Acceptable loader ids: the instance's loader and every API it provides.
    pub loaders: Vec<String>,
    /// The game version, such as `1.21.1`.
    pub game_version: String,
}

/// A request for a project, optionally pinned to a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// The project's slug or id.
    pub project: String,
    /// A version number or version id to pin, if any.
    pub version: Option<String>,
}

impl Spec {
    /// Parses `project` or `project@version`.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError::InvalidSpec`] if either part is empty or contains a character
    /// that cannot appear in a slug, id or version number.
    pub fn parse(raw: &str) -> Result<Self, ModrinthError> {
        let (project, version) = match raw.split_once('@') {
            Some((project, version)) => (project, Some(version)),
            None => (raw, None),
        };
        if !is_reference(project) || version.is_some_and(|version| !is_reference(version)) {
            return Err(ModrinthError::InvalidSpec(raw.to_owned()));
        }
        Ok(Self {
            project: project.to_owned(),
            version: version.map(str::to_owned),
        })
    }
}

/// A Modrinth project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// The stable id.
    pub id: String,
    /// The current slug, which can change.
    pub slug: String,
    /// The display title.
    pub title: String,
    /// The kind of project, such as `mod`.
    pub project_type: String,
}

/// One published version of a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// The stable id.
    pub id: String,
    /// The project it belongs to.
    pub project_id: String,
    /// The version number, as its author wrote it.
    pub version_number: String,
    /// How stable it is.
    pub version_type: VersionType,
    /// When it was published, as an RFC 3339 timestamp.
    pub date_published: String,
    /// The loaders it supports.
    #[serde(default)]
    pub loaders: Vec<String>,
    /// The game versions it supports.
    #[serde(default)]
    pub game_versions: Vec<String>,
    /// Its downloadable files.
    #[serde(default)]
    pub files: Vec<VersionFile>,
    /// Its declared relationships to other projects.
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}

/// How stable a version is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionType {
    /// A stable release.
    Release,
    /// A pre-release.
    Beta,
    /// An early, unstable build.
    Alpha,
    /// A type this client does not know.
    #[serde(other)]
    Unknown,
}

/// A downloadable file of a version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionFile {
    /// Its published hashes.
    pub hashes: Hashes,
    /// Where to download it.
    pub url: String,
    /// Its file name.
    pub filename: String,
    /// Whether it is the version's main file.
    #[serde(default)]
    pub primary: bool,
    /// Its size in bytes.
    pub size: u64,
}

/// The hashes Modrinth publishes for a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hashes {
    /// SHA-512 as hex.
    pub sha512: String,
    /// SHA-1 as hex.
    #[serde(default)]
    pub sha1: Option<String>,
}

/// A version's relationship to another project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    /// A specific version, if pinned.
    #[serde(default)]
    pub version_id: Option<String>,
    /// The related project, if given.
    #[serde(default)]
    pub project_id: Option<String>,
    /// The kind of relationship.
    pub dependency_type: DependencyType,
}

/// The kind of relationship a dependency declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyType {
    /// The project must be installed too.
    Required,
    /// The project adds functionality if installed.
    Optional,
    /// The project must not be installed alongside.
    Incompatible,
    /// The project is bundled inside this file.
    Embedded,
    /// A kind this client does not know.
    #[serde(other)]
    Unknown,
}

/// A search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The project's stable id.
    pub project_id: String,
    /// The project's slug.
    pub slug: String,
    /// The display title.
    pub title: String,
    /// A short description.
    #[serde(default)]
    pub description: String,
    /// Total downloads.
    #[serde(default)]
    pub downloads: u64,
}

#[derive(Deserialize)]
struct SearchResults {
    hits: Vec<SearchHit>,
}

/// A version chosen for installation, and the file to install from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Selection {
    /// The project.
    pub project: Project,
    /// The chosen version.
    pub version: Version,
    /// The file to install.
    pub file: VersionFile,
    /// The slug of the project whose requirement brought this in, if any.
    pub required_by: Option<String>,
}

/// A declared relationship to a project, and the project that declared it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Requirement {
    /// The project referred to.
    pub project_id: String,
    /// The slug of the project that declared it.
    pub declared_by: String,
}

/// What installing a set of projects involves.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct InstallPlan {
    /// Every project to install, requested ones first.
    pub selections: Vec<Selection>,
    /// Required dependencies that were not selected.
    pub unresolved: Vec<Requirement>,
    /// Declared incompatibilities between selected projects.
    pub incompatible: Vec<Requirement>,
}

/// A Modrinth API client over any [`HttpClient`].
pub struct Modrinth<'a> {
    http: &'a dyn HttpClient,
    base: String,
}

impl fmt::Debug for Modrinth<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Modrinth")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl<'a> Modrinth<'a> {
    /// A client for the production API.
    pub fn new(http: &'a dyn HttpClient) -> Self {
        Self::with_base(http, API_BASE)
    }

    /// A client for another deployment of the API, such as staging.
    pub fn with_base(http: &'a dyn HttpClient, base: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into(),
        }
    }

    /// Searches for mods compatible with `target`.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError`] for a request or decoding failure.
    pub fn search(
        &self,
        query: &str,
        target: &Target,
        limit: u8,
    ) -> Result<Vec<SearchHit>, ModrinthError> {
        let mut facets = vec![
            vec!["project_type:mod".to_owned()],
            vec![format!("versions:{}", target.game_version)],
        ];
        if !target.loaders.is_empty() {
            facets.push(
                target
                    .loaders
                    .iter()
                    .map(|loader| format!("categories:{loader}"))
                    .collect(),
            );
        }
        let facets = serde_json::to_string(&facets).unwrap_or_default();
        let limit = limit.min(MAX_SEARCH_LIMIT).to_string();
        let results: SearchResults = self.get_json(
            "/search",
            &[("query", query), ("facets", &facets), ("limit", &limit)],
        )?;
        Ok(results.hits)
    }

    /// Fetches a project by slug or id.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError`] for an invalid reference, a request failure or bad JSON.
    pub fn project(&self, reference: &str) -> Result<Project, ModrinthError> {
        self.get_json(&format!("/project/{}", segment(reference)?), &[])
    }

    /// Lists a project's versions compatible with `target`, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError`] for an invalid reference, a request failure or bad JSON.
    pub fn versions(&self, project: &str, target: &Target) -> Result<Vec<Version>, ModrinthError> {
        let loaders = serde_json::to_string(&target.loaders).unwrap_or_default();
        let game_versions =
            serde_json::to_string(std::slice::from_ref(&target.game_version)).unwrap_or_default();
        let mut versions: Vec<Version> = self.get_json(
            &format!("/project/{}/version", segment(project)?),
            &[
                ("loaders", &loaders),
                ("game_versions", &game_versions),
                ("include_changelog", "false"),
            ],
        )?;
        versions.sort_by(|a, b| b.date_published.cmp(&a.date_published));
        Ok(versions)
    }

    /// Fetches one version by id.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError`] for an invalid id, a request failure or bad JSON.
    pub fn version(&self, id: &str) -> Result<Version, ModrinthError> {
        self.get_json(&format!("/version/{}", segment(id)?), &[])
    }

    /// Chooses the version of `spec` to install: the pinned one if given, otherwise the newest
    /// release, otherwise the newest version of any type.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError::NoMatchingVersion`] if nothing compatible matches.
    pub fn select(&self, spec: &Spec, target: &Target) -> Result<Selection, ModrinthError> {
        let project = self.project(&spec.project)?;
        let versions = self.versions(&project.id, target)?;
        let version = choose(&versions, spec.version.as_deref())
            .cloned()
            .ok_or_else(|| ModrinthError::NoMatchingVersion {
                project: project.slug.clone(),
                wanted: spec.version.clone(),
                loaders: target.loaders.clone(),
                game_version: target.game_version.clone(),
            })?;
        let file = primary_file(&version)?.clone();
        Ok(Selection {
            project,
            version,
            file,
            required_by: None,
        })
    }

    /// Selects `specs` and, when `with_dependencies` is set, every required dependency,
    /// transitively.
    ///
    /// This is a walk, not a solver: each project gets its newest compatible version, and
    /// conflicting version requirements between dependencies are not detected. The M2 solver
    /// replaces it.
    ///
    /// # Errors
    ///
    /// Returns the first selection or request error.
    pub fn plan_install(
        &self,
        specs: &[Spec],
        target: &Target,
        with_dependencies: bool,
    ) -> Result<InstallPlan, ModrinthError> {
        let mut queue: VecDeque<(Spec, Option<String>)> =
            specs.iter().cloned().map(|spec| (spec, None)).collect();
        let mut chosen = BTreeSet::new();
        let mut plan = InstallPlan::default();
        while let Some((spec, required_by)) = queue.pop_front() {
            let mut selection = self.select(&spec, target)?;
            if !chosen.insert(selection.project.id.clone()) {
                continue;
            }
            for dependency in &selection.version.dependencies {
                if !matches!(
                    dependency.dependency_type,
                    DependencyType::Required | DependencyType::Incompatible
                ) {
                    continue;
                }
                let project_id = match (&dependency.project_id, &dependency.version_id) {
                    (Some(id), _) => id.clone(),
                    (None, Some(version)) => self.version(version)?.project_id,
                    (None, None) => continue,
                };
                let requirement = Requirement {
                    project_id,
                    declared_by: selection.project.slug.clone(),
                };
                match dependency.dependency_type {
                    DependencyType::Required if with_dependencies => queue.push_back((
                        Spec {
                            project: requirement.project_id,
                            version: dependency.version_id.clone(),
                        },
                        Some(requirement.declared_by),
                    )),
                    DependencyType::Required => plan.unresolved.push(requirement),
                    _ => plan.incompatible.push(requirement),
                }
            }
            selection.required_by = required_by;
            plan.selections.push(selection);
        }
        plan.unresolved
            .retain(|requirement| !chosen.contains(&requirement.project_id));
        plan.incompatible
            .retain(|requirement| chosen.contains(&requirement.project_id));
        Ok(plan)
    }

    /// Downloads `file` into `dir`, verifying its size and SHA-512 before returning its path.
    ///
    /// Only `https` URLs are accepted, and the file name must be a single safe component. A file
    /// that fails verification is left in `dir`, which the caller owns and discards.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError`] for an unsafe URL or name, a transfer failure, or a mismatch.
    pub fn download(&self, file: &VersionFile, dir: &Path) -> Result<PathBuf, ModrinthError> {
        if !file.url.starts_with("https://") {
            return Err(ModrinthError::InsecureUrl(file.url.clone()));
        }
        let name = RelPath::new(&file.filename)
            .ok()
            .filter(|name| !name.as_str().contains('/'))
            .ok_or_else(|| ModrinthError::UnsafeFileName(file.filename.clone()))?;
        let path = dir.join(name.as_str());
        let io_error = |source| ModrinthError::Io {
            path: path.clone(),
            source,
        };
        let output = File::create_new(&path).map_err(io_error)?;
        let mut sink = HashingWriter {
            inner: output,
            hasher: Sha512::new(),
            written: 0,
        };
        self.http.download(&file.url, &mut sink, file.size)?;
        sink.inner.sync_all().map_err(io_error)?;

        if sink.written != file.size {
            return Err(ModrinthError::SizeMismatch {
                file: file.filename.clone(),
                expected: file.size,
                actual: sink.written,
            });
        }
        let actual = hex(&sink.hasher.finalize());
        if !actual.eq_ignore_ascii_case(&file.hashes.sha512) {
            return Err(ModrinthError::HashMismatch {
                file: file.filename.clone(),
                expected: file.hashes.sha512.clone(),
                actual,
            });
        }
        Ok(path)
    }

    fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, ModrinthError> {
        let url = format!("{}{path}", self.base);
        let body = self.http.get(&url, query, METADATA_LIMIT)?;
        serde_json::from_slice(&body).map_err(|error| ModrinthError::Decode {
            url,
            reason: error.to_string(),
        })
    }
}

/// Why a Modrinth operation failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ModrinthError {
    /// The request failed.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// A response did not have the expected shape.
    #[error("unexpected response from {url}: {reason}")]
    Decode {
        /// The request URL.
        url: String,
        /// The decoder's message.
        reason: String,
    },

    /// A project or version reference is malformed.
    #[error("{0:?} is not a valid Modrinth project or version reference")]
    InvalidSpec(String),

    /// No version matched the request and the target.
    #[error(
        "no {} version of {project} supports loaders {loaders:?} on game version {game_version}",
        .wanted.as_deref().map_or("compatible", |_| "matching")
    )]
    NoMatchingVersion {
        /// The project slug.
        project: String,
        /// The pinned version, if any.
        wanted: Option<String>,
        /// The acceptable loaders.
        loaders: Vec<String>,
        /// The game version.
        game_version: String,
    },

    /// A version has no files.
    #[error("version {version} has no files")]
    NoFiles {
        /// The version id.
        version: String,
    },

    /// A download's file name is not a single safe path component.
    #[error("refusing to write a download named {0:?}")]
    UnsafeFileName(String),

    /// A download URL is not `https`.
    #[error("refusing to download over an insecure URL: {0}")]
    InsecureUrl(String),

    /// A download is not the size Modrinth published.
    #[error("{file} is {actual} bytes, but Modrinth published {expected}")]
    SizeMismatch {
        /// The file name.
        file: String,
        /// The published size.
        expected: u64,
        /// The downloaded size.
        actual: u64,
    },

    /// A download does not match its published SHA-512.
    #[error("{file} failed SHA-512 verification: expected {expected}, got {actual}")]
    HashMismatch {
        /// The file name.
        file: String,
        /// The published hash.
        expected: String,
        /// The hash of what was downloaded.
        actual: String,
    },

    /// A download could not be written.
    #[error("cannot write {}: {source}", .path.display())]
    Io {
        /// The destination.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

/// Hashes everything written through it.
struct HashingWriter<W> {
    inner: W,
    hasher: Sha512,
    written: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let accepted = self.inner.write(buf)?;
        self.hasher.update(buf.get(..accepted).unwrap_or_default());
        self.written += accepted as u64;
        Ok(accepted)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Whether `raw` can be a slug, id or version number, and is safe as one URL path segment.
fn is_reference(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 128
        && raw.chars().any(|c| c.is_ascii_alphanumeric())
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
}

fn segment(raw: &str) -> Result<&str, ModrinthError> {
    if is_reference(raw) {
        Ok(raw)
    } else {
        Err(ModrinthError::InvalidSpec(raw.to_owned()))
    }
}

fn choose<'v>(versions: &'v [Version], wanted: Option<&str>) -> Option<&'v Version> {
    match wanted {
        Some(wanted) => versions
            .iter()
            .find(|version| version.id == wanted || version.version_number == wanted),
        None => versions
            .iter()
            .find(|version| version.version_type == VersionType::Release)
            .or_else(|| versions.first()),
    }
}

fn primary_file(version: &Version) -> Result<&VersionFile, ModrinthError> {
    version
        .files
        .iter()
        .find(|file| file.primary)
        .or_else(|| version.files.first())
        .ok_or_else(|| ModrinthError::NoFiles {
            version: version.id.clone(),
        })
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            // Formatting into a String cannot fail.
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap, io::Write};

    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha512};

    use super::{Modrinth, ModrinthError, Spec, Target, VersionType, hex};
    use crate::http::{HttpClient, HttpError};

    const BASE: &str = "https://api.modrinth.com/v2";

    #[derive(Default)]
    struct FakeHttp {
        json: BTreeMap<String, Value>,
        files: BTreeMap<String, Vec<u8>>,
        requests: RefCell<Vec<String>>,
    }

    impl FakeHttp {
        fn route(&mut self, path: &str, body: Value) {
            self.json.insert(format!("{BASE}{path}"), body);
        }
    }

    impl HttpClient for FakeHttp {
        fn get(
            &self,
            url: &str,
            query: &[(&str, &str)],
            _limit: u64,
        ) -> Result<Vec<u8>, HttpError> {
            let rendered: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();
            self.requests
                .borrow_mut()
                .push(format!("{url}?{}", rendered.join("&")));
            self.json
                .get(url)
                .map(|body| serde_json::to_vec(body).unwrap())
                .ok_or_else(|| HttpError::Status {
                    url: url.to_owned(),
                    status: 404,
                })
        }

        fn download(&self, url: &str, sink: &mut dyn Write, limit: u64) -> Result<u64, HttpError> {
            let bytes = self.files.get(url).ok_or_else(|| HttpError::Status {
                url: url.to_owned(),
                status: 404,
            })?;
            let size = u64::try_from(bytes.len()).unwrap();
            if size > limit {
                return Err(HttpError::TooLarge {
                    url: url.to_owned(),
                    limit,
                });
            }
            sink.write_all(bytes).unwrap();
            Ok(size)
        }
    }

    fn contents(file: &str) -> Vec<u8> {
        format!("contents of {file}").into_bytes()
    }

    fn target() -> Target {
        Target {
            loaders: vec!["fabric".to_owned()],
            game_version: "1.21.1".to_owned(),
        }
    }

    /// A version as Modrinth returns it, including fields this client ignores.
    fn version(id: &str, project: &str, number: &str, kind: &str, date: &str, file: &str) -> Value {
        let bytes = contents(file);
        json!({
            "id": id,
            "project_id": project,
            "version_number": number,
            "version_type": kind,
            "date_published": date,
            "loaders": ["fabric"],
            "game_versions": ["1.21.1"],
            "author_id": "ignored",
            "changelog": null,
            "downloads": 10,
            "files": [{
                "hashes": { "sha512": hex(&Sha512::digest(&bytes)), "sha1": "unused" },
                "url": format!("https://cdn.modrinth.test/{file}"),
                "filename": file,
                "primary": true,
                "size": bytes.len(),
                "file_type": null
            }],
            "dependencies": []
        })
    }

    /// Sodium (a newer beta and an older release) and Iris, which requires Sodium.
    fn catalogue() -> FakeHttp {
        let mut http = FakeHttp::default();
        let sodium =
            json!({ "id": "AANobbMI", "slug": "sodium", "title": "Sodium", "project_type": "mod" });
        http.route("/project/sodium", sodium.clone());
        http.route("/project/AANobbMI", sodium);
        http.route(
            "/project/AANobbMI/version",
            json!([
                version(
                    "S1",
                    "AANobbMI",
                    "0.8.12",
                    "release",
                    "2026-07-06T00:00:00Z",
                    "sodium-0.8.12.jar"
                ),
                version(
                    "S2",
                    "AANobbMI",
                    "0.8.13-beta.1",
                    "beta",
                    "2026-08-07T00:00:00Z",
                    "sodium-beta.jar"
                ),
            ]),
        );
        let mut iris = version(
            "I1",
            "YL57xq9U",
            "1.8.0",
            "release",
            "2026-08-01T00:00:00Z",
            "iris-1.8.0.jar",
        );
        iris.as_object_mut().unwrap().insert(
            "dependencies".to_owned(),
            json!([{ "project_id": "AANobbMI", "version_id": null, "dependency_type": "required" }]),
        );
        http.route(
            "/project/iris",
            json!({ "id": "YL57xq9U", "slug": "iris", "title": "Iris", "project_type": "mod" }),
        );
        http.route("/project/YL57xq9U/version", json!([iris]));
        for file in ["sodium-0.8.12.jar", "sodium-beta.jar", "iris-1.8.0.jar"] {
            http.files
                .insert(format!("https://cdn.modrinth.test/{file}"), contents(file));
        }
        http
    }

    #[test]
    fn specs_accept_slugs_ids_and_pins_and_reject_path_characters() {
        assert_eq!(
            Spec::parse("sodium").unwrap(),
            Spec {
                project: "sodium".to_owned(),
                version: None
            }
        );
        assert_eq!(
            Spec::parse("sodium@mc1.21.1-0.8.13+fabric")
                .unwrap()
                .version
                .as_deref(),
            Some("mc1.21.1-0.8.13+fabric")
        );
        for bad in ["", "@1.0", "sodium@", "../etc", "a/b", "..", "sodium@1 0"] {
            assert!(Spec::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn select_prefers_the_newest_release_and_honours_pins() {
        let http = catalogue();
        let modrinth = Modrinth::new(&http);
        let chosen = modrinth
            .select(&Spec::parse("sodium").unwrap(), &target())
            .unwrap();
        assert_eq!(chosen.version.version_number, "0.8.12");
        assert_eq!(chosen.version.version_type, VersionType::Release);

        let pinned = modrinth
            .select(&Spec::parse("sodium@0.8.13-beta.1").unwrap(), &target())
            .unwrap();
        assert_eq!(pinned.version.id, "S2");

        assert!(matches!(
            modrinth.select(&Spec::parse("sodium@9.9.9").unwrap(), &target()),
            Err(ModrinthError::NoMatchingVersion { .. })
        ));
        assert!(http.requests.borrow().iter().any(|request| {
            request.contains("/project/AANobbMI/version?")
                && request.contains(r#"loaders=["fabric"]"#)
                && request.contains(r#"game_versions=["1.21.1"]"#)
                && request.contains("include_changelog=false")
        }));
    }

    #[test]
    fn required_dependencies_are_walked_only_when_asked() {
        let http = catalogue();
        let modrinth = Modrinth::new(&http);
        let iris = [Spec::parse("iris").unwrap()];

        let with = modrinth.plan_install(&iris, &target(), true).unwrap();
        let chosen: Vec<(&str, Option<&str>)> = with
            .selections
            .iter()
            .map(|selection| {
                (
                    selection.project.slug.as_str(),
                    selection.required_by.as_deref(),
                )
            })
            .collect();
        assert_eq!(chosen, [("iris", None), ("sodium", Some("iris"))]);
        assert!(with.unresolved.is_empty());

        let without = modrinth.plan_install(&iris, &target(), false).unwrap();
        assert_eq!(without.selections.len(), 1);
        let [missing] = without.unresolved.as_slice() else {
            panic!(
                "expected one unresolved requirement, got {:?}",
                without.unresolved
            );
        };
        assert_eq!(
            (missing.project_id.as_str(), missing.declared_by.as_str()),
            ("AANobbMI", "iris")
        );
    }

    #[test]
    fn a_project_reached_twice_is_selected_once() {
        let http = catalogue();
        let plan = Modrinth::new(&http)
            .plan_install(
                &[Spec::parse("sodium").unwrap(), Spec::parse("iris").unwrap()],
                &target(),
                true,
            )
            .unwrap();
        let slugs: Vec<&str> = plan
            .selections
            .iter()
            .map(|selection| selection.project.slug.as_str())
            .collect();
        assert_eq!(slugs, ["sodium", "iris"]);
    }

    #[test]
    fn downloads_are_verified_before_they_are_trusted() {
        let mut http = catalogue();
        let sodium = Modrinth::new(&http)
            .select(&Spec::parse("sodium").unwrap(), &target())
            .unwrap();
        let good = tempfile::tempdir().unwrap();
        let path = Modrinth::new(&http)
            .download(&sodium.file, good.path())
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), contents("sodium-0.8.12.jar"));

        // Same size, one bit different: only the hash can catch it.
        let mut tampered = contents("sodium-0.8.12.jar");
        if let Some(last) = tampered.last_mut() {
            *last ^= 1;
        }
        http.files.insert(sodium.file.url.clone(), tampered);
        let bad = tempfile::tempdir().unwrap();
        assert!(matches!(
            Modrinth::new(&http).download(&sodium.file, bad.path()),
            Err(ModrinthError::HashMismatch { .. })
        ));

        let mut insecure = sodium.file.clone();
        insecure.url = insecure.url.replacen("https://", "http://", 1);
        assert!(matches!(
            Modrinth::new(&http).download(&insecure, bad.path()),
            Err(ModrinthError::InsecureUrl(_))
        ));

        let mut escaping = sodium.file;
        escaping.filename = "../escape.jar".to_owned();
        assert!(matches!(
            Modrinth::new(&http).download(&escaping, bad.path()),
            Err(ModrinthError::UnsafeFileName(_))
        ));
    }

    #[test]
    fn search_asks_for_mods_compatible_with_the_target() {
        let mut http = FakeHttp::default();
        http.route(
            "/search",
            json!({
                "hits": [{ "project_id": "AANobbMI", "slug": "sodium", "title": "Sodium",
                           "description": "A rendering engine", "downloads": 42, "author": "ignored" }],
                "offset": 0, "limit": 5, "total_hits": 1
            }),
        );
        let hits = Modrinth::new(&http).search("render", &target(), 5).unwrap();
        assert_eq!(hits.first().map(|hit| hit.slug.as_str()), Some("sodium"));

        let requests = http.requests.borrow();
        let request = requests.first().unwrap();
        assert!(
            request.contains(
                r#"facets=[["project_type:mod"],["versions:1.21.1"],["categories:fabric"]]"#
            ),
            "{request}"
        );
        assert!(request.contains("limit=5"), "{request}");
    }
}
