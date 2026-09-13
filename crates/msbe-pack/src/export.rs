//! Previewed, policy-gated export (§17.11).
//!
//! A preview is the complete operation: the host's inclusion decision, the codec's plan and every
//! blocker. Execution runs exactly that preview, and refuses one with blockers.

use std::{collections::BTreeSet, path::PathBuf};

use msbe_core::{
    config::Home,
    instance::{Instance, Name},
};
use msbe_fsops::{Digest, Store};
use msbe_provider_api::{
    EntryContent, EnvironmentRequirement, Observations, PackExportContext, PackExportPlan,
    PackOptions, PackRequirement, PackWarning,
};
use msbe_providers::Providers;
use serde::{Deserialize, Serialize};

use crate::{
    IssueCode, PackError, PackIssue, Progress,
    catalog::{self, Direction},
    files,
    inclusion::{self, InclusionItem, ObservationUse, PreviewGroup},
    options, output,
};

/// What a client asks to export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    /// The instance.
    pub instance: Name,
    /// The profile.
    pub profile: Name,
    /// The codec ID.
    pub codec: String,
    /// A preset to start from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Supplied option values, applied over the preset.
    #[serde(default)]
    pub options: PackOptions,
    /// The destination file.
    pub output: PathBuf,
}

/// A complete export operation, shown before anything is written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportPreview {
    /// The request this preview answers.
    pub request: ExportRequest,
    /// The codec's display name.
    pub codec_name: String,
    /// Every option's normalized value.
    pub options: PackOptions,
    /// The digest of the canonical lockfile the preview planned over.
    pub lockfile: Digest,
    /// Every required file with its preview group.
    pub items: Vec<InclusionItem>,
    /// Exact requirements the export records instead of bytes.
    pub requirements: Vec<PackRequirement>,
    /// Installation-owned inputs the recipient must already have.
    pub environment: Vec<EnvironmentRequirement>,
    /// Reasons the export cannot run.
    pub blockers: Vec<PackIssue>,
    /// Concerns to review.
    pub warnings: Vec<PackWarning>,
    /// Observations the plan relied on, with when each was taken.
    pub observations: Vec<ObservationUse>,
    /// Bytes of embedded content.
    pub embedded_bytes: u64,
    /// The codec's plan, absent when the codec could not plan the export.
    pub plan: Option<PackExportPlan>,
}

/// What an executed export wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportReport {
    /// The file written.
    pub output: PathBuf,
    /// The codec.
    pub codec: String,
    /// The digest of the written file.
    pub digest: Digest,
    /// Container entries written.
    pub entries: usize,
    /// Distinct blobs embedded.
    pub embedded: usize,
    /// Requirements recorded instead of bytes.
    pub requirements: usize,
}

/// Plans `request` without writing anything.
///
/// # Errors
///
/// Returns an error when the instance, profile or codec is unavailable, the codec cannot export
/// this plan, or options are invalid. Policy and reproducibility problems are blockers in the
/// preview, not errors.
pub fn preview_export(
    providers: &Providers,
    home: &Home,
    request: ExportRequest,
) -> Result<ExportPreview, PackError> {
    let descriptor = catalog::descriptor(providers, &request.codec, Direction::Export)?;
    let codec = providers.pack_codec(&request.codec)?;
    let instance = Instance::open(home, &request.instance)?;
    let profile = instance.profile(&request.profile)?;
    // A signed program is pinned only when the profile has content from its provider.
    let pins = providers.extension_pins_for(
        profile
            .mods
            .values()
            .filter_map(|entry| entry.provider.as_ref())
            .map(|provenance| provenance.provider.as_str()),
    );
    let lockfile =
        instance.lockfile_for_with_extensions(&request.profile, profile.clone(), pins)?;
    if !descriptor.supported_games.contains(&lockfile.plan.id) {
        return Err(PackError::issue(
            IssueCode::UnsupportedTarget,
            format!(
                "codec {} does not support plan {}",
                descriptor.id, lockfile.plan.id
            ),
        ));
    }
    let normalized = options::normalize(
        &descriptor.option_schema,
        request.preset.as_deref(),
        &request.options,
    )?;
    let classified = files::pack_files(&profile, &lockfile);
    let cached = files::observations(instance.store())?;
    let planned = inclusion::plan(&classified, &cached, normalized.common);
    let relied = Observations {
        entries: cached
            .entries
            .into_iter()
            .filter(|(subject, _)| {
                planned
                    .observations
                    .iter()
                    .any(|used| used.subject == *subject)
            })
            .collect(),
    };
    let mut blockers = planned.blockers;
    let mut warnings = planned.warnings;
    let mut items = planned.items;
    let context = PackExportContext {
        game: &lockfile.plan,
        target: &lockfile.target,
        lockfile: &lockfile,
        files: &classified,
        observations: &relied,
        inclusion: &planned.inclusion,
    };
    let plan = match codec.plan_export(&context, &normalized.values) {
        Ok(plan) => Some(plan),
        Err(error) => {
            blockers.extend(PackError::from(error).issues());
            None
        }
    };
    let (requirements, environment, embedded_bytes) = match &plan {
        Some(plan) => {
            let (issues, bytes) = gate(
                plan,
                &descriptor.id,
                &planned.inclusion.permitted,
                instance.store(),
            );
            blockers.extend(issues);
            regroup(&mut items, plan);
            warnings.extend(plan.warnings.iter().cloned());
            (plan.requirements.clone(), plan.environment.clone(), bytes)
        }
        None => (
            planned.inclusion.requirements,
            planned.inclusion.environment,
            0,
        ),
    };
    let lockfile = files::plan_digest(&lockfile)?;
    Ok(ExportPreview {
        codec_name: descriptor.name.clone(),
        options: normalized.values,
        lockfile,
        items,
        requirements,
        environment,
        blockers,
        warnings,
        observations: planned.observations,
        embedded_bytes,
        plan,
        request,
    })
}

