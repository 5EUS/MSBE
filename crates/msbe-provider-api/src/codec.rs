//! Provider-neutral pack codec contracts.
//!
//! External archive layouts and wire records remain private to the extension that implements a
//! codec. Callers see only descriptors, validated options, import requirements, export plans and
//! content-addressed blob references.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::{Read, Seek, Write},
};

pub use msbe_core::instance::{BlobSource, DistributionDecision, PackFileRole};
use msbe_core::instance::{LockedPlan, LockedTarget, Lockfile};
use msbe_fsops::{Digest, RelPath};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Availability, PackageId};

/// Builds one reviewed pack codec from its provider's validated registration.
pub type BuildPackCodec = fn() -> Result<Box<dyn PackCodec>, PackCodecError>;

/// How a reviewed pack codec joins a provider registration.
#[derive(Debug, Clone, Copy)]
pub struct PackCodecRegistration {
    /// Stable, globally unique codec identifier.
    pub id: &'static str,
    /// Builds the codec implementation.
    pub build: BuildPackCodec,
}

/// A readable, seekable pack input.
pub trait ReadSeek: Read + Seek {}

impl<T: Read + Seek + ?Sized> ReadSeek for T {}

/// A writable, seekable pack output.
pub trait WriteSeek: Write + Seek {}

impl<T: Write + Seek + ?Sized> WriteSeek for T {}

/// A reviewed pack format implementation.
pub trait PackCodec: fmt::Debug + Send + Sync {
    /// Describes discovery, compatibility and user-configurable options.
    fn descriptor(&self) -> &PackCodecDescriptor;

    /// Examines bounded input without network access or mutation.
    ///
    /// # Errors
    ///
    /// Returns [`PackCodecError`] when the input cannot be inspected safely.
    fn probe(&self, input: &mut dyn ReadSeek) -> Result<PackProbe, PackCodecError>;

    /// Parses an input into provider-neutral requirements and embedded blob declarations.
    ///
    /// # Errors
    ///
    /// Returns [`PackCodecError`] when the input, target or options are invalid.
    fn plan_import(
        &self,
        input: &mut dyn ReadSeek,
        context: &PackImportContext,
        options: &PackOptions,
    ) -> Result<PackImportPlan, PackCodecError>;

    /// Plans an export without writing output.
    ///
    /// # Errors
    ///
    /// Returns [`PackCodecError`] when the target, content or options cannot be represented.
    fn plan_export(
        &self,
        context: &PackExportContext<'_>,
        options: &PackOptions,
    ) -> Result<PackExportPlan, PackCodecError>;

    /// Writes a previously validated export plan.
    ///
    /// # Errors
    ///
    /// Returns [`PackCodecError`] when a required blob is unavailable or output cannot be written.
    fn export(
        &self,
        plan: &PackExportPlan,
        blobs: &dyn BlobReader,
        output: &mut dyn WriteSeek,
    ) -> Result<PackExportResult, PackCodecError>;
}

/// Reads verified blobs without exposing store layout to a codec.
pub trait BlobReader {
    /// Streams `digest` into `output`.
    ///
    /// # Errors
    ///
    /// Returns [`PackCodecError`] when the blob is unavailable or cannot be read.
    fn copy_blob(&self, digest: &Digest, output: &mut dyn Write) -> Result<u64, PackCodecError>;
}

/// User-visible codec metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackCodecDescriptor {
    /// Stable, globally unique codec identifier.
    pub id: String,
    /// Provider that owns this external format, or none for a provider-neutral format.
    pub provider: Option<String>,
    /// Human-readable format name.
    pub name: String,
    /// Recognized extensions without a leading dot.
    pub extensions: Vec<String>,
    /// Recognized media types.
    pub media_types: Vec<String>,
    /// Supported operations.
    pub directions: PackDirections,
    /// Supported plan/game identifiers.
    pub supported_games: SupportSet,
    /// User-configurable options.
    pub option_schema: PackOptionSchema,
}

impl PackCodecDescriptor {
    /// Validates descriptor invariants shared by every codec.
    ///
    /// # Errors
    ///
    /// Returns [`PackCodecError::InvalidDescriptor`] for invalid identifiers, duplicate hints or
    /// an invalid option schema.
    pub fn validate(&self) -> Result<(), PackCodecError> {
        if !valid_id(&self.id) {
            return Err(PackCodecError::InvalidDescriptor(format!(
                "codec id {:?} must contain only lowercase ASCII letters, digits and hyphens",
                self.id
            )));
        }
        if self.name.trim().is_empty() {
            return Err(PackCodecError::InvalidDescriptor(
                "codec name must not be empty".to_owned(),
            ));
        }
        unique_nonempty("extension", &self.extensions)?;
        unique_nonempty("media type", &self.media_types)?;
        if !self.directions.import && !self.directions.export {
            return Err(PackCodecError::InvalidDescriptor(
                "codec must support import, export, or both".to_owned(),
            ));
        }
        self.option_schema.validate()
    }
}

