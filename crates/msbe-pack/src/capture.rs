//! Capturing in-game changes into a profile's changes layer (§17.5).
//!
//! Capture is always explicit. It only ever adopts files beneath the plan's declared mutable
//! roots, shows each one with its diff first, and records accepted files as pack-owned files.

use std::{fs, io::Read};

use msbe_core::{
    config::Home,
    instance::{Instance, Name, PackFileRole, ProfileLayer},
};
use msbe_fsops::{Digest, RelPath, Store};
use serde::{Deserialize, Serialize};

use crate::{IssueCode, PackError, Progress, error::io_error, progress::checkpoint};

/// Text files larger than this are shown without a diff.
const DIFF_BYTES: u64 = 64 << 10;
/// Files with more lines than this are shown without a diff.
const DIFF_LINES: usize = 1_000;

/// What a client asks to capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    /// The instance.
    pub instance: Name,
    /// The deployed profile.
    pub profile: Name,
    /// Paths to capture; every candidate when empty.
    #[serde(default)]
    pub paths: Vec<RelPath>,
}

/// Why a file is a capture candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureKind {
    /// A deployed file whose contents changed.
    Changed,
    /// A file no deployment placed.
    New,
}

/// Whether a diff line was kept, added or removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiffKind {
    /// Present before and after.
    Context,
    /// Present only after.
    Added,
    /// Present only before.
    Removed,
}

/// One line of a text diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffLine {
    /// Whether the line was kept, added or removed.
    pub kind: DiffKind,
    /// The line, without its terminator.
    pub text: String,
}

/// One file a capture would adopt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureItem {
    /// The instance-relative path.
    pub path: RelPath,
    /// Why it is a candidate.
    pub kind: CaptureKind,
    /// What the deployment placed.
    pub previous: Option<Digest>,
    /// What is on disk now.
    pub current: Digest,
    /// The role it is recorded with.
    pub role: PackFileRole,
    /// The layer it is recorded in.
    pub layer: String,
    /// Its size in bytes.
    pub size: u64,
    /// A line diff, for small text files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<Vec<DiffLine>>,
}

/// A complete capture, shown before anything is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturePreview {
    /// The request this preview answers.
    pub request: CaptureRequest,
    /// The files that would be adopted.
    pub items: Vec<CaptureItem>,
}

/// What an executed capture recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureReport {
    /// The instance.
    pub instance: Name,
    /// The profile.
    pub profile: Name,
    /// The files adopted.
    pub captured: Vec<RelPath>,
}

/// Lists the changes a capture of `request` would adopt.
///
/// # Errors
///
/// Returns an error when the profile is not deployed, a requested path has no capturable change,
/// or the game directory cannot be read.
pub fn preview_capture(home: &Home, request: CaptureRequest) -> Result<CapturePreview, PackError> {
    let instance = Instance::open(home, &request.instance)?;
    let mut candidates = instance.capture_candidates(&request.profile)?;
    if !request.paths.is_empty() {
        if let Some(unknown) = request
            .paths
            .iter()
            .find(|path| !candidates.iter().any(|candidate| candidate.path == **path))
        {
            return Err(PackError::issue(
                IssueCode::InvalidOptions,
                format!("{unknown} has no capturable change beneath the plan's mutable roots"),
            ));
        }
        candidates.retain(|candidate| request.paths.contains(&candidate.path));
    }
    let root = &instance.config().root;
    let items = candidates
        .into_iter()
        .map(|candidate| {
            let file = candidate.path.to_path(root);
            let size = fs::metadata(&file)
                .map_err(io_error("inspect", &file))?
                .len();
            let before = match &candidate.previous {
                Some(previous) => bounded_text(instance.store(), previous)?,
                None => Some(String::new()),
            };
            let after = if size <= DIFF_BYTES {
                fs::read(&file)
                    .map_err(io_error("read", &file))
                    .map(|bytes| String::from_utf8(bytes).ok())?
            } else {
                None
            };
            let diff = before
                .zip(after)
                .and_then(|(before, after)| line_diff(&before, &after));
            Ok(CaptureItem {
                kind: if candidate.previous.is_some() {
                    CaptureKind::Changed
                } else {
                    CaptureKind::New
                },
                path: candidate.path,
                previous: candidate.previous,
                current: candidate.current,
                role: PackFileRole::PackOwnedConfig,
                layer: ProfileLayer::CHANGES.to_owned(),
                size,
                diff,
            })
        })
        .collect::<Result<_, PackError>>()?;
    Ok(CapturePreview { request, items })
}

