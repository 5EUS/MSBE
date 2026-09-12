//! Reviewed interpreters for the closed declarative provider-program vocabulary.

use std::{error::Error, fmt};

use msbe_provider_api::{
    Adapter, AdapterError, Availability, Capability, HttpClient, PackageId, ProviderProgram,
    Releases, RuntimeKind, Search, Target,
    model::{
        Channel, Dependency, DependencyKind, Project, Release, ReleaseFile, Request, SearchResult,
        Selection,
    },
    program::{DependencyMapping, FileMapping},
};
use serde_json::Value;

const JSON_LIMIT: u64 = 1_048_576;

/// Builds an adapter from a structurally validated provider program.
pub(super) fn build(program: ProviderProgram) -> Box<dyn Adapter> {
    // Both runtimes share one adapter; `runtime` selects its behavior per operation.
    match program.runtime {
        RuntimeKind::DirectUrlV1 | RuntimeKind::CatalogV1 => Box::new(ProgramAdapter { program }),
    }
}

#[derive(Debug)]
struct ProgramAdapter {
    program: ProviderProgram,
}

impl ProgramAdapter {
    fn catalog(&self) -> Result<Catalog<'_>, AdapterError> {
        let base = self
            .program
            .provider
            .api_base()
            .ok_or_else(|| AdapterError::specific(RuntimeError::MissingMetadata))?;
        Ok(Catalog {
            program: &self.program,
            base,
        })
    }
}

impl Adapter for ProgramAdapter {
    fn id(&self) -> &str {
        &self.program.provider.id
    }

    fn request(&self, reference: &str) -> Result<Request, AdapterError> {
        match self.program.runtime {
            RuntimeKind::DirectUrlV1 => direct_selection(&self.program.provider.id, reference)
                .map(|selection| Request::File(Box::new(selection)))
                .map_err(AdapterError::specific),
            RuntimeKind::CatalogV1 => {
                let (reference, version) = reference
                    .split_once('@')
                    .map_or((reference, None), |(project, version)| {
                        (project, Some(version))
                    });
                safe_component(reference)?;
                if version.is_some_and(|version| safe_component(version).is_err()) {
                    return Err(AdapterError::specific(RuntimeError::UnsafeReference));
                }
                Ok(Request::Project {
                    reference: reference.to_owned(),
                    version: version.map(str::to_owned),
                })
            }
        }
    }

    fn as_search(&self) -> Option<&dyn Search> {
        self.program
            .capabilities
            .contains(&Capability::Search)
            .then_some(self)
    }
    fn as_releases(&self) -> Option<&dyn Releases> {
        (self.program.capabilities.contains(&Capability::Project)
            && self.program.capabilities.contains(&Capability::Releases))
        .then_some(self)
    }
}

impl Search for ProgramAdapter {
    fn search(
        &self,
        http: &dyn HttpClient,
        query: &str,
        _: &Target,
        limit: u8,
    ) -> Result<Vec<SearchResult>, AdapterError> {
        let catalog = self.catalog()?;
        let route = catalog
            .program
            .routes
            .search
            .as_deref()
            .ok_or_else(|| AdapterError::specific(RuntimeError::MissingRoute))?;
        let body = catalog.get(http, route, &[("q", query), ("limit", &limit.to_string())])?;
        let items = pointer(
            &body,
            catalog
                .program
                .mappings
                .search_items
                .as_deref()
                .ok_or_else(|| AdapterError::specific(RuntimeError::MissingMapping))?,
        )?
        .as_array()
        .ok_or_else(|| AdapterError::specific(RuntimeError::ExpectedArray))?;
        items
            .iter()
            .map(|item| catalog.search_result(item))
            .collect()
    }
}

impl Releases for ProgramAdapter {
    fn project(&self, http: &dyn HttpClient, reference: &str) -> Result<Project, AdapterError> {
        safe_component(reference)?;
        let catalog = self.catalog()?;
        let route = interpolate(
            catalog
                .program
                .routes
                .project
                .as_deref()
                .ok_or_else(|| AdapterError::specific(RuntimeError::MissingRoute))?,
            "reference",
            reference,
        )?;
        catalog.project(&catalog.get(http, &route, &[])?)
    }

