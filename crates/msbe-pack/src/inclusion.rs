//! Neutral blob-inclusion planning (§17.10).
//!
//! Every required digest is judged on two independent axes: whether its exact bytes can be
//! obtained again, and whether this export's purpose may embed them. Distribution is decided here
//! from the lockfile's source facts and dated observations; it is never read from the lockfile.

use std::collections::{BTreeMap, BTreeSet};

use msbe_core::instance::{BlobSource, DistributionDecision, PackFileRole};
use msbe_fsops::{Digest, RelPath};
use msbe_provider_api::{
    Availability, EnvironmentRequirement, Observations, PackFile, PackInclusion, PackRequirement,
    PackWarning, PackageId, RequirementSource,
};
use serde::{Deserialize, Serialize};

use crate::{
    IssueCode, PackIssue,
    options::{BlobMode, CommonOptions, OnForbidden, Purpose, Reproducibility},
};

/// Whether a digest's exact bytes can be obtained again without embedding them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reacquisition {
    /// An exact provider release or pinned direct download.
    Exact,
    /// A deterministic transform over reproducible inputs.
    Derivable,
    /// The recipient's own installation supplies it.
    Environment,
    /// Nothing but the bytes themselves reproduce it.
    Unsourceable,
}

/// How a preview groups a file for review (§17.13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewGroup {
    /// Referenced through an exact provider or direct source.
    ProviderReference,
    /// Obtainable only by user action.
    UserAction,
    /// Supplied by the recipient's installation.
    EnvironmentInput,
    /// A pack-owned file embedded in the export.
    EmbeddedConfig,
    /// Local content embedded in the export.
    EmbeddedLocal,
    /// Other content embedded in the export.
    EmbeddedOther,
    /// Regenerated from reproducible inputs.
    Derived,
    /// Blocked by policy or reproducibility.
    PolicyBlocker,
    /// Omitted with a warning under best-effort reproducibility.
    Omitted,
}

/// One file's place in an export preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InclusionItem {
    /// Deployment path, or artifact source path when not deployed.
    pub path: RelPath,
    /// Exact content digest.
    pub digest: Digest,
    /// Semantic role.
    pub role: PackFileRole,
    /// The profile layer that introduced the file.
    pub layer: String,
    /// Whether `path` is a deployment destination.
    pub deployed: bool,
    /// How the preview groups it.
    pub group: PreviewGroup,
    /// Whether its bytes can be obtained again.
    pub reacquisition: Reacquisition,
    /// The distribution decision this plan applied.
    pub distribution: DistributionDecision,
}

/// An observation a plan relied on, and when it was taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationUse {
    /// The observed subject.
    pub subject: String,
    /// When it was observed.
    pub observed_at: String,
}

/// The neutral decision for every required digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InclusionPlan {
    /// What a codec may embed and must reference.
    pub inclusion: PackInclusion,
    /// Every file with its group, in path order.
    pub items: Vec<InclusionItem>,
    /// Reasons the export cannot run.
    pub blockers: Vec<PackIssue>,
    /// Non-fatal concerns the user should review.
    pub warnings: Vec<PackWarning>,
    /// Observations the decisions relied on.
    pub observations: Vec<ObservationUse>,
}

/// What one digest becomes in an export.
#[derive(Debug, Clone)]
enum Action {
    Embed(Option<PackWarning>),
    Reference(PackRequirement),
    Derive,
    Environment(EnvironmentRequirement),
    Omit(PackWarning),
    Blocked(PackIssue),
}

impl Action {
    const fn rank(&self) -> u8 {
        match self {
            Self::Embed(_) => 0,
            Self::Reference(_) => 1,
            Self::Derive => 2,
            Self::Environment(_) => 3,
            Self::Omit(_) => 4,
            Self::Blocked(_) => 5,
        }
    }
}

/// One candidate file with the axes and action decided for it.
type Decision<'a> = (&'a PackFile, Reacquisition, DistributionDecision, Action);

