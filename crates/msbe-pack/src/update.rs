//! Updating a profile's pack layer and re-applying its changes layer (§17.5).
//!
//! The changes layer is the difference between the profile and the base its pack layer recorded
//! at import. Updating replaces that base with the new pack version and re-applies each change. A
//! change the new version invalidates is a conflict the user resolves as `keep` or `drop`; nothing
//! is silently dropped or silently kept.

use std::{collections::BTreeMap, path::PathBuf};

use msbe_archive::Limits;
use msbe_core::{
    config::Home,
    instance::{Instance, ModEntry, Name, Profile, ProfileLayer},
};
use msbe_fsops::{Digest, RelPath};
use msbe_provider_api::{
    EnvironmentRequirement, PackImportPlan, PackOptions, PackOrigin, PackWarning,
};
use msbe_providers::Providers;
use serde::{Deserialize, Serialize};

use crate::{
    Connect, IssueCode, PackError, PackIssue, Progress,
    host::ZipPackInput,
    progress::checkpoint,
    stage::{self, Fetcher, ImportItem, StagedLayer},
};

/// What a client asks to update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRequest {
    /// The instance.
    pub instance: Name,
    /// The profile whose pack layer is replaced.
    pub profile: Name,
    /// The new pack version.
    pub input: PathBuf,
    /// The codec ID, or none to detect it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Resolutions for conflicts, keyed by conflict ID.
    #[serde(default)]
    pub resolutions: BTreeMap<String, Resolution>,
}

/// How a conflicting change is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Resolution {
    /// Apply the change over the new pack version.
    Keep,
    /// Discard the change and take the new pack version.
    Drop,
}

/// One operation the changes layer records against the pack base.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "subject", rename_all = "kebab-case")]
pub enum Change {
    /// A mod the pack does not ship was added.
    AddMod(Name),
    /// A mod the pack ships was removed.
    RemoveMod(Name),
    /// A mod the pack ships was replaced or pinned to other content.
    ReplaceMod(Name),
    /// A pack-owned file was added or changed.
    SetConfig(RelPath),
    /// A pack-owned file the pack ships was removed.
    RemoveConfig(RelPath),
    /// The pack's mods were reordered.
    Reorder,
}

impl Change {
    /// The stable ID a resolution names.
    pub fn id(&self) -> String {
        match self {
            Self::AddMod(name) | Self::RemoveMod(name) | Self::ReplaceMod(name) => {
                format!("mod:{name}")
            }
            Self::SetConfig(path) | Self::RemoveConfig(path) => format!("config:{path}"),
            Self::Reorder => "order".to_owned(),
        }
    }
}

/// A change the new pack version invalidates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerConflict {
    /// The conflict ID a resolution names.
    pub id: String,
    /// The change that no longer applies cleanly.
    pub change: Change,
    /// Why.
    pub reason: String,
    /// The resolution supplied, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
}

/// A complete update, shown before anything is acquired or written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePreview {
    /// The request this preview answers.
    pub request: UpdateRequest,
    /// The digest of the input as previewed.
    pub input_digest: Digest,
    /// The codec that planned the input.
    pub codec: String,
    /// The pack version being replaced.
    pub previous_version: Option<String>,
    /// The new pack layer's identity.
    pub origin: PackOrigin,
    /// Everything the new version needs, and how it is obtained.
    pub items: Vec<ImportItem>,
    /// Every change the changes layer records.
    pub changes: Vec<Change>,
    /// Changes the new version invalidates.
    pub conflicts: Vec<LayerConflict>,
    /// Installation-owned inputs the pack requires.
    pub environment: Vec<EnvironmentRequirement>,
    /// Reasons the update cannot run, including unresolved conflicts.
    pub blockers: Vec<PackIssue>,
    /// Concerns to review.
    pub warnings: Vec<PackWarning>,
    /// The staged new pack layer.
    pub layer: StagedLayer,
    /// The codec's plan.
    pub plan: PackImportPlan,
}

