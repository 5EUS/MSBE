//! `option-installer`: an example installer delivered as a sandboxed MSBE plan step extension.
//!
//! A mod built for it ships `installer.json` beside its files:
//!
//! ```json
//! {
//!   "format": "option-installer",
//!   "version": 1,
//!   "always": "core",
//!   "groups": [{
//!     "id": "textures",
//!     "prompt": "Texture resolution",
//!     "default": "standard",
//!     "options": [
//!       { "id": "standard", "label": "Standard", "directory": "options/standard" },
//!       { "id": "high", "label": "High", "directory": "options/high", "requires": "Data/HighRes.esm" }
//!     ]
//!   }]
//! }
//! ```
//!
//! Files under `always` are placed unconditionally. Each group becomes one choice question whose
//! choices are the options this game offers: an option that `requires` a game file is offered only
//! when the game has that file. The chosen option's files are placed, winning over an `always` file
//! at the same destination, and the choices are written to `<module>.choices.txt`. A mod without
//! `installer.json` has every file placed as it is.
//!
//! Everything lands beneath the step's `into` parameter, or else its first root. The extension
//! needs `archive-read`, `ui-prompt`, and `game-read` for any `requires`, and emits `place` and
//! `write-file`. It is the reference for FOMOD-style installers.

use std::collections::BTreeMap;

use msbe_step_guest::{Choice, Context, Error, Operation, Question, Request, Step, serde_json};
use serde::Deserialize;

const MANIFEST: &str = "installer.json";
const MANIFEST_LIMIT: u64 = 1 << 20;
const FORMAT: &str = "option-installer";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    version: u32,
    #[serde(default)]
    always: Option<String>,
    #[serde(default)]
    groups: Vec<Group>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Group {
    id: String,
    prompt: String,
    #[serde(default)]
    default: Option<String>,
    options: Vec<Offer>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Offer {
    id: String,
    label: String,
    directory: String,
    #[serde(default)]
    requires: Option<String>,
}

/// The option-installer step.
#[derive(Debug)]
pub struct OptionInstaller;

impl Step for OptionInstaller {
    fn run(context: &Context, request: Request) -> Result<Vec<Operation>, Error> {
        let root = destination(&request)?;
        let sources: Vec<String> = context
            .entries()?
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        if !sources.iter().any(|source| source == MANIFEST) {
            return Ok(sources
                .into_iter()
                .map(|source| Operation::Place {
                    path: format!("{root}/{source}"),
                    source,
                })
                .collect());
        }
        let manifest: Manifest = serde_json::from_slice(&context.read(MANIFEST, MANIFEST_LIMIT)?)
            .map_err(Error::invalid_archive)?;
        if manifest.format != FORMAT || manifest.version != 1 {
            return Err(Error::invalid_archive(format!(
                "{MANIFEST} is not an {FORMAT} version 1 manifest"
            )));
        }

        let mut placed = BTreeMap::new();
        if let Some(always) = &manifest.always {
            place(&mut placed, &sources, always, &root);
        }
        let mut choices = String::new();
        for group in &manifest.groups {
            let Some(chosen) = choose(context, group)? else {
                continue;
            };
            place(&mut placed, &sources, &chosen.directory, &root);
            choices.push_str(&group.id);
            choices.push('=');
            choices.push_str(&chosen.id);
            choices.push('\n');
        }

        let mut operations: Vec<Operation> = placed
            .into_iter()
            .map(|(path, source)| Operation::Place { source, path })
            .collect();
        if !choices.is_empty() {
            operations.push(Operation::WriteFile {
                path: format!("{root}/{}.choices.txt", request.module),
                text: choices,
            });
        }
        Ok(operations)
    }
}

/// The option of `group` that the recorded answer, or the default, picks among those offered.
fn choose<'a>(context: &Context, group: &'a Group) -> Result<Option<&'a Offer>, Error> {
    let mut offered = Vec::new();
    for offer in &group.options {
        let available = match &offer.requires {
            Some(path) => context.game_has(path)?,
            None => true,
        };
        if available {
            offered.push(offer);
        }
    }
    let Some(first) = offered.first().copied() else {
        return Ok(None);
    };
    let default = group
        .default
        .as_ref()
        .filter(|id| offered.iter().any(|offer| &offer.id == *id))
        .unwrap_or(&first.id);
    let question = Question::choice(
        &group.id,
        &group.prompt,
        offered
            .iter()
            .map(|offer| Choice::new(&offer.id, &offer.label))
            .collect(),
    )
    .with_default(default);
    let answer = context.ask(&question)?;
    offered
        .into_iter()
        .find(|offer| offer.id == answer)
        .map(Some)
        .ok_or_else(|| Error::extension(format!("{answer:?} is not an option of {}", group.id)))
}

