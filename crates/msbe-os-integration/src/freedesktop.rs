//! Link handlers on freedesktop.org desktops, following the Desktop Entry and the MIME
//! Applications Associations specifications.
//!
//! MSBE's registration is one desktop entry, `msbe-handler.desktop`, whose `MimeType` lists every
//! scheme it opens. Each scheme has an `[X-MSBE Scheme <scheme>]` group beside it recording the
//! user list whose default MSBE set, the value it replaced there, and the application that opened
//! the links. The files are read and written here rather than through `xdg-mime`, so no process is
//! started and the outcome does not depend on which desktop's tools are installed.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
};

use msbe_core::config::Freedesktop;

use crate::{
    Error, Owner, Scheme, Status,
    keyfile::{KeyFile, items},
};

/// MSBE's desktop file ID.
const DESKTOP_ID: &str = "msbe-handler.desktop";
const ENTRY: &str = "Desktop Entry";
const DEFAULTS: &str = "Default Applications";
const ADDED: &str = "Added Associations";
const REMOVED: &str = "Removed Associations";
const RECORD: &str = "X-MSBE Scheme ";
const LIST: &str = "mimeapps.list";
/// How deep `applications` subdirectories are searched for desktop entries.
const DEPTH: usize = 8;

/// The freedesktop.org directories registrations are read from and written to.
#[derive(Debug, Clone)]
pub(crate) struct Desktop {
    directories: Freedesktop,
}

/// What MSBE's registration records for one scheme.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Record {
    /// The file name of the user list whose default MSBE set.
    file: Option<String>,
    /// The default MSBE replaced in that list.
    replaced: Option<String>,
    /// The application that opened the scheme's links.
    previous: Option<String>,
}

/// MSBE's desktop entry.
#[derive(Debug, Default)]
struct Registration {
    exec: Option<String>,
    schemes: BTreeMap<Scheme, Record>,
}

impl Registration {
    fn render(&self, exec: &str) -> String {
        let mut text = format!(
            "[{ENTRY}]\nType=Application\nVersion=1.5\nName=MSBE link handler\nComment=Hands provider links to MSBE's download queue\nExec={exec}\nTerminal=false\nNoDisplay=true\nMimeType="
        );
        for scheme in self.schemes.keys() {
            text.push_str(&mime(scheme));
            text.push(';');
        }
        text.push('\n');
        for (scheme, record) in &self.schemes {
            for part in ["\n[", RECORD, scheme.as_str(), "]\n"] {
                text.push_str(part);
            }
            for (key, value) in [
                ("File=", &record.file),
                ("Replaced=", &record.replaced),
                ("Previous=", &record.previous),
            ] {
                if let Some(value) = value {
                    for part in [key, value, "\n"] {
                        text.push_str(part);
                    }
                }
            }
        }
        text
    }
}

impl Desktop {
    pub(crate) const fn new(directories: Freedesktop) -> Self {
        Self { directories }
    }

    pub(crate) fn status(&self, scheme: &Scheme, program: &Path) -> Result<Status, Error> {
        let registration = self.registration()?;
        let record = registration.schemes.get(scheme);
        let owner = self.owner(&mime(scheme));
        Ok(Status {
            scheme: scheme.clone(),
            current: owner == Owner::Msbe
                && record.is_some()
                && registration.exec.as_deref() == Some(exec(program).as_str()),
            previous: record.and_then(|record| record.previous.clone()),
            owner,
        })
    }

    pub(crate) fn register(
        &self,
        scheme: &Scheme,
        program: &Path,
        replace: bool,
    ) -> Result<Status, Error> {
        let mime = mime(scheme);
        let owner = self.owner(&mime);
        if let Owner::Other(name) = &owner
            && !replace
        {
            return Err(Error::Owned {
                scheme: scheme.clone(),
                owner: name.clone(),
            });
        }
        let mut registration = self.registration()?;
        let list_path = self.user_list(&mime)?;
        let mut list = KeyFile::read(&list_path)?.unwrap_or_default();
        let set = list.get(DEFAULTS, &mime).map(str::to_owned);
        let ours = set
            .as_deref()
            .is_some_and(|value| items(value).next() == Some(DESKTOP_ID));
        let file = list_path
            .file_name()
            .and_then(OsStr::to_str)
            .map(str::to_owned);
        let record = if ours {
            // Registering again, as after the program moved, keeps what was replaced the first time.
            Record {
                file,
                ..registration
                    .schemes
                    .get(scheme)
                    .cloned()
                    .unwrap_or_default()
            }
        } else {
            Record {
                file,
                replaced: set,
                previous: match owner {
                    Owner::Other(name) => Some(name),
                    Owner::Nobody | Owner::Msbe => None,
                },
            }
        };
        registration.schemes.insert(scheme.clone(), record);
        // The entry is written first, so the default never names an entry that is not installed.
        write(&self.entry_path(), &registration.render(&exec(program)))?;
        if !ours {
            list.set(DEFAULTS, &mime, DESKTOP_ID);
            write(&list_path, &list.render())?;
        }
        self.status(scheme, program)
    }

