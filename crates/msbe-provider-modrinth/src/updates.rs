//! Update discovery: Modrinth's bulk hash lookups, keeping each file on its release channel.

use std::collections::{BTreeMap, BTreeSet};

use msbe_provider_api::{AdapterError, Provenance, Target, Update, UpdateCheck};

use crate::{
    client::Client,
    wire::{HashQuery, Version, VersionType},
};

/// Checks installed files, identified by the SHA-512 Modrinth published for them, for versions
/// to replace them with, answering in the order `installed` is given.
///
/// A file stays on its release channel or moves to a more stable one: a release is only replaced
/// by a release, and a beta by a beta or a release. A replacement is always newer than the
/// installed version, unless the installed version does not support `target`; then the newest
/// compatible version on its channel is offered, even if it is older.
///
/// Costs one request for the installed versions and one per channel in use, however many files
/// are checked.
pub(crate) fn check(
    client: &Client<'_>,
    installed: &[&Provenance],
    target: &Target,
) -> Result<Vec<UpdateCheck>, AdapterError> {
    let requested: Vec<String> = installed
        .iter()
        .map(|provenance| provenance.sha512.to_ascii_lowercase())
        .collect();
    if requested.is_empty() {
        return Ok(Vec::new());
    }
    let unique: BTreeSet<&str> = requested.iter().map(String::as_str).collect();
    let all: Vec<&str> = unique.into_iter().collect();
    let loader_ids: Vec<String> = target.loader_ids().map(str::to_owned).collect();
    let listed = lowercase_keys(client.versions_by_hash(&HashQuery {
        hashes: &all,
        algorithm: "sha512",
        loaders: None,
        game_versions: None,
        version_types: None,
    })?);

    let mut channels: BTreeMap<&'static [&'static str], Vec<&str>> = BTreeMap::new();
    for (hash, version) in &listed {
        channels
            .entry(channel(version.kind))
            .or_default()
            .push(hash);
    }
    let mut latest = BTreeMap::new();
    for (version_types, group) in channels {
        latest.extend(lowercase_keys(client.latest_by_hash(&HashQuery {
            hashes: &group,
            algorithm: "sha512",
            loaders: Some(&loader_ids),
            game_versions: Some([target.game_version.as_str()]),
            version_types: Some(version_types),
        })?));
    }

    requested
        .iter()
        .map(|hash| match listed.get(hash) {
            Some(version) => decide(version, latest.get(hash), target),
            None => Ok(UpdateCheck::Unlisted),
        })
        .collect()
}

/// The version types a file installed from `installed` may move to: its own, or more stable.
const fn channel(installed: VersionType) -> &'static [&'static str] {
    match installed {
        VersionType::Release => &["release"],
        VersionType::Beta => &["release", "beta"],
        VersionType::Alpha | VersionType::Unknown => &["release", "beta", "alpha"],
    }
}

/// Whether `latest`, the newest version on the installed version's channel, should replace it.
fn decide(
    installed: &Version,
    latest: Option<&Version>,
    target: &Target,
) -> Result<UpdateCheck, AdapterError> {
    let fits = installed.supports(target);
    let replacement = latest.filter(|candidate| {
        candidate.id != installed.id
            && candidate.supports(target)
            && (!fits || candidate.date_published > installed.date_published)
    });
    Ok(match replacement {
        Some(version) => {
            let release = version.clone().into_model();
            let file = release
                .primary_file()
                .cloned()
                .ok_or_else(|| AdapterError::NoFiles {
                    project: release.project.clone(),
                    release: release.id.clone(),
                })?;
            UpdateCheck::Available(Box::new(Update { release, file }))
        }
        None if fits => UpdateCheck::Current,
        None => UpdateCheck::Incompatible,
    })
}

fn lowercase_keys<V>(map: BTreeMap<String, V>) -> BTreeMap<String, V> {
    map.into_iter()
        .map(|(key, value)| (key.to_ascii_lowercase(), value))
        .collect()
}

#[cfg(test)]
mod tests {
    use msbe_provider_api::{Provenance, UpdateCheck, Updates};
    use serde_json::json;

    use crate::{
        ID, Modrinth,
        test_support::{FakeHttp, target, version},
    };

    #[test]
    fn updates_stay_on_their_channel_and_only_go_back_to_regain_compatibility() {
        let hash = |c: char| c.to_string().repeat(128);
        let dated = |id: &str, kind: &str, date: &str, game_version: &str| {
            let mut entry = version(id, "P", id, kind, date, &format!("{id}.jar"));
            entry
                .as_object_mut()
                .unwrap()
                .insert("game_versions".to_owned(), json!([game_version]));
            entry
        };
        let mut http = FakeHttp::default();
        http.route_post(
            "/version_files",
            "",
            json!({
                hash('a'): dated("R1", "release", "2026-07-01T00:00:00Z", "1.21.1"),
                hash('b'): dated("B1", "beta", "2026-08-10T00:00:00Z", "1.21.1"),
                hash('c'): dated("O1", "release", "2026-09-01T00:00:00Z", "1.20.1"),
                hash('d'): dated("G1", "release", "2026-09-01T00:00:00Z", "1.20.1"),
            }),
        );
        http.route_post(
            "/version_files/update",
            "release",
            json!({
                // Newer on the release channel: an update.
                hash('a'): dated("R2", "release", "2026-08-01T00:00:00Z", "1.21.1"),
                // Older, but the installed version does not support 1.21.1: still offered.
                hash('c'): dated("O2", "release", "2026-01-01T00:00:00Z", "1.21.1"),
            }),
        );
        http.route_post(
            "/version_files/update",
            "release,beta",
            // Older than the installed beta, so not a downgrade target.
            json!({ hash('b'): dated("R3", "release", "2026-08-01T00:00:00Z", "1.21.1") }),
        );

        // The first hash is uppercase: lookups are case-insensitive.
        let installed: Vec<Provenance> = ['A', 'b', 'c', 'd', 'e']
            .into_iter()
            .map(|key| Provenance {
                provider: ID.to_owned(),
                project: "P".to_owned(),
                version: String::new(),
                version_number: String::new(),
                sha512: hash(key),
            })
            .collect();
        let installed: Vec<&Provenance> = installed.iter().collect();
        let checks = Modrinth::new().check(&http, &installed, &target()).unwrap();
        let available = |index: usize| match checks.get(index) {
            Some(UpdateCheck::Available(update)) => Some(update.release.id.as_str()),
            _ => None,
        };
        assert_eq!(available(0), Some("R2"));
        assert_eq!(checks.get(1), Some(&UpdateCheck::Current));
        assert_eq!(available(2), Some("O2"));
        assert_eq!(checks.get(3), Some(&UpdateCheck::Incompatible));
        assert_eq!(checks.get(4), Some(&UpdateCheck::Unlisted));

        let requests = http.requests.borrow();
        assert_eq!(requests.len(), 3, "{requests:?}");
        assert!(
            requests.iter().any(|request| {
                request.contains(r#""version_types":["release","beta"]"#)
                    && request.contains(r#""loaders":["fabric"]"#)
                    && request.contains(r#""game_versions":["1.21.1"]"#)
                    && request.contains(r#""algorithm":"sha512""#)
            }),
            "{requests:?}"
        );
    }
}
