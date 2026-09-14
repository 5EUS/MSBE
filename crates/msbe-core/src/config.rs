//! Where MSBE keeps its own state, and the one place the process environment is read.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::instance::{InstanceError, Name};
pub use msbe_plan_schema::ConfigFormat;

/// Merges `layers` in order, so later documents override earlier scalar values.
///
/// Tables and objects merge recursively. Arrays are replaced as a complete value. Properties
/// are treated as a flat, ordered key/value map and emitted deterministically.
///
/// # Errors
///
/// Returns [`ConfigError`] when a layer does not parse as the declared format.
pub fn merge(format: ConfigFormat, layers: &[&[u8]]) -> Result<Vec<u8>, ConfigError> {
    match format {
        ConfigFormat::Json | ConfigFormat::Json5 => merge_json(format, layers),
        ConfigFormat::Toml => merge_toml(layers),
        ConfigFormat::Properties => merge_properties(layers),
    }
}

fn merge_json(format: ConfigFormat, layers: &[&[u8]]) -> Result<Vec<u8>, ConfigError> {
    let mut merged = serde_json::Value::Object(serde_json::Map::new());
    for layer in layers {
        let text = std::str::from_utf8(layer).map_err(ConfigError::Utf8)?;
        let parsed = match format {
            ConfigFormat::Json => serde_json::from_str(text).map_err(ConfigError::Json)?,
            ConfigFormat::Json5 => json5::from_str(text).map_err(ConfigError::Json5)?,
            ConfigFormat::Toml | ConfigFormat::Properties => unreachable!(),
        };
        merge_json_value(&mut merged, parsed);
    }
    serde_json::to_vec_pretty(&merged).map_err(ConfigError::Json)
}

fn merge_json_value(base: &mut serde_json::Value, overlay: serde_json::Value) {
    if let (Some(base), Some(overlay)) = (base.as_object_mut(), overlay.as_object()) {
        for (key, value) in overlay {
            if let Some(existing) = base.get_mut(key) {
                merge_json_value(existing, value.clone());
            } else {
                base.insert(key.clone(), value.clone());
            }
        }
    } else {
        *base = overlay;
    }
}

fn merge_toml(layers: &[&[u8]]) -> Result<Vec<u8>, ConfigError> {
    let mut merged = toml::Value::Table(toml::map::Map::new());
    for layer in layers {
        let text = std::str::from_utf8(layer).map_err(ConfigError::Utf8)?;
        merge_toml_value(
            &mut merged,
            toml::from_str::<toml::Value>(text).map_err(ConfigError::Toml)?,
        );
    }
    toml::to_string_pretty(&merged)
        .map(String::into_bytes)
        .map_err(ConfigError::TomlSerialize)
}

fn merge_toml_value(base: &mut toml::Value, overlay: toml::Value) {
    if let (Some(base), Some(overlay)) = (base.as_table_mut(), overlay.as_table()) {
        for (key, value) in overlay {
            if let Some(existing) = base.get_mut(key) {
                merge_toml_value(existing, value.clone());
            } else {
                base.insert(key.clone(), value.clone());
            }
        }
    } else {
        *base = overlay;
    }
}

fn merge_properties(layers: &[&[u8]]) -> Result<Vec<u8>, ConfigError> {
    let mut merged = BTreeMap::new();
    for layer in layers {
        let text = std::str::from_utf8(layer).map_err(ConfigError::Utf8)?;
        for (line, raw) in text.lines().enumerate() {
            let raw = raw.trim();
            if raw.is_empty() || raw.starts_with(['#', '!']) {
                continue;
            }
            let Some((key, value)) = raw.split_once(['=', ':']) else {
                return Err(ConfigError::Properties { line: line + 1 });
            };
            if key.trim().is_empty() {
                return Err(ConfigError::Properties { line: line + 1 });
            }
            merged.insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }
    let mut output = String::new();
    for (key, value) in merged {
        output.push_str(&key);
        output.push('=');
        output.push_str(&value);
        output.push('\n');
    }
    Ok(output.into_bytes())
}

/// Why structured configuration merging failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// A layer was not UTF-8 text.
    #[error("configuration is not UTF-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    /// A JSON layer was malformed.
    #[error("invalid JSON: {0}")]
    Json(serde_json::Error),
    /// A JSON5 layer was malformed.
    #[error("invalid JSON5: {0}")]
    Json5(json5::Error),
    /// A TOML layer was malformed.
    #[error("invalid TOML: {0}")]
    Toml(toml::de::Error),
    /// The merged TOML document could not be encoded.
    #[error("cannot encode merged TOML: {0}")]
    TomlSerialize(toml::ser::Error),
    /// A properties line has no key/value separator.
    #[error("invalid properties entry on line {line}")]
    Properties {
        /// The one-based source line.
        line: usize,
    },
}

/// MSBE's data directory: instances, their journals, profiles and pinned plans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// Uses `root` as the data directory.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Finds the data directory: `$MSBE_HOME` if set, otherwise `msbe` inside the platform's
    /// per-user data directory.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::HomeUnavailable`] if neither can be determined.
    pub fn discover() -> Result<Self, InstanceError> {
        if let Some(explicit) = env("MSBE_HOME") {
            return Ok(Self::at(explicit));
        }
        platform_data_dir()
            .map(|dir| Self::at(dir.join("msbe")))
            .ok_or(InstanceError::HomeUnavailable)
    }

    /// The data directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding every instance.
    pub fn instances(&self) -> PathBuf {
        self.root.join("instances")
    }

    /// The directory holding one instance's state.
    pub fn instance(&self, name: &Name) -> PathBuf {
        self.instances().join(name.as_str())
    }
}