    pub(crate) fn unregister(&self, scheme: &Scheme, program: &Path) -> Result<Status, Error> {
        let mime = mime(scheme);
        let mut registration = self.registration()?;
        let record = registration.schemes.remove(scheme).unwrap_or_default();
        for path in self.user_lists() {
            let Some(mut list) = KeyFile::read(&path)? else {
                continue;
            };
            let names_msbe = list
                .get(DEFAULTS, &mime)
                .is_some_and(|value| items(value).next() == Some(DESKTOP_ID));
            if !names_msbe {
                continue;
            }
            let recorded = path.file_name().and_then(OsStr::to_str) == record.file.as_deref();
            match &record.replaced {
                Some(replaced) if recorded => list.set(DEFAULTS, &mime, replaced),
                _ => list.remove(DEFAULTS, &mime),
            }
            write(&path, &list.render())?;
        }
        let entry = self.entry_path();
        if registration.schemes.is_empty() {
            #[expect(
                clippy::disallowed_methods,
                reason = "the entry is MSBE's own link handler registration, outside every instance, so there is nothing to journal"
            )]
            let removed = fs::remove_file(&entry);
            match removed {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    return Err(Error::Io {
                        action: "remove",
                        path: entry,
                        source: error,
                    });
                }
                _ => {}
            }
        } else {
            let exec = registration.exec.clone().unwrap_or_else(|| exec(program));
            write(&entry, &registration.render(&exec))?;
        }
        self.status(scheme, program)
    }

    fn entry_path(&self) -> PathBuf {
        self.directories
            .data_home
            .join("applications")
            .join(DESKTOP_ID)
    }

    /// The directories desktop entries are installed in, most important first.
    fn applications(&self) -> impl Iterator<Item = PathBuf> {
        std::iter::once(&self.directories.data_home)
            .chain(&self.directories.data_dirs)
            .map(|directory| directory.join("applications"))
    }

    /// The lists in a directory, most important first: each current desktop's, then the shared one.
    fn lists_in(&self, directory: &Path) -> Vec<PathBuf> {
        self.directories
            .desktops
            .iter()
            .map(|desktop| directory.join(format!("{desktop}-{LIST}")))
            .chain(std::iter::once(directory.join(LIST)))
            .collect()
    }

    /// The lists the user can change, most important first.
    fn user_lists(&self) -> Vec<PathBuf> {
        self.lists_in(&self.directories.config_home)
    }

    /// Every list, most important first.
    fn lists(&self) -> Vec<PathBuf> {
        let config = std::iter::once(&self.directories.config_home)
            .chain(&self.directories.config_dirs)
            .cloned();
        let data = self.applications();
        config
            .chain(data)
            .flat_map(|directory| self.lists_in(&directory))
            .collect()
    }

    /// The user list whose default for `mime` counts: the most important that sets one, or the
    /// shared list when none does.
    fn user_list(&self, mime: &str) -> Result<PathBuf, Error> {
        for path in self.user_lists() {
            if KeyFile::read(&path)?.is_some_and(|list| list.get(DEFAULTS, mime).is_some()) {
                return Ok(path);
            }
        }
        Ok(self.directories.config_home.join(LIST))
    }

    fn registration(&self) -> Result<Registration, Error> {
        let Some(entry) = KeyFile::read(&self.entry_path())? else {
            return Ok(Registration::default());
        };
        let mut schemes = BTreeMap::new();
        for listed in entry
            .get(ENTRY, "MimeType")
            .map(items)
            .into_iter()
            .flatten()
        {
            let Some(scheme) = listed
                .strip_prefix("x-scheme-handler/")
                .and_then(|scheme| Scheme::parse(scheme).ok())
            else {
                continue;
            };
            let group = format!("{RECORD}{scheme}");
            let recorded = |key| entry.get(&group, key).map(str::to_owned);
            let record = Record {
                file: recorded("File"),
                replaced: recorded("Replaced"),
                previous: recorded("Previous"),
            };
            schemes.insert(scheme, record);
        }
        Ok(Registration {
            exec: entry.get(ENTRY, "Exec").map(str::to_owned),
            schemes,
        })
    }

    fn owner(&self, mime: &str) -> Owner {
        match self.handler(mime) {
            None => Owner::Nobody,
            Some((id, _)) if id == DESKTOP_ID => Owner::Msbe,
            Some((id, entry)) => Owner::Other(match entry.get(ENTRY, "Name") {
                Some(name) if !name.is_empty() => format!("{name} ({id})"),
                _ => id,
            }),
        }
    }

    /// The installed desktop entry that opens `mime`: the first installed default, then the first
    /// added association not removed by a more important list, then the first entry that declares
    /// the type. Lists and entries that cannot be read are skipped, as a desktop would skip them.
    fn handler(&self, mime: &str) -> Option<(String, KeyFile)> {
        let lists: Vec<KeyFile> = self
            .lists()
            .iter()
            .filter_map(|path| KeyFile::read(path).ok().flatten())
            .collect();
        for list in &lists {
            for id in list.get(DEFAULTS, mime).map(items).into_iter().flatten() {
                if let Some(entry) = self.installed(id) {
                    return Some((id.to_owned(), entry));
                }
            }
        }
        let mut removed = BTreeSet::new();
        for list in &lists {
            for id in list.get(ADDED, mime).map(items).into_iter().flatten() {
                if !removed.contains(id)
                    && let Some(entry) = self.installed(id)
                {
                    return Some((id.to_owned(), entry));
                }
            }
            removed.extend(list.get(REMOVED, mime).map(items).into_iter().flatten());
        }
        let mut seen = BTreeSet::new();
        for directory in self.applications() {
            for (id, path) in desktop_files(&directory) {
                // An entry in a more important directory hides every other with its ID.
                if !seen.insert(id.clone()) || removed.contains(id.as_str()) {
                    continue;
                }
                let Some(entry) = KeyFile::read(&path).ok().flatten() else {
                    continue;
                };
                let declares = entry.get(ENTRY, "MimeType").is_some_and(|types| {
                    items(types).any(|declared| declared.eq_ignore_ascii_case(mime))
                });
                if declares && !hidden(&entry) {
                    return Some((id, entry));
                }
            }
        }
        None
    }

    /// The desktop entry installed with `id`, unless it is hidden.
    fn installed(&self, id: &str) -> Option<KeyFile> {
        if !id.ends_with(".desktop") || id.contains('/') {
            return None;
        }
        let path = self
            .applications()
            .find_map(|directory| locate(&directory, id, 0))?;
        KeyFile::read(&path)
            .ok()
            .flatten()
            .filter(|entry| !hidden(entry))
    }
}