/// What an executed update changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateReport {
    /// The instance.
    pub instance: Name,
    /// The profile.
    pub profile: Name,
    /// The new pack version.
    pub version: Option<String>,
    /// Changes re-applied.
    pub applied: Vec<String>,
    /// Changes dropped by resolution.
    pub dropped: Vec<String>,
    /// The modules the profile now has.
    pub mods: Vec<Name>,
    /// Files the profile deploys.
    pub files: usize,
}

/// Plans replacing `request.profile`'s pack layer with `request.input`.
///
/// # Errors
///
/// Returns [`IssueCode::LayerConflict`] for a profile without a pack layer, or any error opening
/// or planning the input. Conflicts, missing content and mismatches are reported in the preview.
pub fn preview_update(
    providers: &Providers,
    home: &Home,
    request: UpdateRequest,
) -> Result<UpdatePreview, PackError> {
    let instance = Instance::open(home, &request.instance)?;
    let profile = instance.profile(&request.profile)?;
    let base = profile.pack_layer().cloned().ok_or_else(|| {
        PackError::issue(
            IssueCode::LayerConflict,
            format!(
                "profile {} has no pack layer; import a pack first",
                request.profile
            ),
        )
    })?;
    let target = profile
        .target
        .clone()
        .unwrap_or_else(|| stage::default_target(&instance));
    let opened = stage::open(
        providers,
        &instance,
        &target,
        &request.input,
        request.codec.as_deref(),
        &PackOptions::new(),
    )?;
    let mut staged = stage::stage(&instance, &opened.plan, &target, &base.mods)?;
    // The new base replaces only the pack layer; the profile's own lineage and user changes stay.
    staged.layer.layers.clear();
    staged.layer.deployment = None;
    if base
        .codec
        .as_deref()
        .is_some_and(|codec| codec != opened.plan.codec)
    {
        staged.blockers.push(PackIssue::new(
            IssueCode::LayerConflict,
            format!(
                "the pack layer was imported with {}, not {}",
                base.codec.as_deref().unwrap_or_default(),
                opened.plan.codec
            ),
        ));
    }
    if let (Some(previous), Some(next)) = (&base.pack, &opened.plan.origin.pack)
        && previous != next
    {
        staged.blockers.push(PackIssue::new(
            IssueCode::LayerConflict,
            format!("the profile tracks pack {previous}, but the input is pack {next}"),
        ));
    }
    let changes = changes(&base, &profile);
    let conflicts = conflicts(
        &changes,
        &base,
        &profile,
        &staged.layer,
        &request.resolutions,
    );
    for id in request.resolutions.keys() {
        if !conflicts.iter().any(|conflict| conflict.id == *id) {
            staged.blockers.push(PackIssue::new(
                IssueCode::InvalidOptions,
                format!("there is no conflict {id} to resolve"),
            ));
        }
    }
    for conflict in conflicts
        .iter()
        .filter(|conflict| conflict.resolution.is_none())
    {
        staged.blockers.push(PackIssue::new(
            IssueCode::LayerConflict,
            format!(
                "{}: {}; resolve it as keep or drop",
                conflict.id, conflict.reason
            ),
        ));
    }
    Ok(UpdatePreview {
        input_digest: opened.input.digest(),
        codec: opened.plan.codec.clone(),
        previous_version: base.version.clone(),
        origin: opened.plan.origin.clone(),
        items: staged.items,
        changes,
        conflicts,
        environment: opened.plan.environment.clone(),
        blockers: staged.blockers,
        warnings: staged.warnings,
        layer: staged.layer,
        plan: opened.plan,
        request,
    })
}

