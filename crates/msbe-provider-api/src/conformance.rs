//! The conformance suite every pack codec runs, native or sandboxed.
//!
//! A suite is a JSON document of cases: packs to probe or import, and profiles to export and lay
//! out. Running it against a codec gives a transcript, a canonical record of every result. The
//! extension that owns a format keeps its suite and golden transcript together, and every
//! implementation of that format must reproduce the transcript byte for byte. [`check_invariants`]
//! covers what every codec must do, whatever its format.
//!
//! ```json
//! { "cases": [
//!   { "operation": "probe", "name": "index", "input": { "entries": { "index.json": { "v": 1 } } } },
//!   { "operation": "import", "name": "client", "input": { "entries": {} }, "target": { … } },
//!   { "operation": "export", "name": "fabric", "lockfile": { … }, "files": [], "inclusion": { … } }
//! ] }
//! ```
//!
//! An entry is text, or a JSON value written compactly. Cases compare error messages exactly
//! unless they set `"errors": "kind"`, for an error whose text belongs to the implementation rather
//! than the format. An import origin's digest is recorded as `host-owned`: the host replaces it with
//! the digest of the whole input.

use std::collections::BTreeMap;

use msbe_core::instance::{LockedTarget, Lockfile};
use msbe_fsops::RelPath;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::{
    ContainerKind, Observations, PackCodec, PackCodecDescriptor, PackCodecError, PackEntry,
    PackExportContext, PackExportPlan, PackFile, PackImportContext, PackInclusion, PackInput,
    PackOptions,
};

/// The codec ID [`check_invariants`] gives an export plan no real codec produced.
const FOREIGN: &str = "conformance-foreign";

/// A pack held in memory.
#[derive(Debug, Clone)]
pub struct MemoryInput {
    container: ContainerKind,
    entries: Vec<PackEntry>,
    files: BTreeMap<RelPath, Vec<u8>>,
}

impl MemoryInput {
    /// A pack framed as `container`, holding `files` by path.
    ///
    /// # Errors
    ///
    /// Returns [`ConformanceError::Suite`] when a path is unsafe.
    pub fn new(
        container: ContainerKind,
        files: impl IntoIterator<Item = (String, Vec<u8>)>,
    ) -> Result<Self, ConformanceError> {
        let files = files
            .into_iter()
            .map(|(path, bytes)| Ok((RelPath::new(&path).map_err(suite_error)?, bytes)))
            .collect::<Result<BTreeMap<_, _>, ConformanceError>>()?;
        let entries = files
            .iter()
            .map(|(path, bytes)| PackEntry {
                path: path.clone(),
                size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            })
            .collect();
        Ok(Self {
            container,
            entries,
            files,
        })
    }
}

impl PackInput for MemoryInput {
    fn container(&self) -> ContainerKind {
        self.container
    }

    fn entries(&self) -> &[PackEntry] {
        &self.entries
    }

    fn read(&self, path: &RelPath, limit: u64) -> Result<Vec<u8>, PackCodecError> {
        let bytes = self.files.get(path).ok_or(PackCodecError::FormatMismatch)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
            return Err(PackCodecError::Limit(format!(
                "{path} exceeds the read limit"
            )));
        }
        Ok(bytes.clone())
    }
}

/// Why a codec failed conformance, or a suite could not run.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConformanceError {
    /// The suite or golden transcript is malformed.
    #[error("invalid conformance suite: {0}")]
    Suite(String),
    /// A codec failed an operation an invariant needs to succeed.
    #[error(transparent)]
    Codec(#[from] PackCodecError),
    /// A codec broke a rule every codec must keep.
    #[error("codec {codec:?} {rule}")]
    Invariant {
        /// The codec ID.
        codec: String,
        /// The broken rule.
        rule: String,
    },
    /// A case gave different results on two runs.
    #[error("case {0:?} is not deterministic")]
    Nondeterministic(String),
    /// A case's record differs from the golden transcript.
    #[error(
        "case {case:?} differs from the golden transcript\nexpected: {expected}\nactual:   {actual}"
    )]
    Mismatch {
        /// The first differing case.
        case: String,
        /// Its golden record.
        expected: String,
        /// The record the codec produced.
        actual: String,
    },
}

/// Runs `suite` against `codec`, every case twice, and returns the transcript: pretty-printed JSON
/// with a trailing newline.
///
/// # Errors
///
/// Returns [`ConformanceError::Suite`] for an invalid suite, and
/// [`ConformanceError::Nondeterministic`] for a case whose two runs differ.
pub fn transcript(codec: &dyn PackCodec, suite: &str) -> Result<String, ConformanceError> {
    let suite: Suite = serde_json::from_str(suite).map_err(suite_error)?;
    let mut records = Vec::with_capacity(suite.cases.len());
    for case in &suite.cases {
        let record = case.run(codec)?;
        if record != case.run(codec)? {
            return Err(ConformanceError::Nondeterministic(case.name().to_owned()));
        }
        records.push(record);
    }
    let mut text = serde_json::to_string_pretty(&records).map_err(suite_error)?;
    text.push('\n');
    Ok(text)
}