/// The MIME type a scheme's links have.
fn mime(scheme: &Scheme) -> String {
    format!("x-scheme-handler/{scheme}")
}

fn hidden(entry: &KeyFile) -> bool {
    entry.get(ENTRY, "Hidden") == Some("true")
}

/// The `Exec` value that runs `program handoff <link>`. The path is quoted with `"`, `` ` ``, `$`
/// and `\` escaped and `%` doubled, then every `\` is escaped again, as a string value requires.
fn exec(program: &Path) -> String {
    let mut quoted = String::from("\"");
    for character in program.to_string_lossy().chars() {
        match character {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(character);
            }
            '%' => quoted.push_str("%%"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    format!("{} handoff %u", quoted.replace('\\', "\\\\"))
}

/// Where the entry with `id` is beneath `directory`: at `id`, or in a subdirectory whose name and
/// a `-` prefix the rest of `id`.
fn locate(directory: &Path, id: &str, depth: usize) -> Option<PathBuf> {
    let direct = directory.join(id);
    if direct.is_file() {
        return Some(direct);
    }
    if depth >= DEPTH {
        return None;
    }
    id.match_indices('-').find_map(|(index, _)| {
        let (prefix, rest) = id.split_at(index);
        let subdirectory = directory.join(prefix);
        subdirectory
            .is_dir()
            .then(|| locate(&subdirectory, rest.strip_prefix('-')?, depth + 1))
            .flatten()
    })
}

/// Every desktop entry beneath `directory` with its desktop file ID, sorted by ID.
fn desktop_files(directory: &Path) -> Vec<(String, PathBuf)> {
    fn collect(directory: &Path, prefix: &str, depth: usize, found: &mut Vec<(String, PathBuf)>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if path.is_dir() {
                if depth < DEPTH {
                    collect(&path, &format!("{prefix}{name}-"), depth + 1, found);
                }
            } else if name.ends_with(".desktop") && path.is_file() {
                found.push((format!("{prefix}{name}"), path));
            }
        }
    }
    let mut found = Vec::new();
    collect(directory, "", 0, &mut found);
    found.sort();
    found
}

/// Replaces `path` with `text` atomically. A list linked into place, as dotfile managers do, is
/// written where it is kept.
fn write(path: &Path, text: &str) -> Result<(), Error> {
    let io_error = |action, path: &Path| {
        let path = path.to_path_buf();
        move |source| Error::Io {
            action,
            path,
            source,
        }
    };
    let target = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::canonicalize(path).map_err(io_error("resolve", path))?
        }
        _ => path.to_path_buf(),
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(io_error("create", parent))?;
    }
    msbe_fsops::atomic::write_file(&target, text.as_bytes())?;
    Ok(())
}