/// Operations a codec supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackDirections {
    /// Whether the codec can import.
    pub import: bool,
    /// Whether the codec can export.
    pub export: bool,
}

/// Plan/game IDs accepted by a codec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "ids")]
pub enum SupportSet {
    /// Every plan is supported.
    Universal,
    /// Only these stable plan IDs are supported.
    Explicit(BTreeSet<String>),
}

impl SupportSet {
    /// Whether `game` is declared supported.
    pub fn contains(&self, game: &str) -> bool {
        match self {
            Self::Universal => true,
            Self::Explicit(ids) => ids.contains(game),
        }
    }
}

/// A versioned closed schema for codec options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackOptionSchema {
    /// Option schema version.
    pub schema: u32,
    /// Named complete option sets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<PackPreset>,
    /// Supported fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<PackOptionField>,
    /// Relationships between fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<PackOptionConstraint>,
}

impl PackOptionSchema {
    /// Validates names, defaults, presets and constraints.
    ///
    /// # Errors
    ///
    /// Returns [`PackCodecError::InvalidOptionSchema`] when the schema is ambiguous or refers to
    /// an unknown field.
    pub fn validate(&self) -> Result<(), PackCodecError> {
        if self.schema != 1 {
            return Err(PackCodecError::InvalidOptionSchema(format!(
                "unsupported option schema {}",
                self.schema
            )));
        }
        let mut keys = BTreeSet::new();
        for field in &self.fields {
            if !valid_id(&field.key) || !keys.insert(field.key.as_str()) {
                return Err(PackCodecError::InvalidOptionSchema(format!(
                    "invalid or duplicate option key {:?}",
                    field.key
                )));
            }
            field
                .kind
                .validate_value(&field.default)
                .map_err(|reason| {
                    PackCodecError::InvalidOptionSchema(format!(
                        "invalid default for {:?}: {reason}",
                        field.key
                    ))
                })?;
        }
        let mut presets = BTreeSet::new();
        for preset in &self.presets {
            if !valid_id(&preset.id) || !presets.insert(preset.id.as_str()) {
                return Err(PackCodecError::InvalidOptionSchema(format!(
                    "invalid or duplicate preset id {:?}",
                    preset.id
                )));
            }
            self.validate_options(&preset.values)?;
        }
        for constraint in &self.constraints {
            if !keys.contains(constraint.field()) || !keys.contains(constraint.related()) {
                return Err(PackCodecError::InvalidOptionSchema(
                    "option constraint refers to an unknown field".to_owned(),
                ));
            }
        }
        Ok(())
    }

    /// Validates and fills defaults into user-supplied options.
    ///
    /// # Errors
    ///
    /// Returns [`PackCodecError::InvalidOptions`] for unknown, missing or ill-typed values.
    pub fn normalize(&self, supplied: &PackOptions) -> Result<PackOptions, PackCodecError> {
        self.validate()?;
        let fields: BTreeMap<&str, &PackOptionField> = self
            .fields
            .iter()
            .map(|field| (field.key.as_str(), field))
            .collect();
        for key in supplied.keys() {
            if !fields.contains_key(key.as_str()) {
                return Err(PackCodecError::InvalidOptions(format!(
                    "unknown option {key:?}"
                )));
            }
        }
        let mut normalized = PackOptions::new();
        for field in &self.fields {
            let value = supplied
                .get(&field.key)
                .cloned()
                .unwrap_or_else(|| field.default.clone());
            field.kind.validate_value(&value).map_err(|reason| {
                PackCodecError::InvalidOptions(format!(
                    "invalid value for {:?}: {reason}",
                    field.key
                ))
            })?;
            normalized.insert(field.key.clone(), value);
        }
        Ok(normalized)
    }

    fn validate_options(&self, options: &PackOptions) -> Result<(), PackCodecError> {
        let fields: BTreeMap<&str, &PackOptionField> = self
            .fields
            .iter()
            .map(|field| (field.key.as_str(), field))
            .collect();
        for (key, value) in options {
            let field = fields.get(key.as_str()).ok_or_else(|| {
                PackCodecError::InvalidOptionSchema(format!(
                    "preset contains unknown option {key:?}"
                ))
            })?;
            field.kind.validate_value(value).map_err(|reason| {
                PackCodecError::InvalidOptionSchema(format!(
                    "invalid preset value for {key:?}: {reason}"
                ))
            })?;
        }
        Ok(())
    }
}