/// The environment variable that carries `provider`'s token for headless and CI use:
/// `MSBE_<PROVIDER>_TOKEN`, with the provider id uppercased and each `-` written as `_`.
pub fn provider_token_variable(provider: &str) -> String {
    let mut name = String::from("MSBE_");
    name.extend(provider.chars().map(|character| match character {
        '-' => '_',
        other => other.to_ascii_uppercase(),
    }));
    name.push_str("_TOKEN");
    name
}

/// Reads `provider`'s token from its [`provider_token_variable`], when it is set and is text.
///
/// Only `msbe-secrets` should call this, so the value becomes a zeroized secret straight away.
pub fn provider_token(provider: &str) -> Option<String> {
    env(&provider_token_variable(provider)).and_then(|value| value.into_string().ok())
}

/// The names of the [`provider_token_variable`]s set in the environment, so a process MSBE starts
/// can be given an environment without them.
pub fn provider_token_variables() -> Vec<OsString> {
    std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| {
            name.to_str()
                .is_some_and(|name| name.starts_with("MSBE_") && name.ends_with("_TOKEN"))
        })
        .collect()
}

/// The freedesktop.org base directories and desktop names that decide which application opens a
/// link, as the environment sets them.
#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Freedesktop {
    /// `$XDG_CONFIG_HOME`, which holds the user's `mimeapps.list`.
    pub config_home: PathBuf,
    /// `$XDG_CONFIG_DIRS`, most important first.
    pub config_dirs: Vec<PathBuf>,
    /// `$XDG_DATA_HOME`, whose `applications` directory holds the user's desktop entries.
    pub data_home: PathBuf,
    /// `$XDG_DATA_DIRS`, most important first.
    pub data_dirs: Vec<PathBuf>,
    /// `$XDG_CURRENT_DESKTOP`, lowercased, most important first.
    pub desktops: Vec<String>,
}

/// Finds the freedesktop.org base directories, with the XDG Base Directory Specification's defaults
/// for those unset. Relative paths are ignored, as the specification requires.
///
/// Returns `None` when neither `$HOME` nor the variables it would default are set.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn freedesktop() -> Option<Freedesktop> {
    let home = env("HOME").map(PathBuf::from);
    let directory = |key: &str, default: &str| {
        env(key)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| home.as_ref().map(|home| home.join(default)))
    };
    let directories = |key: &str, default: &str| {
        let listed: Vec<PathBuf> = env(key)
            .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter(|path| path.is_absolute())
            .collect();
        if listed.is_empty() {
            std::env::split_paths(default).collect()
        } else {
            listed
        }
    };
    Some(Freedesktop {
        config_home: directory("XDG_CONFIG_HOME", ".config")?,
        config_dirs: directories("XDG_CONFIG_DIRS", "/etc/xdg"),
        data_home: directory("XDG_DATA_HOME", ".local/share")?,
        data_dirs: directories("XDG_DATA_DIRS", "/usr/local/share:/usr/share"),
        desktops: env("XDG_CURRENT_DESKTOP")
            .and_then(|value| value.into_string().ok())
            .map(|value| {
                value
                    .split(':')
                    .filter(|desktop| !desktop.is_empty())
                    .map(str::to_ascii_lowercase)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Reads one environment variable, treating an empty value as unset.
fn env(key: &str) -> Option<OsString> {
    #[expect(
        clippy::disallowed_methods,
        reason = "configuration is read here and nowhere else in the workspace"
    )]
    let value = std::env::var_os(key);
    value.filter(|value| !value.is_empty())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_data_dir() -> Option<PathBuf> {
    env("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| env("HOME").map(|home| PathBuf::from(home).join(".local").join("share")))
}

#[cfg(target_os = "macos")]
fn platform_data_dir() -> Option<PathBuf> {
    env("HOME").map(|home| {
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
    })
}

#[cfg(windows)]
fn platform_data_dir() -> Option<PathBuf> {
    env("APPDATA").map(PathBuf::from)
}

#[cfg(not(any(unix, windows)))]
fn platform_data_dir() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod merge_tests {
    use super::{ConfigFormat, merge};

    #[test]
    fn json_layers_merge_recursively_and_replace_arrays() {
        let merged = merge(
            ConfigFormat::Json,
            &[
                br#"{"video":{"distance":8,"shaders":["a"]}}"#,
                br#"{"video":{"distance":12,"shaders":["b"]}}"#,
            ],
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&merged).unwrap(),
            serde_json::json!({"video":{"distance":12,"shaders":["b"]}})
        );
    }

    #[test]
    fn json5_toml_and_properties_merge_deterministically() {
        let json5 = merge(
            ConfigFormat::Json5,
            &[
                b"{ // default\n render: { clouds: true } }",
                b"{ render: { clouds: false } }",
            ],
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&json5).unwrap(),
            serde_json::json!({"render":{"clouds":false}})
        );
        let toml = merge(
            ConfigFormat::Toml,
            &[b"[render]\nclouds = true\n", b"[render]\ndistance = 12\n"],
        )
        .unwrap();
        assert!(
            std::str::from_utf8(&toml)
                .unwrap()
                .contains("distance = 12")
        );
        assert_eq!(
            merge(
                ConfigFormat::Properties,
                &[b"clouds=true\ndistance=8\n", b"clouds=false\n"]
            )
            .unwrap(),
            b"clouds=false\ndistance=8\n"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::Home;
    use crate::instance::Name;

    #[test]
    fn instance_state_lives_under_the_data_directory() {
        let home = Home::at("/data/msbe");
        let name = Name::new("demo").unwrap();
        assert_eq!(
            home.instances(),
            std::path::Path::new("/data/msbe/instances")
        );
        assert_eq!(
            home.instance(&name),
            std::path::Path::new("/data/msbe/instances/demo")
        );
    }
}