/// Decides how every file in `files` is carried by an export under `options`.
pub fn plan(
    files: &[PackFile],
    observations: &Observations,
    options: CommonOptions,
) -> InclusionPlan {
    let mut by_digest: BTreeMap<Digest, Vec<&PackFile>> = BTreeMap::new();
    for file in files {
        by_digest.entry(file.digest).or_default().push(file);
    }
    let reproducible: BTreeSet<Digest> = files
        .iter()
        .filter(|file| {
            matches!(
                base_reacquisition(&file.source),
                Reacquisition::Exact | Reacquisition::Environment
            )
        })
        .map(|file| file.digest)
        .collect();

    let mut plan = InclusionPlan {
        inclusion: PackInclusion::default(),
        items: Vec::new(),
        blockers: Vec::new(),
        warnings: Vec::new(),
        observations: Vec::new(),
    };
    let mut observed = BTreeSet::new();
    for (digest, candidates) in by_digest {
        let observation = observations.entries.get(&digest.to_string());
        if let Some(observation) = observation
            && observed.insert(digest)
        {
            plan.observations.push(ObservationUse {
                subject: digest.to_string(),
                observed_at: observation.observed_at.clone(),
            });
        }
        let decided: Vec<Decision<'_>> = candidates
            .iter()
            .map(|file| {
                let reacquisition = reacquisition(&file.source, &reproducible);
                let distribution = distribution(
                    file,
                    observation.and_then(|observation| observation.distribution.as_ref()),
                );
                let action = decide(file, reacquisition, &distribution, options);
                (*file, reacquisition, distribution, action)
            })
            .collect();
        let Some((_, reacquisition, distribution, action)) =
            decided.iter().min_by_key(|(.., action)| action.rank())
        else {
            continue;
        };
        if permitted(distribution, options.purpose) {
            plan.inclusion.permitted.insert(digest);
        }
        match action {
            Action::Embed(warning) => {
                plan.inclusion.embed.insert(digest);
                plan.warnings.extend(warning.clone());
            }
            Action::Reference(requirement) => plan.inclusion.requirements.push(requirement.clone()),
            Action::Derive => {}
            Action::Environment(requirement) => {
                plan.inclusion.environment.push(requirement.clone());
            }
            Action::Omit(warning) => plan.warnings.push(warning.clone()),
            Action::Blocked(issue) => plan.blockers.push(issue.clone()),
        }
        for (file, ..) in &decided {
            plan.items.push(InclusionItem {
                path: file.path.clone(),
                digest,
                role: file.role,
                layer: file.layer.clone(),
                deployed: file.deployed,
                group: group(action, file.role),
                reacquisition: *reacquisition,
                distribution: distribution.clone(),
            });
        }
    }
    plan.items.sort_by(|left, right| left.path.cmp(&right.path));
    plan
}

/// The preview group for a file whose digest was decided as `action`.
pub(crate) const fn embedded_group(role: PackFileRole) -> PreviewGroup {
    match role {
        PackFileRole::PackOwnedConfig => PreviewGroup::EmbeddedConfig,
        PackFileRole::LocalArtifact => PreviewGroup::EmbeddedLocal,
        _ => PreviewGroup::EmbeddedOther,
    }
}

const fn group(action: &Action, role: PackFileRole) -> PreviewGroup {
    match action {
        Action::Embed(_) => embedded_group(role),
        Action::Reference(_) => PreviewGroup::ProviderReference,
        Action::Derive => PreviewGroup::Derived,
        Action::Environment(_) => PreviewGroup::EnvironmentInput,
        Action::Omit(_) => PreviewGroup::Omitted,
        Action::Blocked(_) => PreviewGroup::PolicyBlocker,
    }
}

fn base_reacquisition(source: &BlobSource) -> Reacquisition {
    match source {
        BlobSource::Provider { exact: true, .. } => Reacquisition::Exact,
        BlobSource::Direct { urls, hashes } if !urls.is_empty() && !hashes.is_empty() => {
            Reacquisition::Exact
        }
        BlobSource::Environment { .. } => Reacquisition::Environment,
        _ => Reacquisition::Unsourceable,
    }
}

/// A derived blob is reproducible only when its transform is deterministic and every input is
/// exactly reproducible itself.
fn reacquisition(source: &BlobSource, reproducible: &BTreeSet<Digest>) -> Reacquisition {
    match source {
        BlobSource::Derived { inputs, transform }
            if transform.deterministic
                && inputs.iter().all(|input| reproducible.contains(input)) =>
        {
            Reacquisition::Derivable
        }
        other => base_reacquisition(other),
    }
}

/// Pack-owned files are authored by the pack creator; everything else needs affirmative facts,
/// which only an observation supplies today.
fn distribution(file: &PackFile, observed: Option<&DistributionDecision>) -> DistributionDecision {
    match (observed, &file.source) {
        (Some(decision), _) => decision.clone(),
        (None, BlobSource::PackOwned) => DistributionDecision::Allowed,
        (None, _) => DistributionDecision::Unknown,
    }
}

const fn permitted(distribution: &DistributionDecision, purpose: Purpose) -> bool {
    match distribution {
        DistributionDecision::Allowed => true,
        DistributionDecision::Unknown => matches!(purpose, Purpose::PrivateTransfer),
        DistributionDecision::Forbidden(_) => false,
    }
}