/// Normalized option values keyed by stable field ID.
pub type PackOptions = BTreeMap<String, PackOptionValue>;

/// One named option preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackPreset {
    /// Stable preset ID.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Values supplied by the preset.
    pub values: PackOptions,
}

/// One user-configurable codec option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackOptionField {
    /// Stable option key.
    pub key: String,
    /// Human-readable label.
    pub label: String,
    /// Explanatory text.
    pub description: String,
    /// Whether callers must supply a value.
    pub required: bool,
    /// Default value used when callers omit the option.
    pub default: PackOptionValue,
    /// Accepted value shape.
    pub kind: PackOptionKind,
}

/// Closed option value shapes supported by every client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PackOptionKind {
    /// A binary option.
    Boolean,
    /// A bounded integer.
    Integer {
        /// Inclusive minimum.
        min: i64,
        /// Inclusive maximum.
        max: i64,
        /// Positive increment.
        step: i64,
    },
    /// Bounded text.
    Text {
        /// Minimum byte length.
        min_len: usize,
        /// Maximum byte length.
        max_len: usize,
    },
    /// One value from a closed set.
    Choice {
        /// Available choices.
        values: Vec<PackChoice>,
    },
    /// Several values from a closed set.
    MultiChoice {
        /// Available choices.
        values: Vec<PackChoice>,
        /// Minimum selections.
        min: usize,
        /// Maximum selections.
        max: usize,
    },
    /// A caller-selected path.
    Path {
        /// Whether the path is read or written.
        mode: PathMode,
        /// Allowed extensions without a leading dot.
        extensions: Vec<String>,
    },
}

impl PackOptionKind {
    fn validate_value(&self, value: &PackOptionValue) -> Result<(), &'static str> {
        match (self, value) {
            (Self::Boolean, PackOptionValue::Boolean(_)) => Ok(()),
            (Self::Integer { min, max, step }, PackOptionValue::Integer(value))
                if min <= value && value <= max && *step > 0 && (value - min) % step == 0 =>
            {
                Ok(())
            }
            (Self::Text { min_len, max_len }, PackOptionValue::Text(value))
                if (*min_len..=*max_len).contains(&value.len()) =>
            {
                Ok(())
            }
            (Self::Choice { values }, PackOptionValue::Choice(value))
                if values.iter().any(|choice| choice.value == *value) =>
            {
                Ok(())
            }
            (Self::MultiChoice { values, min, max }, PackOptionValue::MultiChoice(selected))
                if (*min..=*max).contains(&selected.len())
                    && selected
                        .iter()
                        .all(|value| values.iter().any(|choice| choice.value == *value)) =>
            {
                Ok(())
            }
            (Self::Path { extensions, .. }, PackOptionValue::Path(path))
                if extensions.is_empty()
                    || path.rsplit_once('.').is_some_and(|(_, extension)| {
                        extensions
                            .iter()
                            .any(|allowed| allowed.eq_ignore_ascii_case(extension))
                    }) =>
            {
                Ok(())
            }
            _ => Err("value does not satisfy its declared option kind"),
        }
    }
}

/// One closed choice displayed by clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackChoice {
    /// Stable serialized value.
    pub value: String,
    /// Human-readable label.
    pub label: String,
}

/// Whether a path option is an input or output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathMode {
    /// An existing input path.
    Open,
    /// A destination path.
    Save,
}

/// A typed option value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum PackOptionValue {
    /// A binary value.
    Boolean(bool),
    /// An integer value.
    Integer(i64),
    /// A text value.
    Text(String),
    /// One selected choice.
    Choice(String),
    /// Several selected choices.
    MultiChoice(Vec<String>),
    /// A path selected by the caller.
    Path(String),
}

/// A relationship between two option fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PackOptionConstraint {
    /// `field` requires `required` to be present.
    Requires {
        /// The dependent field.
        field: String,
        /// The required field.
        required: String,
    },
    /// `field` cannot be used with `conflicting`.
    ConflictsWith {
        /// The first field.
        field: String,
        /// The conflicting field.
        conflicting: String,
    },
    /// `field` is visible when `controller` equals `value`.
    VisibleWhenEquals {
        /// The conditional field.
        field: String,
        /// The controlling field.
        controller: String,
        /// The activating value.
        value: PackOptionValue,
    },
}

