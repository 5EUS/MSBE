//! Codec discovery and option schemas, as clients see them (§17.3, §17.8).

use msbe_provider_api::{PackCodecDescriptor, PackOptionSchema, PackOptions};
use msbe_providers::Providers;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{IssueCode, PackError, options};

/// The direction a codec is asked to run in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Reading a pack into a profile.
    Import,
    /// Writing a profile as a pack.
    Export,
}

/// A codec's options as a client renders them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodecOptions {
    /// The codec.
    pub codec: String,
    /// Its display name.
    pub name: String,
    /// The direction the schema applies to.
    pub direction: Direction,
    /// The preset applied to `values`, if any.
    pub preset: Option<String>,
    /// The complete schema: common policy fields first for export, then the codec's own.
    pub schema: PackOptionSchema,
    /// Every field's normalized value, as plain JSON.
    pub values: Value,
}

/// Descriptors of permitted codecs supporting `direction` and `game`, in codec ID order.
pub fn codecs(
    providers: &Providers,
    direction: Option<Direction>,
    game: Option<&str>,
) -> Vec<PackCodecDescriptor> {
    providers
        .pack_codecs()
        .into_iter()
        .filter(|descriptor| match direction {
            Some(Direction::Import) => descriptor.directions.import,
            Some(Direction::Export) => descriptor.directions.export,
            None => true,
        })
        .filter(|descriptor| game.is_none_or(|game| descriptor.supported_games.contains(game)))
        .cloned()
        .collect()
}

/// The schema `codec` accepts in `direction` and its values once `preset` is applied.
///
/// # Errors
///
/// Returns [`IssueCode::UnknownCodec`], [`IssueCode::UnsupportedDirection`] or
/// [`IssueCode::InvalidOptions`].
pub fn codec_options(
    providers: &Providers,
    codec: &str,
    direction: Direction,
    preset: Option<&str>,
) -> Result<CodecOptions, PackError> {
    let descriptor = descriptor(providers, codec, direction)?;
    let (schema, values) = match direction {
        Direction::Export => {
            let normalized =
                options::normalize(&descriptor.option_schema, preset, &PackOptions::new())?;
            (
                options::effective_schema(&descriptor.option_schema)?,
                normalized.values,
            )
        }
        Direction::Import => {
            if preset.is_some() {
                return Err(PackError::issue(
                    IssueCode::InvalidOptions,
                    "presets select export policy and do not apply to import",
                ));
            }
            let schema = descriptor.option_schema.clone();
            let values = schema.normalize(&PackOptions::new())?;
            (schema, values)
        }
    };
    Ok(CodecOptions {
        codec: descriptor.id.clone(),
        name: descriptor.name.clone(),
        direction,
        preset: preset.map(str::to_owned),
        schema,
        values: options::to_json(&values),
    })
}

/// Converts a plain JSON options document into values typed by `codec`'s schema in `direction`.
///
/// # Errors
///
/// As for [`codec_options`], or when the document does not match the schema.
pub fn options_document(
    providers: &Providers,
    codec: &str,
    direction: Direction,
    document: &Value,
) -> Result<PackOptions, PackError> {
    let descriptor = descriptor(providers, codec, direction)?;
    let schema = match direction {
        Direction::Export => options::effective_schema(&descriptor.option_schema)?,
        Direction::Import => descriptor.option_schema.clone(),
    };
    options::from_json(&schema, document)
}

/// The permitted codec `id`, checked to support `direction`.
pub(crate) fn descriptor<'a>(
    providers: &'a Providers,
    id: &str,
    direction: Direction,
) -> Result<&'a PackCodecDescriptor, PackError> {
    let descriptor = providers.pack_codec(id)?.descriptor();
    let supported = match direction {
        Direction::Import => descriptor.directions.import,
        Direction::Export => descriptor.directions.export,
    };
    if supported {
        Ok(descriptor)
    } else {
        Err(PackError::issue(
            IssueCode::UnsupportedDirection,
            format!("codec {id} does not support {direction:?}").to_lowercase(),
        ))
    }
}