    fn releases(
        &self,
        http: &dyn HttpClient,
        project: &str,
        target: &Target,
    ) -> Result<Vec<Release>, AdapterError> {
        safe_component(project)?;
        let catalog = self.catalog()?;
        let route = interpolate(
            catalog
                .program
                .routes
                .releases
                .as_deref()
                .ok_or_else(|| AdapterError::specific(RuntimeError::MissingRoute))?,
            "project",
            project,
        )?;
        let body = catalog.get(http, &route, &[])?;
        let releases = body
            .as_array()
            .ok_or_else(|| AdapterError::specific(RuntimeError::ExpectedArray))?;
        releases
            .iter()
            .filter_map(|release| catalog.release(release, target).transpose())
            .collect()
    }

    fn release_project(&self, _: &dyn HttpClient, _: &str) -> Result<PackageId, AdapterError> {
        Err(AdapterError::specific(
            RuntimeError::UnsupportedReleaseProject,
        ))
    }
}

struct Catalog<'a> {
    program: &'a ProviderProgram,
    base: &'a str,
}
impl Catalog<'_> {
    fn get(
        &self,
        http: &dyn HttpClient,
        route: &str,
        query: &[(&str, &str)],
    ) -> Result<Value, AdapterError> {
        let url = format!("{}{}", self.base.trim_end_matches('/'), route);
        let bytes = http
            .get(&url, query, JSON_LIMIT)
            .map_err(AdapterError::specific)?;
        serde_json::from_slice(&bytes)
            .map_err(|error| AdapterError::specific(RuntimeError::Json(error.to_string())))
    }
    fn text(value: &Value, pointer_value: Option<&str>) -> Result<String, AdapterError> {
        pointer(
            value,
            pointer_value.ok_or_else(|| AdapterError::specific(RuntimeError::MissingMapping))?,
        )?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| AdapterError::specific(RuntimeError::ExpectedText))
    }
    fn project(&self, value: &Value) -> Result<Project, AdapterError> {
        let map = &self.program.mappings.project;
        Ok(Project {
            id: PackageId {
                provider: self.program.provider.id.clone(),
                project: Self::text(value, map.id.as_deref())?,
            },
            slug: map
                .slug
                .as_deref()
                .map(|path| Self::text(value, Some(path)))
                .transpose()?,
            title: Self::text(value, map.title.as_deref())?,
            client: Availability::Optional,
            server: Availability::Optional,
        })
    }
    fn search_result(&self, value: &Value) -> Result<SearchResult, AdapterError> {
        let map = &self.program.mappings.project;
        Ok(SearchResult {
            provider: self.program.provider.id.clone(),
            project: Self::text(value, map.id.as_deref())?,
            reference: Self::text(value, map.slug.as_deref().or(map.id.as_deref()))?,
            title: Self::text(value, map.title.as_deref())?,
            description: map
                .description
                .as_deref()
                .map(|path| Self::text(value, Some(path)))
                .transpose()?
                .unwrap_or_default(),
            icon_url: None,
            downloads: map
                .downloads
                .as_deref()
                .and_then(|path| pointer(value, path).ok())
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        })
    }
    fn release(&self, value: &Value, target: &Target) -> Result<Option<Release>, AdapterError> {
        let map = &self.program.mappings.release;
        let game_versions = Self::text_array(value, map.game_versions.as_deref())?;
        let loaders = Self::text_array(value, map.loaders.as_deref())?;
        if !game_versions
            .iter()
            .any(|game| game == &target.game_version)
            || !target
                .loader_ids()
                .any(|loader| loaders.iter().any(|mapped| mapped == loader))
        {
            return Ok(None);
        }
        let files: Vec<ReleaseFile> = pointer(
            value,
            map.files
                .as_deref()
                .ok_or_else(|| AdapterError::specific(RuntimeError::MissingMapping))?,
        )?
        .as_array()
        .ok_or_else(|| AdapterError::specific(RuntimeError::ExpectedArray))?
        .iter()
        .map(|value| file(value, &map.file))
        .collect::<Result<_, _>>()?;
        if files.is_empty() {
            return Err(AdapterError::specific(RuntimeError::NoFiles));
        }
        let dependencies = pointer(
            value,
            map.dependencies
                .as_deref()
                .ok_or_else(|| AdapterError::specific(RuntimeError::MissingMapping))?,
        )?
        .as_array()
        .ok_or_else(|| AdapterError::specific(RuntimeError::ExpectedArray))?
        .iter()
        .map(|value| self.dependency(value, &map.dependency))
        .collect::<Result<_, _>>()?;
        Ok(Some(Release {
            id: Self::text(value, map.id.as_deref())?,
            project: PackageId {
                provider: self.program.provider.id.clone(),
                project: Self::text(value, self.program.mappings.project.id.as_deref())?,
            },
            number: Self::text(value, map.number.as_deref())?,
            channel: Channel::Unknown,
            published: Self::text(value, map.published.as_deref())?,
            files,
            dependencies,
        }))
    }

    fn text_array(value: &Value, mapping: Option<&str>) -> Result<Vec<String>, AdapterError> {
        pointer(
            value,
            mapping.ok_or_else(|| AdapterError::specific(RuntimeError::MissingMapping))?,
        )?
        .as_array()
        .ok_or_else(|| AdapterError::specific(RuntimeError::ExpectedArray))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| AdapterError::specific(RuntimeError::ExpectedText))
        })
        .collect()
    }

    fn dependency(
        &self,
        value: &Value,
        mapping: &DependencyMapping,
    ) -> Result<Dependency, AdapterError> {
        let project = Self::text(value, mapping.project.as_deref())?;
        let kind = match Self::text(value, mapping.kind.as_deref())?.as_str() {
            "required" => DependencyKind::Required,
            "optional" => DependencyKind::Optional,
            "incompatible" => DependencyKind::Incompatible,
            "embedded" => DependencyKind::Embedded,
            _ => DependencyKind::Unknown,
        };
        Ok(Dependency {
            project: Some(PackageId {
                provider: self.program.provider.id.clone(),
                project,
            }),
            release: mapping
                .release
                .as_deref()
                .map(|path| Self::text(value, Some(path)))
                .transpose()?,
            kind,
        })
    }
}

