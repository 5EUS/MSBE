//! Common pack options, presets and option-document conversion (§17.8).
//!
//! These policy fields are prepended to every codec's schema, so no codec declares purpose,
//! reproducibility or blob selection itself, and no codec field can weaken policy.

use msbe_provider_api::{
    PackChoice, PackOptionField, PackOptionKind, PackOptionSchema, PackOptionValue, PackOptions,
    PackPreset,
};
use serde_json::{Map, Value};

use crate::{IssueCode, PackError, host::Compression};

/// Redistribution policy and warning posture.
pub const PURPOSE: &str = "purpose";
/// Whether unresolved exact content fails an export.
pub const REPRODUCIBILITY: &str = "reproducibility";
/// Default embedded-blob selection.
pub const BLOB_MODE: &str = "blob-mode";
/// What forbidden, unsourceable content does.
pub const ON_FORBIDDEN: &str = "on-forbidden";
/// Container compression.
pub const COMPRESSION: &str = "compression";
/// Normalized ordering, timestamps, permissions and metadata.
pub const DETERMINISTIC: &str = "deterministic";

/// Who an export is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Public distribution: content without affirmative rights is never embedded.
    Distribute,
    /// A private transfer: unknown-rights content may be embedded with a warning.
    PrivateTransfer,
}

/// How strictly an export must reproduce every digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reproducibility {
    /// Every digest must be embedded, referenced exactly, derived or environment-bound.
    Strict,
    /// As strict, but content obtainable only by user action is acceptable.
    AllowUserAction,
    /// Unreproducible content is omitted with a warning.
    BestEffort,
}

/// Which blobs an export embeds by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobMode {
    /// Embed nothing; reference everything.
    Thin,
    /// Embed content with no exact source; reference the rest.
    Portable,
    /// Embed every blob policy allows.
    Complete,
}

/// What forbidden content with no exact source does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnForbidden {
    /// Block the export.
    Error,
    /// Record it as an external requirement the recipient must satisfy.
    ExternalRequirement,
}

/// The host-owned policy options every export carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommonOptions {
    /// Who the export is for.
    pub purpose: Purpose,
    /// How strictly every digest must be reproducible.
    pub reproducibility: Reproducibility,
    /// Default blob selection.
    pub blob_mode: BlobMode,
    /// What forbidden unsourceable content does.
    pub on_forbidden: OnForbidden,
    /// Container compression.
    pub compression: Compression,
}

/// Validated options: the complete normalized map and the host policy it selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedOptions {
    /// The preset the values started from, if any.
    pub preset: Option<String>,
    /// Every field, including defaults, in key order.
    pub values: PackOptions,
    /// The host policy the common fields select.
    pub common: CommonOptions,
}

/// The common fields and presets prepended to every codec schema.
pub fn common_schema() -> PackOptionSchema {
    PackOptionSchema {
        schema: 1,
        presets: common_presets(),
        fields: common_fields(),
        constraints: Vec::new(),
    }
}

fn common_presets() -> Vec<PackPreset> {
    vec![
        preset(
            "thin",
            "Thin",
            &[
                (BLOB_MODE, "thin"),
                (PURPOSE, "private-transfer"),
                (REPRODUCIBILITY, "strict"),
            ],
        ),
        preset(
            "portable",
            "Portable",
            &[
                (BLOB_MODE, "portable"),
                (PURPOSE, "private-transfer"),
                (REPRODUCIBILITY, "strict"),
            ],
        ),
        preset(
            "complete",
            "Complete",
            &[
                (BLOB_MODE, "complete"),
                (PURPOSE, "private-transfer"),
                (REPRODUCIBILITY, "strict"),
            ],
        ),
        preset(
            "public-distribution",
            "Public distribution",
            &[
                (BLOB_MODE, "portable"),
                (PURPOSE, "distribute"),
                (REPRODUCIBILITY, "strict"),
                (ON_FORBIDDEN, "error"),
            ],
        ),
    ]
}