/// Blocks every digest the codec would embed that policy does not permit, and counts the bytes the
/// export embeds. A codec never widens what the host decided may be embedded.
fn gate(
    plan: &PackExportPlan,
    codec: &str,
    permitted: &BTreeSet<Digest>,
    store: &Store,
) -> (Vec<PackIssue>, u64) {
    let mut issues: Vec<PackIssue> = plan
        .embedded
        .iter()
        .filter(|file| !permitted.contains(&file.digest))
        .map(|file| PackIssue {
            code: IssueCode::DistributionForbidden,
            path: Some(file.path.clone()),
            digest: Some(file.digest),
            message: format!(
                "codec {codec} would embed {}, which policy does not permit",
                file.path
            ),
        })
        .collect();
    let embedded: BTreeSet<Digest> = plan.embedded.iter().map(|file| file.digest).collect();
    let mut bytes = 0_u64;
    for digest in embedded {
        match files::blob_size(store, &digest) {
            Some(size) => bytes = bytes.saturating_add(size),
            None => issues.push(PackIssue {
                code: IssueCode::MissingBlob,
                path: None,
                digest: Some(digest),
                message: format!("the store does not hold {digest}"),
            }),
        }
    }
    (issues, bytes)
}

/// Moves references the codec chose to embed instead into their embedded preview group.
fn regroup(items: &mut [InclusionItem], plan: &PackExportPlan) {
    let embedded: BTreeSet<Digest> = plan.embedded.iter().map(|file| file.digest).collect();
    for item in items.iter_mut().filter(|item| {
        embedded.contains(&item.digest) && item.group == PreviewGroup::ProviderReference
    }) {
        item.group = inclusion::embedded_group(item.role);
    }
}

/// Writes exactly what `preview` describes.
///
/// The output is written beside its destination under a temporary name while every blob is hashed,
/// its entries are verified against the layout, and only then is it renamed into place. A failure
/// or cancellation leaves the destination untouched.
///
/// # Errors
///
/// Returns [`PackError::Blocked`] for a preview with blockers, or any layout, store, integrity or
/// write failure.
pub fn execute_export(
    providers: &Providers,
    home: &Home,
    preview: &ExportPreview,
    progress: &dyn Progress,
) -> Result<ExportReport, PackError> {
    if !preview.blockers.is_empty() {
        return Err(PackError::Blocked(preview.blockers.clone()));
    }
    let plan = preview.plan.as_ref().ok_or_else(|| {
        PackError::issue(IssueCode::CodecFailure, "the preview has no codec plan")
    })?;
    let descriptor = catalog::descriptor(providers, &preview.request.codec, Direction::Export)?;
    let codec = providers.pack_codec(&preview.request.codec)?;
    let compression = options::normalize(&descriptor.option_schema, None, &preview.options)?
        .common
        .compression;
    let instance = Instance::open(home, &preview.request.instance)?;
    let layout = codec.layout(plan)?;
    let embedded: BTreeSet<Digest> = plan.embedded.iter().map(|file| file.digest).collect();
    if let Some(stray) = layout.entries.iter().find_map(|entry| match entry.content {
        EntryContent::Blob(digest) if !embedded.contains(&digest) => Some(digest),
        _ => None,
    }) {
        return Err(PackError::issue(
            IssueCode::DistributionForbidden,
            format!("the codec layout names blob {stray}, which its plan does not embed"),
        ));
    }

    let destination = &preview.request.output;
    let digest = output::write(
        &layout,
        instance.store(),
        compression,
        destination,
        progress,
    )?;
    Ok(ExportReport {
        output: destination.clone(),
        codec: plan.codec.clone(),
        digest,
        entries: layout.entries.len(),
        embedded: embedded.len(),
        requirements: plan.requirements.len(),
    })
}