fn pointer<'a>(value: &'a Value, path: &str) -> Result<&'a Value, AdapterError> {
    value
        .pointer(path)
        .ok_or_else(|| AdapterError::specific(RuntimeError::MissingPointer(path.to_owned())))
}
fn safe_component(value: &str) -> Result<(), AdapterError> {
    if !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        Ok(())
    } else {
        Err(AdapterError::specific(RuntimeError::UnsafeReference))
    }
}
fn interpolate(route: &str, name: &str, value: &str) -> Result<String, AdapterError> {
    let marker = format!("{{{name}}}");
    if route.matches(&marker).count() == 1 {
        Ok(route.replace(&marker, value))
    } else {
        Err(AdapterError::specific(RuntimeError::InvalidRouteTemplate))
    }
}
fn file(value: &Value, mapping: &FileMapping) -> Result<ReleaseFile, AdapterError> {
    let text = |path: Option<&str>| {
        pointer(
            value,
            path.ok_or_else(|| AdapterError::specific(RuntimeError::MissingMapping))?,
        )
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| AdapterError::specific(RuntimeError::ExpectedText))
        })
    };
    let optional = |path: Option<&str>| path.map(|path| pointer(value, path)).transpose();
    Ok(ReleaseFile {
        url: text(mapping.url.as_deref())?,
        name: text(mapping.name.as_deref())?,
        size: optional(mapping.size.as_deref())?.and_then(Value::as_u64),
        sha256: optional(mapping.sha256.as_deref())?
            .and_then(Value::as_str)
            .map(str::to_owned),
        sha512: optional(mapping.sha512.as_deref())?
            .and_then(Value::as_str)
            .map(str::to_owned),
        primary: optional(mapping.primary.as_deref())?
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}
fn direct_selection(provider: &str, raw: &str) -> Result<Selection, RuntimeError> {
    let (url, checksum) = raw
        .split_once('#')
        .map_or((raw, None), |(url, fragment)| (url, Some(fragment)));
    let path = url
        .strip_prefix("https://")
        .ok_or(RuntimeError::InsecureUrl)?
        .split('?')
        .next()
        .unwrap_or_default();
    let name = path
        .split_once('/')
        .and_then(|(host, path)| (!host.is_empty()).then_some(path))
        .and_then(|path| path.rsplit('/').next())
        .and_then(percent_decode)
        .filter(|name| safe_file_name(name))
        .ok_or(RuntimeError::InvalidUrl)?;
    let (sha256, sha512) =
        checksum
            .map(parse_checksum)
            .transpose()?
            .map_or((None, None), |(algorithm, digest)| {
                if algorithm == "sha256" {
                    (Some(digest), None)
                } else {
                    (None, Some(digest))
                }
            });
    let package = PackageId {
        provider: provider.to_owned(),
        project: url.to_owned(),
    };
    let file = ReleaseFile {
        url: url.to_owned(),
        name: name.clone(),
        size: None,
        sha256,
        sha512,
        primary: true,
    };
    Ok(Selection {
        project: Project {
            id: package.clone(),
            slug: None,
            title: name.clone(),
            client: Availability::Optional,
            server: Availability::Optional,
        },
        release: Release {
            id: url.to_owned(),
            project: package,
            number: name,
            channel: Channel::Unknown,
            published: String::new(),
            files: vec![file.clone()],
            dependencies: Vec::new(),
        },
        file,
        required_by: None,
    })
}
fn percent_decode(raw: &str) -> Option<String> {
    let mut decoded = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let escape = bytes
                .get(index + 1..index + 3)
                .filter(|escape| escape.iter().all(u8::is_ascii_hexdigit))?;
            decoded.push(u8::from_str_radix(std::str::from_utf8(escape).ok()?, 16).ok()?);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}
fn safe_file_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\', '\0'])
}
fn parse_checksum(fragment: &str) -> Result<(&str, String), RuntimeError> {
    let (algorithm, digest) = fragment
        .split_once('=')
        .ok_or(RuntimeError::InvalidChecksum)?;
    let length = if algorithm == "sha256" {
        64
    } else if algorithm == "sha512" {
        128
    } else {
        return Err(RuntimeError::InvalidChecksum);
    };
    if digest.len() == length
        && digest
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        Ok((algorithm, digest.to_ascii_lowercase()))
    } else {
        Err(RuntimeError::InvalidChecksum)
    }
}