/// Runs exactly the update `preview` describes.
///
/// # Errors
///
/// Returns [`PackError::Blocked`] for a preview with blockers or unresolved conflicts,
/// [`IssueCode::StalePlan`] when the input changed, or any acquisition or resolution failure.
/// Nothing is written unless the updated profile resolves.
pub fn execute_update(
    providers: &Providers,
    home: &Home,
    preview: &UpdatePreview,
    connect: Connect<'_>,
    progress: &dyn Progress,
) -> Result<UpdateReport, PackError> {
    if !preview.blockers.is_empty() {
        return Err(PackError::Blocked(preview.blockers.clone()));
    }
    let instance = Instance::open(home, &preview.request.instance)?;
    let profile = instance.profile(&preview.request.profile)?;
    let input = ZipPackInput::open(&preview.request.input, &Limits::default())?;
    if input.digest() != preview.input_digest {
        return Err(PackError::issue(
            IssueCode::StalePlan,
            "the pack changed after it was previewed",
        ));
    }
    let mut fetcher = Fetcher::new(providers, &instance, &preview.layer.target, connect)?;
    let base = stage::obtain(
        &mut fetcher,
        &input,
        &preview.plan,
        &preview.layer,
        progress,
    )?;
    let applied = apply(
        &base,
        &preview.layer,
        &profile,
        &preview.changes,
        &preview.conflicts,
    );
    let updated = Profile {
        target: profile.target.clone(),
        order: applied.order,
        components: if preview.layer.components.is_empty() {
            profile.components.clone()
        } else {
            preview.layer.components.clone()
        },
        mods: applied.mods,
        configs: applied.configs,
        layers: stage::pack_layers(&preview.layer, &preview.plan, &base),
    };
    checkpoint(progress)?;
    let lockfile = instance.write_profile(&preview.request.profile, &updated)?;
    Ok(UpdateReport {
        instance: preview.request.instance.clone(),
        profile: preview.request.profile.clone(),
        version: preview.origin.version.clone(),
        applied: applied.applied,
        dropped: applied.dropped,
        mods: updated.load_order(),
        files: lockfile.deployment.len(),
    })
}

/// Every operation that turns `base` into `profile`.
pub(crate) fn changes(base: &ProfileLayer, profile: &Profile) -> Vec<Change> {
    let mut changes = Vec::new();
    for (name, entry) in &profile.mods {
        match base.mods.get(name) {
            None => changes.push(Change::AddMod(name.clone())),
            Some(original) if original != entry => changes.push(Change::ReplaceMod(name.clone())),
            Some(_) => {}
        }
    }
    changes.extend(
        base.mods
            .keys()
            .filter(|name| !profile.mods.contains_key(*name))
            .cloned()
            .map(Change::RemoveMod),
    );
    for (path, digest) in &profile.configs {
        if base.configs.get(path) != Some(digest) {
            changes.push(Change::SetConfig(path.clone()));
        }
    }
    changes.extend(
        base.configs
            .keys()
            .filter(|path| !profile.configs.contains_key(*path))
            .cloned()
            .map(Change::RemoveConfig),
    );
    let effective = profile.load_order();
    let reordered: Vec<&Name> = effective
        .iter()
        .filter(|name| base.mods.contains_key(*name))
        .collect();
    let original = ordered(&base.order, &base.mods);
    let original: Vec<&Name> = original
        .iter()
        .filter(|name| profile.mods.contains_key(*name))
        .collect();
    if reordered != original {
        changes.push(Change::Reorder);
    }
    changes.sort();
    changes
}

/// The changes `next` invalidates, with any resolution supplied for each.
fn conflicts(
    changes: &[Change],
    base: &ProfileLayer,
    profile: &Profile,
    next: &StagedLayer,
    resolutions: &BTreeMap<String, Resolution>,
) -> Vec<LayerConflict> {
    let pack_changed = |name: &Name| match (next.mods.get(name), base.mods.get(name)) {
        (Some(next), Some(original)) => !stage::same(
            &stage::module_identity(next),
            &stage::entry_identity(original),
        ),
        _ => false,
    };
    changes
        .iter()
        .filter_map(|change| {
            let reason = match change {
                Change::AddMod(name) => next
                    .mods
                    .contains_key(name)
                    .then_some("the updated pack now ships a mod with this name"),
                Change::RemoveMod(name) if !next.mods.contains_key(name) => {
                    Some("the updated pack no longer ships this mod")
                }
                Change::ReplaceMod(name) if !next.mods.contains_key(name) => {
                    Some("the updated pack removed the mod this change replaces")
                }
                Change::RemoveMod(name) | Change::ReplaceMod(name) => {
                    pack_changed(name).then_some("the updated pack changed this mod")
                }
                Change::SetConfig(path) | Change::RemoveConfig(path) => (next.configs.get(path)
                    != base.configs.get(path))
                .then_some("the updated pack changed this file"),
                Change::Reorder => profile
                    .load_order()
                    .iter()
                    .any(|name| base.mods.contains_key(name) && !next.mods.contains_key(name))
                    .then_some("the updated pack removed mods this order names"),
            }?;
            Some(LayerConflict {
                id: change.id(),
                change: change.clone(),
                reason: reason.to_owned(),
                resolution: resolutions.get(&change.id()).copied(),
            })
        })
        .collect()
}

