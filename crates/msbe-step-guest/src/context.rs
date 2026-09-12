//! What a step can read: the mod, the game, and recorded answers.

#[cfg(not(target_arch = "wasm32"))]
use std::{cell::RefCell, collections::BTreeMap};

use serde::Deserialize;

use crate::{Error, Question};

/// One file of the mod: its path inside the mod and its size.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// The file's path inside the mod.
    pub path: String,
    /// The file's size in bytes.
    pub size: u64,
}

/// The mod, game and answers a step runs against.
///
/// Inside the sandbox every call crosses to the host, which enforces the step's grant, its read
/// budget and every limit, whatever a step asks for.
#[derive(Debug, Default)]
pub struct Context {
    #[cfg(not(target_arch = "wasm32"))]
    archive: BTreeMap<String, Vec<u8>>,
    #[cfg(not(target_arch = "wasm32"))]
    game: BTreeMap<String, Vec<u8>>,
    #[cfg(not(target_arch = "wasm32"))]
    answers: BTreeMap<String, String>,
    #[cfg(not(target_arch = "wasm32"))]
    log: RefCell<Vec<String>>,
}

impl Context {
    /// The mod, game and answers the host is serving this call.
    #[cfg(target_arch = "wasm32")]
    #[doc(hidden)]
    pub const fn host() -> Self {
        Self {}
    }

    /// A mod, game files and recorded answers held in memory, for testing a step natively.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn native(
        archive: impl IntoIterator<Item = (String, Vec<u8>)>,
        game: impl IntoIterator<Item = (String, Vec<u8>)>,
        answers: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            archive: archive.into_iter().collect(),
            game: game.into_iter().collect(),
            answers: answers.into_iter().collect(),
            log: RefCell::default(),
        }
    }

    /// Every file of the mod, in lexical path order.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when the host cannot list the mod.
    pub fn entries(&self) -> Result<Vec<Entry>, Error> {
        #[cfg(target_arch = "wasm32")]
        {
            let bytes = crate::abi::entries()?;
            serde_json::from_slice(&bytes).map_err(Error::extension)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            Ok(self
                .archive
                .iter()
                .map(|(path, bytes)| Entry {
                    path: path.clone(),
                    size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                })
                .collect())
        }
    }

    /// The mod's file at `path`, failing rather than returning more than `limit` bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidArchive`] for a file the mod lacks, [`Error::Limit`] for one over
    /// the limit or the host's read budget, and [`Error::UnsafePath`] for an unsafe path.
    pub fn read(&self, path: &str, limit: u64) -> Result<Vec<u8>, Error> {
        #[cfg(target_arch = "wasm32")]
        {
            crate::abi::read(path, limit)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let bytes = self
                .archive
                .get(path)
                .ok_or_else(|| Error::invalid_archive(format!("the mod has no file {path}")))?;
            within(path, bytes, limit)
        }
    }

    /// The game file at the instance-relative `path` as it was before MSBE changed anything, or
    /// `None` when the game has no such file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Denied`] for a path the step's `game_read` globs do not cover,
    /// [`Error::Limit`] for a file over `limit` or the host's read budget, and
    /// [`Error::UnsafePath`] for an unsafe path.
    pub fn read_game(&self, path: &str, limit: u64) -> Result<Option<Vec<u8>>, Error> {
        #[cfg(target_arch = "wasm32")]
        {
            crate::abi::read_game(path, limit)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.game
                .get(path)
                .map(|bytes| within(path, bytes, limit))
                .transpose()
        }
    }

    /// Whether the game has a file at `path`, without reading its contents.
    ///
    /// # Errors
    ///
    /// As for [`Context::read_game`], except that a file's size is never a limit.
    pub fn game_has(&self, path: &str) -> Result<bool, Error> {
        match self.read_game(path, 0) {
            Ok(found) => Ok(found.is_some()),
            Err(Error::Limit { .. }) => Ok(true),
            Err(error) => Err(error),
        }
    }

    /// The answer to `question`: the one recorded for the mod, or the question's default.
    ///
    /// # Errors
    ///
    /// Returns [`Error::QuestionPending`] when there is neither, and an [`Error`] when the host
    /// refuses the question as malformed.
    pub fn ask(&self, question: &Question) -> Result<String, Error> {
        #[cfg(target_arch = "wasm32")]
        {
            let bytes = serde_json::to_vec(question).map_err(Error::extension)?;
            crate::abi::ask(&bytes, &question.id)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.answers
                .get(&question.id)
                .or(question.default.as_ref())
                .cloned()
                .ok_or_else(|| Error::QuestionPending {
                    question: question.id.clone(),
                })
        }
    }

    /// Records a diagnostic line the host may show. Lines past the host's limit are dropped.
    pub fn log(&self, message: &str) {
        #[cfg(target_arch = "wasm32")]
        {
            crate::abi::log(message);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.log.borrow_mut().push(message.to_owned());
        }
    }

    /// Every line [`Context::log`] recorded, for tests.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn logged(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn within(path: &str, bytes: &[u8], limit: u64) -> Result<Vec<u8>, Error> {
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(Error::Limit {
            message: format!("{path} exceeds the {limit}-byte read limit"),
        });
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::{Context, Error};
    use crate::{Choice, Question};

    #[test]
    fn native_context_reads_bounds_and_answers() {
        let context = Context::native(
            [("a.txt".to_owned(), b"mod".to_vec())],
            [("Data/Base.esm".to_owned(), b"game".to_vec())],
            [("colour".to_owned(), "blue".to_owned())],
        );
        assert_eq!(context.entries().unwrap().len(), 1);
        assert_eq!(context.read("a.txt", 3).unwrap(), b"mod");
        assert!(matches!(context.read("a.txt", 2), Err(Error::Limit { .. })));
        assert!(context.game_has("Data/Base.esm").unwrap());
        assert_eq!(context.read_game("Data/Other.esm", 10).unwrap(), None);

        let choices = vec![Choice::new("red", "Red"), Choice::new("blue", "Blue")];
        let asked = Question::choice("colour", "Colour", choices.clone());
        assert_eq!(context.ask(&asked).unwrap(), "blue");
        let unanswered = Question::choice("size", "Size", choices);
        assert_eq!(
            context.ask(&unanswered),
            Err(Error::QuestionPending {
                question: "size".to_owned()
            })
        );
        assert_eq!(context.ask(&unanswered.with_default("red")).unwrap(), "red");
        context.log("done");
        assert_eq!(context.logged(), ["done"]);
    }
}