fn decide(
    file: &PackFile,
    reacquisition: Reacquisition,
    distribution: &DistributionDecision,
    options: CommonOptions,
) -> Action {
    let complete = options.blob_mode == BlobMode::Complete;
    let private = options.purpose == Purpose::PrivateTransfer;
    match reacquisition {
        Reacquisition::Environment => Action::Environment(EnvironmentRequirement {
            root: match &file.source {
                BlobSource::Environment { root, .. } => root.clone(),
                _ => String::new(),
            },
            path: match &file.source {
                BlobSource::Environment { path, .. } => path.clone(),
                _ => file.path.clone(),
            },
            digest: file.digest,
        }),
        Reacquisition::Exact | Reacquisition::Derivable => match distribution {
            DistributionDecision::Allowed if complete => Action::Embed(None),
            DistributionDecision::Unknown if complete && private => {
                Action::Embed(Some(unknown_rights_warning(file)))
            }
            _ if reacquisition == Reacquisition::Derivable => Action::Derive,
            _ => Action::Reference(requirement(file)),
        },
        Reacquisition::Unsourceable => match distribution {
            DistributionDecision::Forbidden(reason) => match options.on_forbidden {
                OnForbidden::Error => Action::Blocked(issue(
                    file,
                    IssueCode::DistributionForbidden,
                    format!(
                        "{} cannot be redistributed ({reason}) and has no exact source",
                        file.path
                    ),
                )),
                OnForbidden::ExternalRequirement => unsourceable(
                    file,
                    options,
                    IssueCode::UserActionRequired,
                    format!(
                        "{} cannot be redistributed ({reason}); the recipient must supply it",
                        file.path
                    ),
                ),
            },
            DistributionDecision::Unknown if !private => Action::Blocked(issue(
                file,
                IssueCode::DistributionUnknown,
                format!(
                    "{} has no exact source and nothing establishes permission to distribute it",
                    file.path
                ),
            )),
            DistributionDecision::Allowed | DistributionDecision::Unknown
                if options.blob_mode == BlobMode::Thin =>
            {
                unsourceable(
                    file,
                    options,
                    IssueCode::UnreproducibleContent,
                    format!(
                        "{} has no exact source, and a thin export embeds nothing",
                        file.path
                    ),
                )
            }
            DistributionDecision::Unknown => Action::Embed(Some(unknown_rights_warning(file))),
            DistributionDecision::Allowed => Action::Embed(None),
        },
    }
}

fn unsourceable(
    file: &PackFile,
    options: CommonOptions,
    code: IssueCode,
    message: String,
) -> Action {
    match options.reproducibility {
        Reproducibility::BestEffort => Action::Omit(PackWarning {
            code: format!("{code:?}"),
            message,
        }),
        Reproducibility::Strict | Reproducibility::AllowUserAction => {
            Action::Blocked(issue(file, code, message))
        }
    }
}

fn unknown_rights_warning(file: &PackFile) -> PackWarning {
    PackWarning {
        code: format!("{:?}", IssueCode::DistributionUnknown),
        message: format!(
            "{} is embedded for a private transfer without established redistribution rights",
            file.path
        ),
    }
}

fn issue(file: &PackFile, code: IssueCode, message: String) -> PackIssue {
    PackIssue {
        code,
        path: Some(file.path.clone()),
        digest: Some(file.digest),
        message,
    }
}