/// The profile content an update produces.
#[derive(Debug)]
struct Updated {
    mods: BTreeMap<Name, ModEntry>,
    configs: BTreeMap<RelPath, Digest>,
    order: Vec<Name>,
    applied: Vec<String>,
    dropped: Vec<String>,
}

/// Re-applies every kept change to the new base.
fn apply(
    base: &BTreeMap<Name, ModEntry>,
    next: &StagedLayer,
    profile: &Profile,
    changes: &[Change],
    conflicts: &[LayerConflict],
) -> Updated {
    let mut applied = Updated {
        mods: base.clone(),
        configs: next.configs.clone(),
        order: Vec::new(),
        applied: Vec::new(),
        dropped: Vec::new(),
    };
    let mut reorder = false;
    for change in changes {
        let id = change.id();
        let keep = conflicts
            .iter()
            .find(|conflict| conflict.id == id)
            .is_none_or(|conflict| conflict.resolution == Some(Resolution::Keep));
        if !keep {
            applied.dropped.push(id);
            continue;
        }
        applied.applied.push(id);
        match change {
            Change::AddMod(name) | Change::ReplaceMod(name) => {
                if let Some(entry) = profile.mods.get(name) {
                    applied.mods.insert(name.clone(), entry.clone());
                }
            }
            Change::RemoveMod(name) => {
                applied.mods.remove(name);
            }
            Change::SetConfig(path) => {
                if let Some(digest) = profile.configs.get(path) {
                    applied.configs.insert(path.clone(), *digest);
                }
            }
            Change::RemoveConfig(path) => {
                applied.configs.remove(path);
            }
            Change::Reorder => reorder = true,
        }
    }
    let (primary, secondary) = if reorder {
        (profile.load_order(), next.order.clone())
    } else {
        (next.order.clone(), profile.load_order())
    };
    for name in primary.into_iter().chain(secondary) {
        if applied.mods.contains_key(&name) && !applied.order.contains(&name) {
            applied.order.push(name);
        }
    }
    let rest: Vec<Name> = applied
        .mods
        .keys()
        .filter(|name| !applied.order.contains(*name))
        .cloned()
        .collect();
    applied.order.extend(rest);
    applied
}