#[derive(Debug)]
pub(super) enum RuntimeError {
    MissingMetadata,
    MissingRoute,
    MissingMapping,
    MissingPointer(String),
    ExpectedArray,
    ExpectedText,
    UnsafeReference,
    InvalidRouteTemplate,
    UnsupportedReleaseProject,
    InsecureUrl,
    InvalidUrl,
    InvalidChecksum,
    NoFiles,
    Json(String),
}
impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPointer(pointer) => write!(
                formatter,
                "provider program JSON pointer {pointer:?} was absent"
            ),
            Self::Json(error) => {
                write!(formatter, "provider program returned invalid JSON: {error}")
            }
            error => write!(formatter, "provider program runtime error: {error:?}"),
        }
    }
}
impl Error for RuntimeError {}

#[cfg(test)]
mod tests {
    use super::direct_selection;

    #[test]
    fn direct_urls_decode_safe_percent_escaped_file_names() {
        let selection = direct_selection("url", "https://example.test/mod%20file.jar").unwrap();
        assert_eq!(selection.file.name, "mod file.jar");
        for url in [
            "https://example.test",
            "https://example.test/mod%",
            "https://example.test/mod%2",
            "https://example.test/mod%2fother.jar",
            "https://example.test/%ff.jar",
        ] {
            assert!(direct_selection("url", url).is_err(), "{url}");
        }
    }
}
