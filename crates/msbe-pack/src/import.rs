//! Previewed import into a new or empty profile (§17.12).

use std::{collections::BTreeMap, path::PathBuf};

use msbe_archive::Limits;
use msbe_core::{
    config::Home,
    instance::{Instance, InstanceError, Name, Profile},
};
use msbe_fsops::Digest;
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

/// What a client asks to import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    /// The instance.
    pub instance: Name,
    /// The profile to create or fill.
    pub profile: Name,
    /// The pack file.
    pub input: PathBuf,
    /// The codec ID, or none to detect it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Codec option values.
    #[serde(default)]
    pub options: PackOptions,
}

/// A complete import operation, shown before anything is acquired or written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportPreview {
    /// The request this preview answers.
    pub request: ImportRequest,
    /// The digest of the input as previewed.
    pub input_digest: Digest,
    /// The codec that planned the input.
    pub codec: String,
    /// The pack's title, when it declares one.
    pub title: Option<String>,
    /// The pack layer this import records.
    pub origin: PackOrigin,
    /// Whether the profile will be created.
    pub creates_profile: bool,
    /// Everything that will be obtained, and how.
    pub items: Vec<ImportItem>,
    /// Installation-owned inputs the pack requires.
    pub environment: Vec<EnvironmentRequirement>,
    /// Reasons the import cannot run.
    pub blockers: Vec<PackIssue>,
    /// Concerns to review.
    pub warnings: Vec<PackWarning>,
    /// The staged layer.
    pub layer: StagedLayer,
    /// The codec's plan.
    pub plan: PackImportPlan,
}

/// What an executed import changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    /// The instance.
    pub instance: Name,
    /// The profile.
    pub profile: Name,
    /// The codec.
    pub codec: String,
    /// Whether the profile was created.
    pub created: bool,
    /// The modules the profile now has.
    pub added: Vec<Name>,
    /// Pack-owned files recorded.
    pub configs: usize,
    /// Files the profile deploys.
    pub files: usize,
}

/// Plans importing `request` without acquiring or writing anything.
///
/// # Errors
///
/// Returns an error when the instance is unavailable, the profile already has content, no codec
/// recognizes the input, or the codec cannot plan it. Missing content, environment mismatches and
/// incompatible targets are blockers in the preview.
pub fn preview_import(
    providers: &Providers,
    home: &Home,
    request: ImportRequest,
) -> Result<ImportPreview, PackError> {
    let instance = Instance::open(home, &request.instance)?;
    let existing = existing_profile(&instance, &request.profile)?;
    if let Some(profile) = &existing
        && (!profile.mods.is_empty()
            || !profile.configs.is_empty()
            || profile.pack_layer().is_some())
    {
        return Err(PackError::issue(
            IssueCode::LayerConflict,
            format!(
                "profile {} already has content; import into a new profile, or update its pack layer with pack update",
                request.profile
            ),
        ));
    }
    let target = existing
        .as_ref()
        .and_then(|profile| profile.target.clone())
        .unwrap_or_else(|| stage::default_target(&instance));
    let opened = stage::open(
        providers,
        &instance,
        &target,
        &request.input,
        request.codec.as_deref(),
        &request.options,
    )?;
    if let Some(lockfile) = &opened.plan.lockfile {
        providers.check_extension_pins(&lockfile.extensions)?;
    }
    let staged = stage::stage(&instance, &opened.plan, &target, &BTreeMap::new())?;
    Ok(ImportPreview {
        input_digest: opened.input.digest(),
        codec: opened.plan.codec.clone(),
        title: opened.plan.title.clone(),
        origin: opened.plan.origin.clone(),
        creates_profile: existing.is_none(),
        items: staged.items,
        environment: opened.plan.environment.clone(),
        blockers: staged.blockers,
        warnings: staged.warnings,
        layer: staged.layer,
        plan: opened.plan,
        request,
    })
}

/// Runs exactly the import `preview` describes.
///
/// Every byte is obtained and verified, and the resulting profile is resolved, before the profile
/// is written. A native bundle must reach its lockfile's deployment digest map. A failure or
/// cancellation leaves no profile behind.
///
/// # Errors
///
/// Returns [`PackError::Blocked`] for a preview with blockers, [`IssueCode::StalePlan`] when the
/// input changed, or any acquisition, integrity or resolution failure.
pub fn execute_import(
    providers: &Providers,
    home: &Home,
    preview: &ImportPreview,
    connect: Connect<'_>,
    progress: &dyn Progress,
) -> Result<ImportReport, PackError> {
    if !preview.blockers.is_empty() {
        return Err(PackError::Blocked(preview.blockers.clone()));
    }
    let instance = Instance::open(home, &preview.request.instance)?;
    let input = ZipPackInput::open(&preview.request.input, &Limits::default())?;
    if input.digest() != preview.input_digest {
        return Err(PackError::issue(
            IssueCode::StalePlan,
            "the pack changed after it was previewed",
        ));
    }
    let mut fetcher = Fetcher::new(providers, &instance, &preview.layer.target, connect)?;
    let mods = stage::obtain(
        &mut fetcher,
        &input,
        &preview.plan,
        &preview.layer,
        progress,
    )?;
    let profile = stage::assemble(&preview.layer, &preview.plan, mods);
    if let Some(expected) = &preview.layer.deployment {
        let lockfile = instance.lockfile_for(&preview.request.profile, profile.clone())?;
        if lockfile.deployment != *expected {
            return Err(PackError::issue(
                IssueCode::IntegrityMismatch,
                "the imported profile does not reach the bundle's deployment digest map",
            ));
        }
    }
    checkpoint(progress)?;
    let lockfile = instance.write_profile(&preview.request.profile, &profile)?;
    Ok(ImportReport {
        instance: preview.request.instance.clone(),
        profile: preview.request.profile.clone(),
        codec: preview.codec.clone(),
        created: preview.creates_profile,
        added: profile.load_order(),
        configs: profile.configs.len(),
        files: lockfile.deployment.len(),
    })
}

/// The profile named `name`, or none when it does not exist yet.
pub(crate) fn existing_profile(
    instance: &Instance,
    name: &Name,
) -> Result<Option<Profile>, PackError> {
    match instance.profile(name) {
        Ok(profile) => Ok(Some(profile)),
        Err(InstanceError::UnknownProfile(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