/// Records exactly the files `preview` lists, in one profile write.
///
/// # Errors
///
/// Returns [`IssueCode::StalePlan`] when a file changed after the preview, or an error reading it or
/// writing the profile.
pub fn execute_capture(
    home: &Home,
    preview: &CapturePreview,
    progress: &dyn Progress,
) -> Result<CaptureReport, PackError> {
    let instance = Instance::open(home, &preview.request.instance)?;
    let root = &instance.config().root;
    let total = u64::try_from(preview.items.len()).unwrap_or(u64::MAX);
    let mut files = Vec::with_capacity(preview.items.len());
    for (done, item) in (0_u64..).zip(&preview.items) {
        checkpoint(progress)?;
        progress.report(done, total, item.path.as_str());
        let file = item.path.to_path(root);
        let bytes = fs::read(&file).map_err(io_error("read", &file))?;
        if Digest::of_bytes(&bytes) != item.current {
            return Err(PackError::issue(
                IssueCode::StalePlan,
                format!("{} changed after the capture was previewed", item.path),
            ));
        }
        files.push((item.path.clone(), bytes));
    }
    checkpoint(progress)?;
    instance.set_profile_configs(&preview.request.profile, &files)?;
    progress.report(total, total, "captured");
    Ok(CaptureReport {
        instance: preview.request.instance.clone(),
        profile: preview.request.profile.clone(),
        captured: files.into_iter().map(|(path, _)| path).collect(),
    })
}

/// A stored blob as text, when it is small UTF-8.
fn bounded_text(store: &Store, digest: &Digest) -> Result<Option<String>, PackError> {
    let path = store.blob_path(digest);
    let mut bytes = Vec::new();
    store
        .open_blob(digest)?
        .take(DIFF_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error("read", &path))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > DIFF_BYTES {
        return Ok(None);
    }
    Ok(String::from_utf8(bytes).ok())
}

/// A minimal line diff by longest common subsequence, or none for long files.
fn line_diff(before: &str, after: &str) -> Option<Vec<DiffLine>> {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    if old.len() > DIFF_LINES || new.len() > DIFF_LINES {
        return None;
    }
    let width = new.len() + 1;
    let cell = |row: usize, column: usize| row * width + column;
    let mut lengths = vec![0_u32; (old.len() + 1) * width];
    for row in (0..old.len()).rev() {
        for column in (0..new.len()).rev() {
            let at = |row, column| lengths.get(cell(row, column)).copied().unwrap_or(0);
            let value = if old.get(row) == new.get(column) {
                at(row + 1, column + 1) + 1
            } else {
                at(row + 1, column).max(at(row, column + 1))
            };
            if let Some(slot) = lengths.get_mut(cell(row, column)) {
                *slot = value;
            }
        }
    }
    let at = |row, column| lengths.get(cell(row, column)).copied().unwrap_or(0);
    let line = |kind, text: &str| DiffLine {
        kind,
        text: text.to_owned(),
    };
    let (mut row, mut column) = (0, 0);
    let mut lines = Vec::new();
    loop {
        match (old.get(row), new.get(column)) {
            (Some(left), Some(right)) if left == right => {
                lines.push(line(DiffKind::Context, left));
                row += 1;
                column += 1;
            }
            (Some(left), Some(_)) if at(row + 1, column) >= at(row, column + 1) => {
                lines.push(line(DiffKind::Removed, left));
                row += 1;
            }
            (_, Some(right)) => {
                lines.push(line(DiffKind::Added, right));
                column += 1;
            }
            (Some(left), None) => {
                lines.push(line(DiffKind::Removed, left));
                row += 1;
            }
            (None, None) => return Some(lines),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DiffKind, line_diff};

    #[test]
    fn line_diffs_mark_additions_and_removals_around_context() {
        let diff = line_diff(
            "fov = 70\nvsync = true\n",
            "fov = 90\nvsync = true\nfps = 144\n",
        )
        .unwrap();
        let kinds: Vec<(DiffKind, &str)> = diff
            .iter()
            .map(|line| (line.kind, line.text.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                (DiffKind::Removed, "fov = 70"),
                (DiffKind::Added, "fov = 90"),
                (DiffKind::Context, "vsync = true"),
                (DiffKind::Added, "fps = 144"),
            ]
        );
    }
}