/// Checks that `codec` reproduces `golden`, the transcript of `suite` kept by its format's owner.
///
/// # Errors
///
/// Returns [`ConformanceError::Mismatch`] naming the first case that differs, or any error
/// [`transcript`] returns.
pub fn check(codec: &dyn PackCodec, suite: &str, golden: &str) -> Result<(), ConformanceError> {
    let actual = transcript(codec, suite)?;
    if actual == golden {
        return Ok(());
    }
    let parse = |text: &str| serde_json::from_str::<Vec<Value>>(text).map_err(suite_error);
    let (expected, produced) = (parse(golden)?, parse(&actual)?);
    let differing = expected
        .iter()
        .zip(&produced)
        .find(|(expected, produced)| expected != produced);
    Err(match differing {
        Some((expected, produced)) => ConformanceError::Mismatch {
            case: expected
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            expected: expected.to_string(),
            actual: produced.to_string(),
        },
        None => ConformanceError::Mismatch {
            case: "the transcript as a whole".to_owned(),
            expected: format!("{} records, exactly as golden", expected.len()),
            actual: format!("{} records, formatted differently", produced.len()),
        },
    })
}

/// Checks what every codec must do, whatever its format: its descriptor is valid; it probes packs
/// in no format at zero confidence rather than failing; and it refuses to lay out an export plan
/// another codec produced.
///
/// # Errors
///
/// Returns [`ConformanceError::Invariant`] naming the broken rule, or
/// [`ConformanceError::Codec`] when an operation fails.
pub fn check_invariants(codec: &dyn PackCodec) -> Result<(), ConformanceError> {
    let descriptor = codec.descriptor();
    descriptor.validate()?;
    let unrelated = [
        MemoryInput::new(ContainerKind::Zip, [])?,
        MemoryInput::new(ContainerKind::Directory, [])?,
        MemoryInput::new(
            ContainerKind::Zip,
            [("readme.txt".to_owned(), b"not a pack".to_vec())],
        )?,
        MemoryInput::new(
            ContainerKind::File,
            [("pack".to_owned(), b"not a pack".to_vec())],
        )?,
    ];
    for input in &unrelated {
        let probe = codec.probe(input)?;
        if probe.confidence != 0 {
            return Err(invariant(
                descriptor,
                format!(
                    "claims a pack in no format with confidence {}",
                    probe.confidence
                ),
            ));
        }
    }
    if descriptor.directions.export {
        let foreign = PackExportPlan {
            codec: FOREIGN.to_owned(),
            options: PackOptions::new(),
            embedded: Vec::new(),
            requirements: Vec::new(),
            environment: Vec::new(),
            warnings: Vec::new(),
            codec_state: Value::Null,
        };
        if codec.layout(&foreign).is_ok() {
            return Err(invariant(
                descriptor,
                "lays out an export plan another codec produced",
            ));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Suite {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Case {
    Probe {
        name: String,
        input: InputSpec,
        #[serde(default)]
        errors: ErrorDetail,
    },
    Import {
        name: String,
        input: InputSpec,
        #[serde(default)]
        game: Option<String>,
        #[serde(default)]
        target: Option<LockedTarget>,
        #[serde(default)]
        options: PackOptions,
        #[serde(default)]
        errors: ErrorDetail,
    },
    Export {
        name: String,
        lockfile: Box<Lockfile>,
        #[serde(default)]
        files: Vec<PackFile>,
        #[serde(default)]
        observations: Observations,
        #[serde(default)]
        inclusion: PackInclusion,
        #[serde(default)]
        options: PackOptions,
        #[serde(default)]
        errors: ErrorDetail,
    },
}

impl Case {
    fn name(&self) -> &str {
        match self {
            Self::Probe { name, .. } | Self::Import { name, .. } | Self::Export { name, .. } => {
                name
            }
        }
    }

    /// One case's record: its name, operation, and the result of each step.
    fn run(&self, codec: &dyn PackCodec) -> Result<Value, ConformanceError> {
        match self {
            Self::Probe {
                name,
                input,
                errors,
            } => Ok(json!({
                "name": name,
                "operation": "probe",
                "result": outcome(codec.probe(&input.build()?), *errors, std::convert::identity)?,
            })),
            Self::Import {
                name,
                input,
                game,
                target,
                options,
                errors,
            } => {
                let context = PackImportContext {
                    game: game.clone(),
                    target: target.clone(),
                };
                let result = codec.plan_import(&input.build()?, &context, options);
                Ok(json!({
                    "name": name,
                    "operation": "import",
                    "result": outcome(result, *errors, host_owned_digest)?,
                }))
            }
            Self::Export {
                name,
                lockfile,
                files,
                observations,
                inclusion,
                options,
                errors,
            } => {
                let context = PackExportContext {
                    game: &lockfile.plan,
                    target: &lockfile.target,
                    lockfile,
                    files,
                    observations,
                    inclusion,
                };
                let plan = codec.plan_export(&context, options);
                let layout = match &plan {
                    Ok(plan) => Some(outcome(codec.layout(plan), *errors, readable_layout)?),
                    Err(_) => None,
                };
                let mut record = json!({
                    "name": name,
                    "operation": "export",
                    "plan": outcome(plan, *errors, std::convert::identity)?,
                });
                if let (Some(layout), Some(fields)) = (layout, record.as_object_mut()) {
                    fields.insert("layout".to_owned(), layout);
                }
                Ok(record)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputSpec {
    #[serde(default = "zip")]
    container: ContainerKind,
    entries: BTreeMap<String, Content>,
}

const fn zip() -> ContainerKind {
    ContainerKind::Zip
}

impl InputSpec {
    fn build(&self) -> Result<MemoryInput, ConformanceError> {
        let files = self
            .entries
            .iter()
            .map(|(path, content)| {
                let bytes = match content {
                    Content::Text(text) => text.clone().into_bytes(),
                    Content::Json(value) => serde_json::to_vec(value).map_err(suite_error)?,
                };
                Ok((path.clone(), bytes))
            })
            .collect::<Result<Vec<_>, ConformanceError>>()?;
        MemoryInput::new(self.container, files)
    }
}

/// An entry's contents: text as written, or a JSON value written compactly.
#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Json(Value),
}

/// How much of an error a case compares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ErrorDetail {
    /// The kind and the message.
    #[default]
    Message,
    /// The kind only.
    Kind,
}

/// A result as a record: `{"ok": value}` shaped by `shape`, or `{"error": {"kind", "message"}}`.
fn outcome<T: Serialize>(
    result: Result<T, PackCodecError>,
    errors: ErrorDetail,
    shape: fn(Value) -> Value,
) -> Result<Value, ConformanceError> {
    Ok(match result {
        Ok(value) => json!({ "ok": shape(serde_json::to_value(value).map_err(suite_error)?) }),
        Err(error) => {
            let mut failure = json!({ "kind": kind(&error) });
            if let (ErrorDetail::Message, Some(fields)) = (errors, failure.as_object_mut()) {
                fields.insert("message".to_owned(), Value::String(error.to_string()));
            }
            json!({ "error": failure })
        }
    })
}

/// An import plan with its origin digest marked as the host's to fill in.
fn host_owned_digest(mut plan: Value) -> Value {
    if let Some(digest) = plan.pointer_mut("/origin/digest") {
        *digest = Value::String("host-owned".to_owned());
    }
    plan
}

/// A layout with inline UTF-8 contents written as text, so a golden transcript reads as a diff.
fn readable_layout(mut layout: Value) -> Value {
    let entries = layout.get_mut("entries").and_then(Value::as_array_mut);
    for entry in entries.into_iter().flatten() {
        let Some(content) = entry.get_mut("content") else {
            continue;
        };
        if content.get("kind").and_then(Value::as_str) != Some("inline") {
            continue;
        }
        let bytes: Option<Vec<u8>> =
            content
                .get("value")
                .and_then(Value::as_array)
                .and_then(|values| {
                    values
                        .iter()
                        .map(|value| value.as_u64().and_then(|byte| u8::try_from(byte).ok()))
                        .collect()
                });
        if let Some(text) = bytes.and_then(|bytes| String::from_utf8(bytes).ok()) {
            *content = json!({ "kind": "inline", "text": text });
        }
    }
    layout
}

/// The stable name of an error's kind, as the codec ABI spells it.
const fn kind(error: &PackCodecError) -> &'static str {
    match error {
        PackCodecError::InvalidDescriptor(_) => "invalid_descriptor",
        PackCodecError::InvalidOptionSchema(_) => "invalid_option_schema",
        PackCodecError::InvalidOptions(_) => "invalid_options",
        PackCodecError::UnsupportedDirection(_) => "unsupported_direction",
        PackCodecError::UnsupportedTarget { .. } => "unsupported_target",
        PackCodecError::FormatMismatch => "format_mismatch",
        PackCodecError::Limit(_) => "limit",
        PackCodecError::UnsafePath(_) => "unsafe_path",
        PackCodecError::MissingBlob(_) => "missing_blob",
        PackCodecError::Unreproducible(_) => "unreproducible",
        PackCodecError::DistributionForbidden(_) => "distribution_forbidden",
        PackCodecError::Io(_) => "io",
        PackCodecError::Codec(_) => "codec",
    }
}

fn invariant(descriptor: &PackCodecDescriptor, rule: impl Into<String>) -> ConformanceError {
    ConformanceError::Invariant {
        codec: descriptor.id.clone(),
        rule: rule.into(),
    }
}

fn suite_error(error: impl std::fmt::Display) -> ConformanceError {
    ConformanceError::Suite(error.to_string())
}