impl PackOptionConstraint {
    fn field(&self) -> &str {
        match self {
            Self::Requires { field, .. }
            | Self::ConflictsWith { field, .. }
            | Self::VisibleWhenEquals { field, .. } => field,
        }
    }

    fn related(&self) -> &str {
        match self {
            Self::Requires { required, .. } => required,
            Self::ConflictsWith { conflicting, .. } => conflicting,
            Self::VisibleWhenEquals { controller, .. } => controller,
        }
    }
}

/// Result of bounded format detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackProbe {
    /// Confidence from zero (not this format) to 100 (definitive signature).
    pub confidence: u8,
    /// Optional diagnostic suitable for a selection preview.
    pub reason: Option<String>,
}

/// Context available while parsing an import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackImportContext {
    /// Selected plan ID, when importing into an existing instance.
    pub game: Option<String>,
    /// Selected compatibility target, when known.
    pub target: Option<LockedTarget>,
}

/// Provider-neutral result of parsing a pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackImportPlan {
    /// Codec that produced the plan.
    pub codec: String,
    /// Optional pack title.
    pub title: Option<String>,
    /// Canonical lockfile preserved by a native bundle, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockfile: Option<Lockfile>,
    /// Imported target hints.
    pub target: ImportedTarget,
    /// Content to acquire or request from the user.
    pub requirements: Vec<PackRequirement>,
    /// Blobs contained in the input archive.
    pub embedded: Vec<EmbeddedBlob>,
    /// Non-fatal compatibility or portability warnings.
    pub warnings: Vec<PackWarning>,
}

/// Target facts declared by an imported format.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedTarget {
    /// Stable plan/game ID, when the codec can map it.
    pub game: Option<String>,
    /// Game version, when declared.
    pub game_version: Option<String>,
    /// Stable loader ID, when the codec can map it.
    pub loader: Option<String>,
    /// Loader version, when declared.
    pub loader_version: Option<String>,
}

/// Content needed by an imported pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PackRequirement {
    /// An exact or resolvable provider project.
    Provider {
        /// Stable provider/project identity.
        package: PackageId,
        /// Exact provider version identity, when declared.
        version: Option<String>,
        /// Published integrity values.
        hashes: BTreeMap<String, String>,
        /// Requested destination, when the format declares one.
        destination: Option<RelPath>,
        /// Side availability declared by the format.
        side: Availability,
    },
    /// A direct URL requirement.
    Direct {
        /// Candidate HTTPS URLs.
        urls: Vec<String>,
        /// Published integrity values.
        hashes: BTreeMap<String, String>,
        /// Requested destination, when the format declares one.
        destination: Option<RelPath>,
        /// Side availability declared by the format.
        side: Availability,
    },
    /// Content requiring explicit user action.
    UserAction {
        /// Provider responsible for the action.
        provider: String,
        /// Opaque provider reference.
        reference: String,
        /// Human-readable reason.
        reason: String,
        /// Requested destination, when the format declares one.
        destination: Option<RelPath>,
    },
}

/// One embedded blob declared by an import plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddedBlob {
    /// Safe archive-relative entry path.
    pub entry: RelPath,
    /// Expected content digest.
    pub digest: Digest,
    /// Intended deployment path, when declared.
    pub destination: Option<RelPath>,
}

/// A non-fatal issue surfaced during planning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackWarning {
    /// Stable machine-readable warning code.
    pub code: String,
    /// Human-readable detail.
    pub message: String,
}

/// Resolved profile state available to an exporter.
pub struct PackExportContext<'a> {
    /// Pinned plan identity.
    pub game: &'a LockedPlan,
    /// Pinned compatibility target.
    pub target: &'a LockedTarget,
    /// Canonical lockfile.
    pub lockfile: &'a Lockfile,
    /// Classified files available to the codec.
    pub files: &'a [PackFile],
}

impl fmt::Debug for PackExportContext<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackExportContext")
            .field("game", self.game)
            .field("target", self.target)
            .field("lockfile", self.lockfile)
            .field("files", &self.files)
            .finish()
    }
}

/// One content-addressed file available during export planning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackFile {
    /// Portable deployment path.
    pub path: RelPath,
    /// Exact content digest.
    pub digest: Digest,
    /// Semantic role recorded by core.
    pub role: PackFileRole,
    /// How the exact bytes can be reproduced.
    pub source: BlobSource,
    /// Whether the bytes may be embedded.
    pub distribution: DistributionDecision,
}