/// `order`, then every other key of `mods` by name.
fn ordered<T>(order: &[Name], mods: &BTreeMap<Name, T>) -> Vec<Name> {
    let mut result: Vec<Name> = order
        .iter()
        .filter(|name| mods.contains_key(*name))
        .cloned()
        .collect();
    let rest: Vec<Name> = mods
        .keys()
        .filter(|name| !result.contains(*name))
        .cloned()
        .collect();
    result.extend(rest);
    result
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use msbe_core::instance::ProfileTarget;
    use msbe_core::instance::{ModEntry, Name, Profile, ProfileLayer, Provenance, StoredFile};
    use msbe_fsops::{Digest, RelPath};
    use msbe_plan_schema::Side;

    use super::{Change, Resolution, apply, changes, conflicts};
    use crate::stage::{StagedLayer, StagedModule};

    fn module(bytes: &[u8]) -> ModEntry {
        let blob = Digest::of_bytes(bytes);
        ModEntry {
            origin: "mod.jar".to_owned(),
            provider: Some(Provenance {
                provider: "example".to_owned(),
                project: "project".to_owned(),
                version: "v".to_owned(),
                version_number: "1".to_owned(),
                hashes: BTreeMap::from([(
                    "sha256".to_owned(),
                    blob.to_string().trim_start_matches("sha256:").to_owned(),
                )]),
            }),
            files: vec![StoredFile {
                source: RelPath::new("mod.jar").unwrap(),
                blob,
            }],
            answers: BTreeMap::new(),
        }
    }

    fn name(raw: &str) -> Name {
        Name::new(raw).unwrap()
    }

    fn layer(mods: &[(&str, ModEntry)]) -> ProfileLayer {
        ProfileLayer {
            id: "pack".to_owned(),
            kind: "pack".to_owned(),
            codec: None,
            pack: None,
            version: Some("1".to_owned()),
            digest: None,
            mods: mods
                .iter()
                .map(|(key, entry)| (name(key), entry.clone()))
                .collect(),
            configs: BTreeMap::new(),
            order: mods.iter().map(|(key, _)| name(key)).collect(),
        }
    }

    fn staged(mods: &[(&str, ModEntry)]) -> StagedLayer {
        StagedLayer {
            target: ProfileTarget {
                loader: "loader".to_owned(),
                loader_version: None,
                side: Side::Client,
            },
            order: mods.iter().map(|(key, _)| name(key)).collect(),
            mods: mods
                .iter()
                .map(|(key, entry)| {
                    (
                        name(key),
                        StagedModule::Exact {
                            entry: entry.clone(),
                        },
                    )
                })
                .collect(),
            configs: BTreeMap::new(),
            components: BTreeMap::new(),
            layers: Vec::new(),
            deployment: None,
        }
    }

    #[test]
    fn changes_are_reapplied_and_invalidated_ones_are_conflicts() {
        let (a1, b1, c1) = (module(b"a1"), module(b"b1"), module(b"c1"));
        let base = layer(&[("a", a1.clone()), ("b", b1.clone()), ("c", c1)]);
        let local = module(b"local");
        let profile = Profile {
            target: None,
            order: vec![name("a"), name("b"), name("local")],
            components: BTreeMap::new(),
            mods: BTreeMap::from([
                (name("a"), module(b"a-pinned")),
                (name("b"), b1.clone()),
                (name("local"), local.clone()),
            ]),
            configs: BTreeMap::new(),
            layers: vec![base.clone()],
        };
        let recorded = changes(&base, &profile);
        assert_eq!(
            recorded,
            [
                Change::AddMod(name("local")),
                Change::RemoveMod(name("c")),
                Change::ReplaceMod(name("a"))
            ]
        );

        // Version 2 updates a and drops c; b is unchanged.
        let next = staged(&[("a", module(b"a2")), ("b", b1)]);
        let found = conflicts(&recorded, &base, &profile, &next, &BTreeMap::new());
        let ids: Vec<&str> = found.iter().map(|conflict| conflict.id.as_str()).collect();
        assert_eq!(ids, ["mod:c", "mod:a"]);

        let resolved = conflicts(
            &recorded,
            &base,
            &profile,
            &next,
            &BTreeMap::from([
                ("mod:a".to_owned(), Resolution::Drop),
                ("mod:c".to_owned(), Resolution::Keep),
            ]),
        );
        let base_mods: BTreeMap<Name, ModEntry> = [("a", module(b"a2")), ("b", module(b"b1"))]
            .into_iter()
            .map(|(key, entry)| (name(key), entry))
            .collect();
        let applied = apply(&base_mods, &next, &profile, &recorded, &resolved);
        assert_eq!(
            applied.mods.get(&name("a")),
            Some(&module(b"a2")),
            "dropped pin takes v2"
        );
        assert_eq!(
            applied.mods.get(&name("local")),
            Some(&local),
            "the addition is kept"
        );
        assert_eq!(applied.order, [name("a"), name("b"), name("local")]);
        assert_eq!(applied.dropped, ["mod:a"]);
    }
}
