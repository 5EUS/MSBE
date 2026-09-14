//! Link handlers on Windows: a URL protocol class key beneath the current user's hive.
//!
//! MSBE's key has an `MSBE` subkey naming the program. When MSBE replaced another application's
//! per-user key, a copy of that key is kept beneath `MSBE\Previous`, and unregistering puts it back.
//! Nothing beneath `HKEY_LOCAL_MACHINE` is written: a machine-wide handler is only reported, and
//! opens the links again once MSBE's per-user key is gone.
//!
//! A key is replaced by building the new one under a staging name and renaming it into place, so a
//! failure part-way never leaves the scheme with neither key.

use std::path::Path;

use windows_registry::{CURRENT_USER, Key, LOCAL_MACHINE, Value};

use crate::{Error, Owner, Scheme, Status};

const CLASSES: &str = r"Software\Classes";
const MARKER: &str = "MSBE";
const PREVIOUS: &str = r"MSBE\Previous";
const COMMAND: &str = r"shell\open\command";
/// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`, `0x80070002`.
const NOT_FOUND: i32 = -2_147_024_894;
/// How deep a replaced key is copied.
const DEPTH: usize = 16;

/// A classes key beneath the current user's hive.
#[derive(Debug, Clone)]
pub(crate) struct Classes {
    root: String,
    /// Whether the local machine's classes are also consulted, as Windows merges them.
    machine: bool,
}

/// A copied key: its values and subkeys.
struct Tree {
    values: Vec<(String, Value)>,
    keys: Vec<(String, Self)>,
}

impl Classes {
    pub(crate) fn current_user() -> Self {
        Self {
            root: CLASSES.to_owned(),
            machine: true,
        }
    }

    pub(crate) fn at(root: &str) -> Self {
        Self {
            root: root.to_owned(),
            machine: false,
        }
    }

    fn path(&self, name: &str) -> String {
        format!(r"{}\{name}", self.root)
    }

    pub(crate) fn status(&self, scheme: &Scheme, program: &Path) -> Result<Status, Error> {
        let path = self.path(scheme.as_str());
        let mut status = Status {
            scheme: scheme.clone(),
            owner: Owner::Nobody,
            current: false,
            previous: None,
        };
        match open(CURRENT_USER, &path)? {
            Some(key) => {
                if let Some(marker) = open(&key, MARKER)? {
                    status.owner = Owner::Msbe;
                    status.previous = marker.get_string("Previous").ok();
                    status.current = marker.get_string("Program").ok().as_deref()
                        == program.to_str()
                        && open(&key, COMMAND)?.and_then(|command| command.get_string("").ok())
                            == Some(command(program));
                } else if handles_links(&key) {
                    status.owner = Owner::Other(describe(&key, &path));
                } else {
                    status.owner = self.machine_owner(scheme)?;
                }
            }
            None => status.owner = self.machine_owner(scheme)?,
        }
        Ok(status)
    }

    pub(crate) fn register(
        &self,
        scheme: &Scheme,
        program: &Path,
        replace: bool,
    ) -> Result<Status, Error> {
        let status = self.status(scheme, program)?;
        let path = self.path(scheme.as_str());
        match &status.owner {
            Owner::Other(owner) if !replace => {
                return Err(Error::Owned {
                    scheme: scheme.clone(),
                    owner: owner.clone(),
                });
            }
            Owner::Msbe => {
                // Registering again updates the program in place and keeps what was replaced.
                let key = create(CURRENT_USER, &path)?;
                write_registration(&key, scheme, program, &path)?;
                return self.status(scheme, program);
            }
            Owner::Other(_) | Owner::Nobody => {}
        }
        let replaced = match open(CURRENT_USER, &path)? {
            Some(existing) => Some(read_tree(&existing, 0, &path)?),
            None => None,
        };
        let staging = self.staging(scheme);
        remove_tree(&staging)?;
        {
            let key = create(CURRENT_USER, &staging)?;
            write_registration(&key, scheme, program, &staging)?;
            let marker = create(&key, MARKER)?;
            if let Owner::Other(owner) = &status.owner {
                set(&marker, "Previous", &Value::from(owner.as_str()), &staging)?;
            }
            if let Some(tree) = &replaced {
                write_tree(&create(&key, PREVIOUS)?, tree, &staging)?;
            }
        }
        remove_tree(&path)?;
        rename(&staging, scheme.as_str())?;
        self.status(scheme, program)
    }

