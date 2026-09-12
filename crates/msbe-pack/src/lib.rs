//! Provider-neutral pack orchestration.
//!
//! This crate selects reviewed codecs, normalizes their option schemas, decides which blobs an
//! export may embed, and runs previewed import, update, export, capture and instance snapshots.
//! It never names a provider, game, loader or external format: those live in the extension that
//! owns them. See `docs/17-pack-formats-and-native-bundles.md`.

mod capture;
mod catalog;
mod error;
mod export;
mod files;
pub mod host;
mod import;
pub mod inclusion;
pub mod options;
mod output;
mod progress;
mod snapshot;
mod stage;
mod update;

use msbe_provider_api::{HttpClient, HttpError};

pub use capture::{
    CaptureItem, CaptureKind, CapturePreview, CaptureReport, CaptureRequest, DiffKind, DiffLine,
    execute_capture, preview_capture,
};
pub use catalog::{CodecOptions, Direction, codec_options, codecs, options_document};
pub use error::{IssueCode, PackError, PackIssue};
pub use export::{ExportPreview, ExportReport, ExportRequest, execute_export, preview_export};
pub use files::plan_digest;
pub use import::{ImportPreview, ImportReport, ImportRequest, execute_import, preview_import};
pub use progress::{Progress, Silent};
pub use snapshot::{
    RestorePreview, SnapshotReport, create_snapshot, preview_restore, restore_snapshot,
};
pub use stage::{ImportAction, ImportItem, StagedLayer, StagedModule};
pub use update::{
    Change, LayerConflict, Resolution, UpdatePreview, UpdateReport, UpdateRequest, execute_update,
    preview_update,
};

/// Opens a network client on first use, so an operation that needs no download never connects.
pub type Connect<'a> = &'a dyn Fn() -> Result<Box<dyn HttpClient>, HttpError>;