/// The exact requirement that reproduces `file` from its recorded source.
pub(crate) fn requirement(file: &PackFile) -> PackRequirement {
    let (hashes, sources) = match &file.source {
        BlobSource::Provider { provenance, .. } => (
            provenance.hashes.clone(),
            vec![RequirementSource::Provider {
                package: PackageId {
                    provider: provenance.provider.clone(),
                    project: provenance.project.clone(),
                },
                version: Some(provenance.version.clone()),
            }],
        ),
        BlobSource::Direct { urls, hashes } => (
            hashes.clone(),
            vec![RequirementSource::Direct { urls: urls.clone() }],
        ),
        _ => (BTreeMap::new(), Vec::new()),
    };
    PackRequirement {
        digest: Some(file.digest),
        hashes,
        destination: file.deployed.then(|| file.path.clone()),
        side: Availability::Required,
        answers: BTreeMap::new(),
        sources,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use msbe_core::instance::{BlobSource, DistributionDecision, PackFileRole, Provenance};
    use msbe_fsops::{Digest, RelPath};
    use msbe_provider_api::{Observation, Observations, PackFile};

    use super::{PreviewGroup, plan};
    use crate::{
        IssueCode,
        host::Compression,
        options::{BlobMode, CommonOptions, OnForbidden, Purpose, Reproducibility},
    };

    fn options(blob_mode: BlobMode, purpose: Purpose) -> CommonOptions {
        CommonOptions {
            purpose,
            reproducibility: Reproducibility::Strict,
            blob_mode,
            on_forbidden: OnForbidden::Error,
            compression: Compression::Deflate,
        }
    }

    fn file(path: &str, role: PackFileRole, source: BlobSource) -> PackFile {
        PackFile {
            path: RelPath::new(path).unwrap(),
            digest: Digest::of_bytes(path.as_bytes()),
            role,
            layer: "changes".to_owned(),
            deployed: true,
            source,
            distribution: DistributionDecision::Unknown,
        }
    }

    fn profile() -> Vec<PackFile> {
        let provenance = Provenance {
            provider: "example".to_owned(),
            project: "project".to_owned(),
            version: "release".to_owned(),
            version_number: "1.0".to_owned(),
            hashes: BTreeMap::from([("sha512".to_owned(), "ab".to_owned())]),
        };
        vec![
            file(
                "mods/provider.jar",
                PackFileRole::ProviderArtifact,
                BlobSource::Provider {
                    provenance,
                    exact: true,
                },
            ),
            file(
                "config/pack.toml",
                PackFileRole::PackOwnedConfig,
                BlobSource::PackOwned,
            ),
            file(
                "mods/local.jar",
                PackFileRole::LocalArtifact,
                BlobSource::Local,
            ),
        ]
    }

    fn groups(files: &[PackFile], options: CommonOptions) -> Vec<(String, PreviewGroup)> {
        plan(files, &Observations::default(), options)
            .items
            .into_iter()
            .map(|item| (item.path.as_str().to_owned(), item.group))
            .collect()
    }

    #[test]
    fn blob_modes_produce_the_documented_inclusion_sets() {
        let files = profile();
        assert_eq!(
            groups(
                &files,
                options(BlobMode::Portable, Purpose::PrivateTransfer)
            ),
            [
                ("config/pack.toml".to_owned(), PreviewGroup::EmbeddedConfig),
                ("mods/local.jar".to_owned(), PreviewGroup::EmbeddedLocal),
                (
                    "mods/provider.jar".to_owned(),
                    PreviewGroup::ProviderReference
                ),
            ]
        );
        assert_eq!(
            groups(
                &files,
                options(BlobMode::Complete, Purpose::PrivateTransfer)
            ),
            [
                ("config/pack.toml".to_owned(), PreviewGroup::EmbeddedConfig),
                ("mods/local.jar".to_owned(), PreviewGroup::EmbeddedLocal),
                ("mods/provider.jar".to_owned(), PreviewGroup::EmbeddedOther),
            ]
        );
        let thin = plan(
            &files,
            &Observations::default(),
            options(BlobMode::Thin, Purpose::PrivateTransfer),
        );
        assert!(thin.inclusion.embed.is_empty());
        assert_eq!(
            thin.blockers
                .iter()
                .map(|issue| issue.code)
                .collect::<Vec<_>>(),
            [
                IssueCode::UnreproducibleContent,
                IssueCode::UnreproducibleContent
            ],
            "strict thin exports fail on content with no exact source"
        );
    }

    #[test]
    fn public_distribution_never_embeds_unknown_or_forbidden_content() {
        let mut files = profile();
        let public = plan(
            &files,
            &Observations::default(),
            options(BlobMode::Complete, Purpose::Distribute),
        );
        assert_eq!(
            public.inclusion.embed.len(),
            1,
            "only the pack-owned file is embedded"
        );
        assert!(
            public
                .inclusion
                .permitted
                .iter()
                .all(|digest| public.inclusion.embed.contains(digest))
        );
        let [blocker] = public.blockers.as_slice() else {
            panic!("expected one blocker, got {:?}", public.blockers);
        };
        assert_eq!(blocker.code, IssueCode::DistributionUnknown);

        let provider = files.remove(0);
        let observations = Observations {
            entries: BTreeMap::from([(
                provider.digest.to_string(),
                Observation {
                    observed_at: "2026-09-12".to_owned(),
                    currently_acquirable: true,
                    distribution: Some(DistributionDecision::Forbidden("provider flag".to_owned())),
                },
            )]),
        };
        let forbidden = plan(
            std::slice::from_ref(&provider),
            &observations,
            options(BlobMode::Complete, Purpose::PrivateTransfer),
        );
        assert!(forbidden.inclusion.embed.is_empty() && forbidden.inclusion.permitted.is_empty());
        assert_eq!(
            forbidden.inclusion.requirements.len(),
            1,
            "an exact source is referenced instead"
        );
        assert_eq!(
            forbidden
                .observations
                .first()
                .map(|used| used.observed_at.as_str()),
            Some("2026-09-12")
        );
    }
}