    pub(crate) fn unregister(&self, scheme: &Scheme, program: &Path) -> Result<Status, Error> {
        let path = self.path(scheme.as_str());
        let previous = {
            let Some(key) = open(CURRENT_USER, &path)? else {
                return self.status(scheme, program);
            };
            if open(&key, MARKER)?.is_none() {
                return self.status(scheme, program);
            }
            match open(&key, PREVIOUS)? {
                Some(saved) => Some(read_tree(&saved, 0, &path)?),
                None => None,
            }
        };
        if let Some(tree) = previous {
            let staging = self.staging(scheme);
            remove_tree(&staging)?;
            write_tree(&create(CURRENT_USER, &staging)?, &tree, &staging)?;
            remove_tree(&path)?;
            rename(&staging, scheme.as_str())?;
        } else {
            remove_tree(&path)?;
        }
        self.status(scheme, program)
    }

    fn staging(&self, scheme: &Scheme) -> String {
        self.path(&format!("{scheme}.msbe-staging"))
    }

    fn machine_owner(&self, scheme: &Scheme) -> Result<Owner, Error> {
        if !self.machine {
            return Ok(Owner::Nobody);
        }
        let path = format!(r"{CLASSES}\{scheme}");
        Ok(match open(LOCAL_MACHINE, &path)? {
            Some(key) if handles_links(&key) => Owner::Other(describe(&key, &path)),
            _ => Owner::Nobody,
        })
    }
}

/// The command a registration runs: the program, `handoff`, and the link.
fn command(program: &Path) -> String {
    format!("\"{}\" handoff \"%1\"", program.display())
}

fn write_registration(key: &Key, scheme: &Scheme, program: &Path, path: &str) -> Result<(), Error> {
    set(
        key,
        "",
        &Value::from(format!("URL:{scheme} link").as_str()),
        path,
    )?;
    set(key, "URL Protocol", &Value::from(""), path)?;
    set(
        &create(key, COMMAND)?,
        "",
        &Value::from(command(program).as_str()),
        path,
    )?;
    set(
        &create(key, MARKER)?,
        "Program",
        &Value::from(program.to_string_lossy().as_ref()),
        path,
    )
}

fn handles_links(key: &Key) -> bool {
    key.get_value("URL Protocol").is_ok() || key.open(COMMAND).is_ok()
}

/// The command that opens links, or the key's description, or its path.
fn describe(key: &Key, path: &str) -> String {
    key.open(COMMAND)
        .and_then(|command| command.get_string(""))
        .or_else(|_| key.get_string(""))
        .ok()
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| path.to_owned())
}

fn open(parent: &Key, path: &str) -> Result<Option<Key>, Error> {
    match parent.open(path) {
        Ok(key) => Ok(Some(key)),
        Err(error) if error.code().0 == NOT_FOUND => Ok(None),
        Err(error) => Err(failure("open", path, &error)),
    }
}

fn create(parent: &Key, path: &str) -> Result<Key, Error> {
    parent
        .create(path)
        .map_err(|error| failure("create", path, &error))
}

fn set(key: &Key, name: &str, value: &Value, path: &str) -> Result<(), Error> {
    key.set_value(name, value)
        .map_err(|error| failure("write", path, &error))
}

/// Removes the key at `path` beneath the current user's hive, and everything beneath it.
fn remove_tree(path: &str) -> Result<(), Error> {
    match CURRENT_USER.remove_tree(path) {
        Err(error) if error.code().0 != NOT_FOUND => Err(failure("remove", path, &error)),
        _ => Ok(()),
    }
}

/// Renames the key at `path` beneath the current user's hive to `name`, keeping its parent.
fn rename(path: &str, name: &str) -> Result<(), Error> {
    CURRENT_USER
        .rename(path, name)
        .map_err(|error| failure("rename", path, &error))
}

fn read_tree(key: &Key, depth: usize, path: &str) -> Result<Tree, Error> {
    if depth > DEPTH {
        return Err(Error::Registry {
            action: "copy",
            key: path.to_owned(),
            message: "it is nested too deeply".to_owned(),
        });
    }
    let values = key
        .values()
        .map_err(|error| failure("read", path, &error))?
        .collect();
    let mut keys = Vec::new();
    for name in key.keys().map_err(|error| failure("read", path, &error))? {
        let child_path = format!(r"{path}\{name}");
        let child = key
            .open(&name)
            .map_err(|error| failure("open", &child_path, &error))?;
        keys.push((name, read_tree(&child, depth + 1, &child_path)?));
    }
    Ok(Tree { values, keys })
}

fn write_tree(key: &Key, tree: &Tree, path: &str) -> Result<(), Error> {
    for (name, value) in &tree.values {
        set(key, name, value, path)?;
    }
    for (name, child) in &tree.keys {
        let child_path = format!(r"{path}\{name}");
        write_tree(&create(key, name)?, child, &child_path)?;
    }
    Ok(())
}

fn failure(action: &'static str, key: &str, error: &impl std::fmt::Display) -> Error {
    Error::Registry {
        action,
        key: key.to_owned(),
        message: error.to_string(),
    }
}