/// A serializable export plan reviewed before writing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackExportPlan {
    /// Codec that produced the plan.
    pub codec: String,
    /// Normalized options used to produce the plan.
    pub options: PackOptions,
    /// Files to embed.
    pub embedded: Vec<PackFile>,
    /// Requirements emitted instead of embedded bytes.
    pub requirements: Vec<PackRequirement>,
    /// Non-fatal warnings.
    pub warnings: Vec<PackWarning>,
    /// Codec-private, bounded plan state.
    pub codec_state: serde_json::Value,
}

/// Result of writing an export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackExportResult {
    /// Codec that wrote the output.
    pub codec: String,
    /// Number of embedded blobs.
    pub embedded: usize,
    /// Number of external requirements.
    pub referenced: usize,
    /// SHA-256 of the complete output when the writer can report it.
    pub digest: Option<Digest>,
}

/// Why a pack codec operation failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PackCodecError {
    /// A descriptor is invalid.
    #[error("invalid pack codec descriptor: {0}")]
    InvalidDescriptor(String),
    /// An option schema is invalid.
    #[error("invalid pack option schema: {0}")]
    InvalidOptionSchema(String),
    /// Supplied options are invalid.
    #[error("invalid pack options: {0}")]
    InvalidOptions(String),
    /// The selected direction is unsupported.
    #[error("pack codec does not support {0}")]
    UnsupportedDirection(&'static str),
    /// The selected target is unsupported.
    #[error("pack codec does not support plan {game:?} with loader {loader:?}")]
    UnsupportedTarget {
        /// Stable plan/game ID.
        game: String,
        /// Stable loader ID.
        loader: String,
    },
    /// Input does not match the codec's format.
    #[error("input does not match the pack codec")]
    FormatMismatch,
    /// A manifest or entry exceeds a shared limit.
    #[error("pack input exceeds limit: {0}")]
    Limit(String),
    /// A required blob is unavailable.
    #[error("required blob {0} is unavailable")]
    MissingBlob(Digest),
    /// Content cannot be represented reproducibly.
    #[error("pack content is not reproducible: {0}")]
    Unreproducible(String),
    /// Distribution policy prohibits embedding content.
    #[error("pack content cannot be distributed: {0}")]
    DistributionForbidden(String),
    /// Codec I/O failed.
    #[error("pack codec I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// Codec-specific parsing or encoding failed without exposing a wire type.
    #[error("pack codec failed: {0}")]
    Codec(String),
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn unique_nonempty(kind: &str, values: &[String]) -> Result<(), PackCodecError> {
    let mut seen = BTreeSet::new();
    for value in values {
        let normalized = value.trim().to_ascii_lowercase();
        if normalized.is_empty() || !seen.insert(normalized) {
            return Err(PackCodecError::InvalidDescriptor(format!(
                "{kind} values must be non-empty and unique"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        PackChoice, PackCodecDescriptor, PackDirections, PackOptionField, PackOptionKind,
        PackOptionSchema, PackOptionValue, SupportSet,
    };

    fn schema() -> PackOptionSchema {
        PackOptionSchema {
            schema: 1,
            presets: Vec::new(),
            fields: vec![
                PackOptionField {
                    key: "deterministic".to_owned(),
                    label: "Deterministic".to_owned(),
                    description: "Normalize archive metadata".to_owned(),
                    required: true,
                    default: PackOptionValue::Boolean(true),
                    kind: PackOptionKind::Boolean,
                },
                PackOptionField {
                    key: "blob-mode".to_owned(),
                    label: "Blob mode".to_owned(),
                    description: "Select embedded content".to_owned(),
                    required: true,
                    default: PackOptionValue::Choice("portable".to_owned()),
                    kind: PackOptionKind::Choice {
                        values: vec![PackChoice {
                            value: "portable".to_owned(),
                            label: "Portable".to_owned(),
                        }],
                    },
                },
            ],
            constraints: Vec::new(),
        }
    }

    #[test]
    fn options_are_normalized_in_key_order() {
        let normalized = schema().normalize(&BTreeMap::new()).unwrap();
        assert_eq!(
            normalized.keys().map(String::as_str).collect::<Vec<_>>(),
            ["blob-mode", "deterministic"]
        );
    }

    #[test]
    fn descriptor_rejects_duplicate_detection_hints() {
        let descriptor = PackCodecDescriptor {
            id: "example-pack".to_owned(),
            provider: None,
            name: "Example".to_owned(),
            extensions: vec!["pack".to_owned(), "PACK".to_owned()],
            media_types: Vec::new(),
            directions: PackDirections {
                import: true,
                export: true,
            },
            supported_games: SupportSet::Universal,
            option_schema: schema(),
        };
        assert!(descriptor.validate().is_err());
    }
}