fn common_fields() -> Vec<PackOptionField> {
    vec![
        choice(
            PURPOSE,
            "Purpose",
            "Redistribution policy and warning posture. Backups are snapshots, not exports.",
            "private-transfer",
            &[
                ("distribute", "Distribute publicly"),
                ("private-transfer", "Private transfer"),
            ],
        ),
        choice(
            REPRODUCIBILITY,
            "Reproducibility",
            "Whether content that cannot be reproduced exactly fails the export.",
            "strict",
            &[
                ("strict", "Strict"),
                ("allow-user-action", "Allow user action"),
                ("best-effort", "Best effort"),
            ],
        ),
        choice(
            BLOB_MODE,
            "Blob mode",
            "Which content is embedded rather than referenced.",
            "portable",
            &[
                ("thin", "Thin"),
                ("portable", "Portable"),
                ("complete", "Complete"),
            ],
        ),
        choice(
            ON_FORBIDDEN,
            "When redistribution is forbidden",
            "Never embeds; chooses failure or an explicit external requirement.",
            "error",
            &[
                ("error", "Fail the export"),
                ("external-requirement", "Record an external requirement"),
            ],
        ),
        choice(
            COMPRESSION,
            "Compression",
            "Changes bytes and size, never logical content.",
            "deflate",
            &[("deflate", "Deflate"), ("store", "Store")],
        ),
        PackOptionField {
            key: DETERMINISTIC.to_owned(),
            label: "Deterministic".to_owned(),
            description: "Normalized ordering, timestamps, permissions and metadata.".to_owned(),
            required: true,
            default: PackOptionValue::Boolean(true),
            kind: PackOptionKind::Boolean,
        },
    ]
}

/// `codec`'s schema with the common fields and presets prepended.
///
/// # Errors
///
/// Returns [`IssueCode::CodecFailure`] when the codec redeclares a common field or preset, or the
/// combined schema is invalid.
pub fn effective_schema(codec: &PackOptionSchema) -> Result<PackOptionSchema, PackError> {
    let mut schema = common_schema();
    for field in &codec.fields {
        if schema.fields.iter().any(|common| common.key == field.key) {
            return Err(PackError::issue(
                IssueCode::CodecFailure,
                format!("codec option {:?} redeclares a common option", field.key),
            ));
        }
        schema.fields.push(field.clone());
    }
    for preset in &codec.presets {
        if schema.presets.iter().any(|common| common.id == preset.id) {
            return Err(PackError::issue(
                IssueCode::CodecFailure,
                format!("codec preset {:?} redeclares a common preset", preset.id),
            ));
        }
        schema.presets.push(preset.clone());
    }
    schema.constraints.extend(codec.constraints.iter().cloned());
    schema.validate()?;
    Ok(schema)
}

/// Applies `preset`, then `supplied`, to `codec`'s effective schema and fills every default.
///
/// # Errors
///
/// Returns [`IssueCode::InvalidOptions`] for an unknown preset or field, an ill-typed value, or
/// a request for non-deterministic output, which schema-1 hosts do not write.
pub fn normalize(
    codec: &PackOptionSchema,
    preset: Option<&str>,
    supplied: &PackOptions,
) -> Result<NormalizedOptions, PackError> {
    let schema = effective_schema(codec)?;
    let mut merged = PackOptions::new();
    if let Some(id) = preset {
        let found = schema
            .presets
            .iter()
            .find(|candidate| candidate.id == id)
            .ok_or_else(|| invalid(format!("unknown preset {id:?}")))?;
        merged.extend(found.values.clone());
    }
    merged.extend(supplied.clone());
    let values = schema.normalize(&merged)?;
    if values.get(DETERMINISTIC) == Some(&PackOptionValue::Boolean(false)) {
        return Err(invalid(
            "this host writes only deterministic archives; deterministic must be true",
        ));
    }
    Ok(NormalizedOptions {
        preset: preset.map(str::to_owned),
        common: common_options(&values)?,
        values,
    })
}