/// Places every mod file beneath `directory` at the same relative path beneath `root`.
fn place(placed: &mut BTreeMap<String, String>, sources: &[String], directory: &str, root: &str) {
    let prefix = format!("{}/", directory.trim_end_matches('/'));
    for source in sources {
        if let Some(relative) = source.strip_prefix(&prefix) {
            placed.insert(format!("{root}/{relative}"), source.clone());
        }
    }
}

/// Where files land: the `into` parameter when it lies within a root, or else the first root.
fn destination(request: &Request) -> Result<String, Error> {
    match request.parameters.get("into") {
        Some(into)
            if request
                .roots
                .iter()
                .any(|root| into == root || into.starts_with(&format!("{root}/"))) =>
        {
            Ok(into.clone())
        }
        Some(into) => Err(Error::extension(format!(
            "the into parameter {into:?} is outside the step's roots"
        ))),
        None => request
            .roots
            .first()
            .cloned()
            .ok_or_else(|| Error::extension("the step has no roots to install into")),
    }
}

msbe_step_guest::export_step!(OptionInstaller);

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use msbe_step_guest::{Context, Operation, Request, Step};

    use super::OptionInstaller;

    const MANIFEST: &str = r#"{
        "format": "option-installer", "version": 1, "always": "core",
        "groups": [{ "id": "textures", "prompt": "Textures", "default": "standard", "options": [
            { "id": "standard", "label": "Standard", "directory": "options/standard" },
            { "id": "high", "label": "High", "directory": "options/high", "requires": "Data/High.esm" }
        ]}]
    }"#;

    fn request() -> Request {
        serde_json::from_value(serde_json::json!({
            "step": "install", "module": "pack", "loader": "default", "game_version": null,
            "parameters": {}, "roots": ["mods"]
        }))
        .unwrap()
    }

    fn archive() -> Vec<(String, Vec<u8>)> {
        [
            ("installer.json", MANIFEST.as_bytes()),
            ("core/readme.txt", b"core".as_slice()),
            ("options/standard/texture.dds", b"standard".as_slice()),
            ("options/high/texture.dds", b"high".as_slice()),
        ]
        .into_iter()
        .map(|(path, bytes)| (path.to_owned(), bytes.to_vec()))
        .collect()
    }

    #[test]
    fn places_the_answered_option_only_when_the_game_offers_it() {
        let high = [("textures".to_owned(), "high".to_owned())];
        let game = [("Data/High.esm".to_owned(), b"esm".to_vec())];
        let context = Context::native(archive(), game, high.clone());
        let operations = OptionInstaller::run(&context, request()).unwrap();
        let placed: BTreeMap<&str, &str> = operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::Place { source, path } => Some((path.as_str(), source.as_str())),
                Operation::WriteFile { .. } => None,
            })
            .collect();
        assert_eq!(placed.get("mods/texture.dds"), Some(&"options/high/texture.dds"));
        assert_eq!(placed.get("mods/readme.txt"), Some(&"core/readme.txt"));

        let without = Context::native(archive(), [], high);
        assert!(OptionInstaller::run(&without, request()).is_err());
    }
}
