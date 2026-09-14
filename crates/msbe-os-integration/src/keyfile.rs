//! Desktop entries and MIME applications lists, edited line by line so every line MSBE does not
//! change is written back as it was read.

use std::{
    fs,
    io::{self, Read as _},
    path::Path,
};

use crate::Error;

/// The most bytes a desktop entry or list may be.
const FILE_LIMIT: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Group {
        name: String,
        raw: String,
    },
    Entry {
        key: String,
        value: String,
        raw: String,
    },
    Other(String),
}

/// A key file: groups of `key=value` entries, with comments and blank lines kept in place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct KeyFile {
    lines: Vec<Line>,
}

impl KeyFile {
    /// Reads a key file. A line that is not a group, entry, comment or blank line is kept as it is
    /// and otherwise ignored, so a file another program wrote is never refused or damaged.
    pub(crate) fn parse(text: &str) -> Self {
        let lines = text
            .lines()
            .map(|raw| {
                let trimmed = raw.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    return Line::Other(raw.to_owned());
                }
                if let Some(name) = trimmed
                    .strip_prefix('[')
                    .and_then(|rest| rest.strip_suffix(']'))
                {
                    return Line::Group {
                        name: name.to_owned(),
                        raw: raw.to_owned(),
                    };
                }
                match raw.split_once('=') {
                    Some((key, value)) if !key.trim().is_empty() => Line::Entry {
                        key: key.trim().to_owned(),
                        value: value.trim().to_owned(),
                        raw: raw.to_owned(),
                    },
                    _ => Line::Other(raw.to_owned()),
                }
            })
            .collect();
        Self { lines }
    }

    /// Reads the key file at `path`, or `None` when there is none.
    pub(crate) fn read(path: &Path) -> Result<Option<Self>, Error> {
        let failed = |source| Error::Io {
            action: "read",
            path: path.to_path_buf(),
            source,
        };
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(failed(error)),
        };
        let mut bytes = Vec::new();
        file.take(FILE_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(failed)?;
        if bytes.len() as u64 > FILE_LIMIT {
            return Err(failed(io::Error::new(
                io::ErrorKind::InvalidData,
                "it is larger than 1 MiB",
            )));
        }
        let text = String::from_utf8(bytes).map_err(|_| {
            failed(io::Error::new(
                io::ErrorKind::InvalidData,
                "it is not UTF-8",
            ))
        })?;
        Ok(Some(Self::parse(&text)))
    }

    /// The value of the first `key` in `group`.
    pub(crate) fn get(&self, group: &str, key: &str) -> Option<&str> {
        let mut inside = false;
        self.lines.iter().find_map(|line| match line {
            Line::Group { name, .. } => {
                inside = name == group;
                None
            }
            Line::Entry {
                key: found, value, ..
            } if inside && found == key => Some(value.as_str()),
            Line::Entry { .. } | Line::Other(_) => None,
        })
    }

    /// Sets `key` in `group` to `value`: the first such entry is replaced in place, or a new one
    /// follows the group's last entry. A missing group is added at the end.
    pub(crate) fn set(&mut self, group: &str, key: &str, value: &str) {
        let entry = Line::Entry {
            key: key.to_owned(),
            value: value.to_owned(),
            raw: format!("{key}={value}"),
        };
        let mut inside = false;
        let mut last = None;
        for (index, line) in self.lines.iter_mut().enumerate() {
            match line {
                Line::Group { name, .. } => {
                    inside = name == group;
                    if inside && last.is_none() {
                        last = Some(index);
                    }
                }
                Line::Entry { key: found, .. } if inside && found == key => {
                    *line = entry;
                    return;
                }
                Line::Entry { .. } if inside => last = Some(index),
                Line::Entry { .. } | Line::Other(_) => {}
            }
        }
        if let Some(index) = last {
            self.lines.insert(index + 1, entry);
            return;
        }
        if self
            .lines
            .last()
            .is_some_and(|line| !matches!(line, Line::Other(raw) if raw.trim().is_empty()))
        {
            self.lines.push(Line::Other(String::new()));
        }
        self.lines.push(Line::Group {
            name: group.to_owned(),
            raw: format!("[{group}]"),
        });
        self.lines.push(entry);
    }

    /// Removes every `key` in `group`.
    pub(crate) fn remove(&mut self, group: &str, key: &str) {
        let mut inside = false;
        self.lines.retain(|line| match line {
            Line::Group { name, .. } => {
                inside = name == group;
                true
            }
            Line::Entry { key: found, .. } => !(inside && found == key),
            Line::Other(_) => true,
        });
    }

    /// The file's text, one line per line read or set.
    pub(crate) fn render(&self) -> String {
        let mut text = String::new();
        for line in &self.lines {
            text.push_str(match line {
                Line::Group { raw, .. } | Line::Entry { raw, .. } | Line::Other(raw) => raw,
            });
            text.push('\n');
        }
        text
    }
}

/// The desktop file IDs or MIME types in a `;`-separated list value.
pub(crate) fn items(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(';')
        .map(str::trim)
        .filter(|item| !item.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{KeyFile, items};

    const LIST: &str = "# managed by hand\n[Default Applications]\ntext/plain = editor.desktop\n\n[Added Associations]\ntext/plain=editor.desktop;\nnot an entry\n";

    #[test]
    fn unchanged_lines_are_written_back_exactly() {
        let mut list = KeyFile::parse(LIST);
        assert_eq!(list.render(), LIST);
        assert_eq!(
            list.get("Default Applications", "text/plain"),
            Some("editor.desktop")
        );
        assert_eq!(list.get("Added Associations", "missing"), None);

        list.set("Default Applications", "x-scheme-handler/link", "a.desktop");
        list.set("Added Associations", "text/plain", "viewer.desktop;");
        assert_eq!(
            list.render(),
            "# managed by hand\n[Default Applications]\ntext/plain = editor.desktop\nx-scheme-handler/link=a.desktop\n\n[Added Associations]\ntext/plain=viewer.desktop;\nnot an entry\n"
        );
        list.remove("Default Applications", "x-scheme-handler/link");
        list.set("Added Associations", "text/plain", "editor.desktop;");
        assert_eq!(list.render(), LIST);
    }

    #[test]
    fn a_missing_group_is_added_after_a_blank_line() {
        let mut list = KeyFile::parse("[Added Associations]\ntext/plain=editor.desktop;\n");
        list.set("Default Applications", "x-scheme-handler/link", "a.desktop");
        assert_eq!(
            list.render(),
            "[Added Associations]\ntext/plain=editor.desktop;\n\n[Default Applications]\nx-scheme-handler/link=a.desktop\n"
        );
        let mut empty = KeyFile::default();
        empty.set("Default Applications", "x-scheme-handler/link", "a.desktop");
        assert_eq!(
            empty.render(),
            "[Default Applications]\nx-scheme-handler/link=a.desktop\n"
        );
    }

    #[test]
    fn list_values_split_on_semicolons() {
        assert_eq!(
            items(" a.desktop; ;b.desktop;").collect::<Vec<_>>(),
            ["a.desktop", "b.desktop"]
        );
    }
}