/// Converts a JSON object of plain values into options typed by `schema`.
///
/// # Errors
///
/// Returns [`IssueCode::InvalidOptions`] for a non-object document, an unknown key, or a value
/// whose JSON type does not match its field.
pub fn from_json(schema: &PackOptionSchema, document: &Value) -> Result<PackOptions, PackError> {
    let object = match document {
        Value::Null => return Ok(PackOptions::new()),
        Value::Object(object) => object,
        _ => return Err(invalid("options must be an object")),
    };
    object
        .iter()
        .map(|(key, value)| {
            let field = schema
                .fields
                .iter()
                .find(|field| field.key == *key)
                .ok_or_else(|| invalid(format!("unknown option {key:?}")))?;
            let wrong = || invalid(format!("option {key:?} has the wrong type"));
            let typed = match (&field.kind, value) {
                (PackOptionKind::Boolean, Value::Bool(flag)) => PackOptionValue::Boolean(*flag),
                (PackOptionKind::Integer { .. }, Value::Number(number)) => {
                    PackOptionValue::Integer(number.as_i64().ok_or_else(wrong)?)
                }
                (PackOptionKind::Text { .. }, Value::String(text)) => {
                    PackOptionValue::Text(text.clone())
                }
                (PackOptionKind::Choice { .. }, Value::String(text)) => {
                    PackOptionValue::Choice(text.clone())
                }
                (PackOptionKind::Path { .. }, Value::String(text)) => {
                    PackOptionValue::Path(text.clone())
                }
                (PackOptionKind::MultiChoice { .. }, Value::Array(items)) => {
                    PackOptionValue::MultiChoice(
                        items
                            .iter()
                            .map(|item| item.as_str().map(str::to_owned).ok_or_else(wrong))
                            .collect::<Result<_, _>>()?,
                    )
                }
                _ => return Err(wrong()),
            };
            Ok((key.clone(), typed))
        })
        .collect()
}

/// Options as a JSON object of plain values, the shape [`from_json`] reads.
pub fn to_json(options: &PackOptions) -> Value {
    Value::Object(
        options
            .iter()
            .map(|(key, value)| {
                let plain = match value {
                    PackOptionValue::Boolean(flag) => Value::Bool(*flag),
                    PackOptionValue::Integer(number) => Value::from(*number),
                    PackOptionValue::Text(text)
                    | PackOptionValue::Choice(text)
                    | PackOptionValue::Path(text) => Value::String(text.clone()),
                    PackOptionValue::MultiChoice(items) => {
                        Value::Array(items.iter().cloned().map(Value::String).collect())
                    }
                };
                (key.clone(), plain)
            })
            .collect::<Map<_, _>>(),
    )
}

fn common_options(values: &PackOptions) -> Result<CommonOptions, PackError> {
    let choice = |key: &str| match values.get(key) {
        Some(PackOptionValue::Choice(value)) => Ok(value.as_str()),
        _ => Err(invalid(format!("option {key:?} is missing"))),
    };
    let unexpected =
        |key: &str, value: &str| invalid(format!("option {key:?} cannot be {value:?}"));
    Ok(CommonOptions {
        purpose: match choice(PURPOSE)? {
            "distribute" => Purpose::Distribute,
            "private-transfer" => Purpose::PrivateTransfer,
            other => return Err(unexpected(PURPOSE, other)),
        },
        reproducibility: match choice(REPRODUCIBILITY)? {
            "strict" => Reproducibility::Strict,
            "allow-user-action" => Reproducibility::AllowUserAction,
            "best-effort" => Reproducibility::BestEffort,
            other => return Err(unexpected(REPRODUCIBILITY, other)),
        },
        blob_mode: match choice(BLOB_MODE)? {
            "thin" => BlobMode::Thin,
            "portable" => BlobMode::Portable,
            "complete" => BlobMode::Complete,
            other => return Err(unexpected(BLOB_MODE, other)),
        },
        on_forbidden: match choice(ON_FORBIDDEN)? {
            "error" => OnForbidden::Error,
            "external-requirement" => OnForbidden::ExternalRequirement,
            other => return Err(unexpected(ON_FORBIDDEN, other)),
        },
        compression: match choice(COMPRESSION)? {
            "deflate" => Compression::Deflate,
            "store" => Compression::Store,
            other => return Err(unexpected(COMPRESSION, other)),
        },
    })
}

fn invalid(message: impl Into<String>) -> PackError {
    PackError::issue(IssueCode::InvalidOptions, message)
}

fn choice(
    key: &str,
    label: &str,
    description: &str,
    default: &str,
    choices: &[(&str, &str)],
) -> PackOptionField {
    PackOptionField {
        key: key.to_owned(),
        label: label.to_owned(),
        description: description.to_owned(),
        required: true,
        default: PackOptionValue::Choice(default.to_owned()),
        kind: PackOptionKind::Choice {
            values: choices
                .iter()
                .map(|(value, label)| PackChoice {
                    value: (*value).to_owned(),
                    label: (*label).to_owned(),
                })
                .collect(),
        },
    }
}

fn preset(id: &str, name: &str, values: &[(&str, &str)]) -> PackPreset {
    PackPreset {
        id: id.to_owned(),
        name: name.to_owned(),
        values: values
            .iter()
            .map(|(key, value)| {
                (
                    (*key).to_owned(),
                    PackOptionValue::Choice((*value).to_owned()),
                )
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use msbe_provider_api::{PackOptionSchema, PackOptionValue, PackOptions};
    use serde_json::json;

    use super::{BlobMode, IssueCode, Purpose, effective_schema, from_json, normalize, to_json};

    fn empty() -> PackOptionSchema {
        PackOptionSchema {
            schema: 1,
            presets: Vec::new(),
            fields: Vec::new(),
            constraints: Vec::new(),
        }
    }

    #[test]
    fn presets_select_policy_and_supplied_values_override_them() {
        let public = normalize(&empty(), Some("public-distribution"), &PackOptions::new()).unwrap();
        assert_eq!(public.common.purpose, Purpose::Distribute);
        assert_eq!(public.common.blob_mode, BlobMode::Portable);
        assert_eq!(public.preset.as_deref(), Some("public-distribution"));
        assert_eq!(public.values.len(), 6, "every default is filled");

        let overridden = normalize(
            &empty(),
            Some("thin"),
            &PackOptions::from([(
                "blob-mode".to_owned(),
                PackOptionValue::Choice("complete".to_owned()),
            )]),
        )
        .unwrap();
        assert_eq!(overridden.common.blob_mode, BlobMode::Complete);
    }

    #[test]
    fn unknown_presets_fields_and_nondeterminism_are_invalid() {
        let unknown = normalize(&empty(), Some("fastest"), &PackOptions::new()).unwrap_err();
        assert_eq!(unknown.code(), IssueCode::InvalidOptions);
        let nondeterministic = normalize(
            &empty(),
            None,
            &PackOptions::from([("deterministic".to_owned(), PackOptionValue::Boolean(false))]),
        )
        .unwrap_err();
        assert_eq!(nondeterministic.code(), IssueCode::InvalidOptions);
        let schema = effective_schema(&empty()).unwrap();
        assert_eq!(
            from_json(&schema, &json!({"shiny": true}))
                .unwrap_err()
                .code(),
            IssueCode::InvalidOptions
        );
    }

    #[test]
    fn json_documents_round_trip_through_the_schema() {
        let schema = effective_schema(&empty()).unwrap();
        let document = json!({"blob-mode": "thin", "deterministic": true});
        let options = from_json(&schema, &document).unwrap();
        assert_eq!(to_json(&options), document);
        assert_eq!(
            from_json(&schema, &json!({"blob-mode": 3}))
                .unwrap_err()
                .code(),
            IssueCode::InvalidOptions
        );
    }
}
