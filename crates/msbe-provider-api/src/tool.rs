//! External tools: the `tool-v1` runtime's closed vocabulary, and the seam through which it runs a
//! tool the user installed and registered (`docs/06-providers-and-policy.md` §6.5).
//!
//! A program names a tool's arguments and where its output lands, as literal tokens and whole-token
//! slots from a closed set: `{output}`, `{game}` and `{item}`. No argument is assembled from text,
//! and nothing passes through a shell. The program executes nothing: the runtime builds a
//! [`ToolInvocation`], and the [`ToolHost`] its caller supplies runs it. This crate never starts a
//! process.

use std::{
    fs::File,
    io::{self, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

use msbe_fsops::RelPath;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{acquisition::AcquiredArtifact, hashing::HashingWriter};

/// The longest a tool may run.
pub const TIMEOUT_LIMIT: u64 = 86_400;
/// The most arguments a tool takes.
const ARGUMENT_LIMIT: usize = 64;
/// The most output path segments.
const SEGMENT_LIMIT: usize = 16;
/// The longest literal token.
const TOKEN_LIMIT: usize = 256;
/// The longest game or item identifier passed to a tool.
const VALUE_LIMIT: usize = 128;

/// `[tool]`: how the `tool-v1` runtime runs a registered tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolProgram {
    /// The tool's arguments: literal tokens, or a whole-token `{output}`, `{game}` or `{item}`.
    pub arguments: Vec<String>,
    /// Where the fetched item lands beneath `{output}`, as path segments that may be `{game}` or
    /// `{item}`.
    #[serde(default)]
    pub output: Vec<String>,
    /// How many seconds the tool may run before it is stopped.
    pub timeout: u64,
}

/// A whole-token slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Output,
    Game,
    Item,
}

enum Token<'a> {
    Literal(&'a str),
    Slot(Slot),
}

fn token(raw: &str) -> Result<Token<'_>, ToolProgramError> {
    match raw {
        "{output}" => Ok(Token::Slot(Slot::Output)),
        "{game}" => Ok(Token::Slot(Slot::Game)),
        "{item}" => Ok(Token::Slot(Slot::Item)),
        _ if raw.contains(['{', '}']) => Err(ToolProgramError::UnknownSlot(raw.to_owned())),
        _ if raw.is_empty() || raw.len() > TOKEN_LIMIT || raw.chars().any(char::is_control) => {
            Err(ToolProgramError::InvalidToken(raw.to_owned()))
        }
        _ => Ok(Token::Literal(raw)),
    }
}

impl ToolProgram {
    /// Checks the arguments, output path and timeout.
    ///
    /// # Errors
    ///
    /// Returns [`ToolProgramError`] for an unknown slot, a slot inside a literal, an invalid token
    /// or segment, `{output}` not given exactly once, `{item}` not given, `{output}` in the output
    /// path, or a timeout out of range.
    pub fn validate(&self) -> Result<(), ToolProgramError> {
        if self.arguments.len() > ARGUMENT_LIMIT {
            return Err(ToolProgramError::TooManyArguments);
        }
        let mut outputs = 0;
        let mut items = 0;
        for argument in &self.arguments {
            match token(argument)? {
                Token::Slot(Slot::Output) => outputs += 1,
                Token::Slot(Slot::Item) => items += 1,
                Token::Slot(Slot::Game) | Token::Literal(_) => {}
            }
        }
        if outputs != 1 || items == 0 {
            return Err(ToolProgramError::MissingSlot);
        }
        if self.output.len() > SEGMENT_LIMIT {
            return Err(ToolProgramError::InvalidSegment(self.output.join("/")));
        }
        for segment in &self.output {
            match token(segment)? {
                Token::Slot(Slot::Output) => return Err(ToolProgramError::OutputInOutput),
                Token::Slot(Slot::Game | Slot::Item) => {}
                Token::Literal(literal) if is_segment(literal) => {}
                Token::Literal(literal) => {
                    return Err(ToolProgramError::InvalidSegment(literal.to_owned()));
                }
            }
        }
        if self.timeout == 0 || self.timeout > TIMEOUT_LIMIT {
            return Err(ToolProgramError::InvalidTimeout);
        }
        Ok(())
    }

    /// The invocation that fetches `item` of `game` with `tool`'s registered program. Both
    /// identifiers must pass [`is_tool_value`]; callers check them first.
    pub fn invocation(&self, tool: &str, game: &str, item: &str) -> ToolInvocation {
        let fill = |raw: &str| match token(raw) {
            Ok(Token::Slot(Slot::Game)) => game.to_owned(),
            Ok(Token::Slot(Slot::Item)) => item.to_owned(),
            _ => raw.to_owned(),
        };
        ToolInvocation {
            tool: tool.to_owned(),
            arguments: self
                .arguments
                .iter()
                .map(|argument| match token(argument) {
                    Ok(Token::Slot(Slot::Output)) => ToolArgument::Output,
                    _ => ToolArgument::Literal(fill(argument)),
                })
                .collect(),
            output: self.output.iter().map(|segment| fill(segment)).collect(),
            timeout: Duration::from_secs(self.timeout),
        }
    }
}

/// Whether `raw` may fill `{game}` or `{item}`: letters, digits, `-`, `_`, `.` and `+`, starting with
/// a letter or digit, so it can be neither read as an option nor escape a directory.
pub fn is_tool_value(raw: &str) -> bool {
    raw.len() <= VALUE_LIMIT
        && raw
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
        && raw.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '+')
        })
}

/// Whether `raw` is one safe path segment.
fn is_segment(raw: &str) -> bool {
    RelPath::new(raw).is_ok_and(|path| !path.as_str().contains('/'))
}

/// Why a `tool-v1` program was refused.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ToolProgramError {
    /// A `tool-v1` program has no `[tool]`.
    #[error("a tool-v1 program needs [tool]")]
    MissingTool,
    /// `[tool]` is declared on another runtime.
    #[error("[tool] is only for tool-v1 programs")]
    ToolOnOtherRuntime,
    /// `external_tool` acquisition is declared on another runtime.
    #[error("external_tool acquisition is only for tool-v1 programs")]
    ExternalToolOnOtherRuntime,
    /// A `tool-v1` program acquires some other way.
    #[error("a tool-v1 program acquires through external_tool")]
    NotExternalTool,
    /// A `tool-v1` program declares something other than games, `[provider]` and `[tool]`.
    #[error("a tool-v1 program declares only games, [provider] and [tool]")]
    UnexpectedSection,
    /// A `tool-v1` program requires signing in.
    #[error(
        "a tool-v1 program cannot require signing in: the tool signs in through its own flow, and MSBE never holds its credentials"
    )]
    RequiresAuth,
    /// A `tool-v1` program does not require acknowledging its terms.
    #[error("a tool-v1 program must require acknowledging its terms")]
    NoAcknowledgement,
    /// A game identifier cannot be passed to a tool.
    #[error("game identifier {0:?} cannot be passed to a tool")]
    InvalidGame(String),
    /// A token names a slot that does not exist, or puts a slot inside a literal.
    #[error(
        "tool token {0:?} is neither a literal nor exactly one of {{output}}, {{game}} or {{item}}"
    )]
    UnknownSlot(String),
    /// A literal token is empty, too long, or has a control character.
    #[error("tool token {0:?} is empty, longer than 256 bytes, or has a control character")]
    InvalidToken(String),
    /// The arguments do not give `{output}` exactly once, or never give `{item}`.
    #[error("tool arguments need {{output}} exactly once and {{item}} at least once")]
    MissingSlot,
    /// There are more than 64 arguments.
    #[error("a tool takes at most 64 arguments")]
    TooManyArguments,
    /// An output segment is not one safe path segment, or there are more than 16.
    #[error("tool output segment {0:?} is not a single safe path segment")]
    InvalidSegment(String),
    /// The output path contains `{output}`.
    #[error("the tool's output path cannot contain {{output}}")]
    OutputInOutput,
    /// The timeout is zero or longer than a day.
    #[error("a tool's timeout is between 1 and 86400 seconds")]
    InvalidTimeout,
}

/// One run of a registered tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvocation {
    /// The provider whose registered program runs.
    pub tool: String,
    /// The arguments, in order.
    pub arguments: Vec<ToolArgument>,
    /// Where the fetched item lands, as path segments beneath the output directory.
    pub output: Vec<String>,
    /// How long the tool may run.
    pub timeout: Duration,
}

/// One argument of a tool run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolArgument {
    /// Passed as it is.
    Literal(String),
    /// The fresh, empty directory the host creates for the run.
    Output,
}

/// Runs registered tools for the `tool-v1` runtime.
pub trait ToolHost {
    /// Runs `invocation` with a fresh, empty directory created beneath `dir` in place of its
    /// `{output}` argument, and returns that directory once the tool exits successfully. What the
    /// tool prints is never parsed.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when no program is registered for the tool, the registered program
    /// changed, or the run fails, times out or is cancelled.
    fn run(&self, invocation: &ToolInvocation, dir: &Path) -> Result<PathBuf, ToolError>;
}

/// A host that runs no tool, for callers outside the download queue.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTools;

impl ToolHost for NoTools {
    fn run(&self, invocation: &ToolInvocation, _dir: &Path) -> Result<PathBuf, ToolError> {
        Err(ToolError::NoHost {
            tool: invocation.tool.clone(),
        })
    }
}

/// Why a tool did not fetch an item. Every failure the tool itself causes tells the user to complete
/// the tool's own sign-in, or to import the content themselves.
#[derive(Debug, Error)]
pub enum ToolError {
    /// Tools run only through the download queue.
    #[error("{tool} runs only through the download queue; queue it with msbe download add")]
    NoHost {
        /// The provider.
        tool: String,
    },
    /// No program is registered for the tool.
    #[error(
        "no program is registered for {tool}; register the one you installed with msbe tool register"
    )]
    NotRegistered {
        /// The provider.
        tool: String,
    },
    /// The registered program is not the one registered.
    #[error(
        "the program registered for {tool} changed after it was registered (SHA-256 {registered}, now {found}); register it again to run it"
    )]
    Changed {
        /// The provider.
        tool: String,
        /// The SHA-256 recorded when it was registered.
        registered: String,
        /// Its SHA-256 now.
        found: String,
    },
    /// The tool did not exit successfully.
    #[error(
        "{tool} is unavailable: it exited with {status}. Complete its own sign-in, or add content you obtained yourself.{output}"
    )]
    Failed {
        /// The provider.
        tool: String,
        /// How it exited.
        status: String,
        /// The end of what it printed, redacted, as a line of its own; empty when it printed nothing.
        output: String,
    },
    /// The tool exited successfully but left nothing where the program says the item lands.
    #[error(
        "{tool} is unavailable: it left nothing in {path}. Complete its own sign-in, or add content you obtained yourself."
    )]
    Empty {
        /// The provider.
        tool: String,
        /// Where the item should have landed.
        path: String,
    },
    /// The tool ran past its timeout and was stopped.
    #[error("{tool} did not finish within {seconds} seconds, and was stopped")]
    TimedOut {
        /// The provider.
        tool: String,
        /// The timeout.
        seconds: u64,
    },
    /// The download was cancelled while the tool ran, and the tool was stopped.
    #[error("{tool} was stopped because its download was cancelled")]
    Cancelled {
        /// The provider.
        tool: String,
    },
    /// The host could not run the tool.
    #[error("cannot run {tool}: {message}")]
    Host {
        /// The provider.
        tool: String,
        /// What went wrong.
        message: String,
    },
}

/// Describes `directory`, which a tool filled, as an acquired artifact: its total size, and digests
/// of a manifest listing each file's SHA-256 and path relative to `directory`, in path order. The
/// same content therefore always has the same digests, however the tool wrote it.
///
/// # Errors
///
/// Returns an error when an entry cannot be read, or is a symbolic link or has a name that is not
/// UTF-8.
pub fn acquired_directory(directory: &Path) -> io::Result<AcquiredArtifact> {
    let mut files = Vec::new();
    let mut pending = vec![(directory.to_path_buf(), String::new())];
    while let Some((current, prefix)) = pending.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let name = entry.file_name().into_string().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "an entry's name is not UTF-8")
            })?;
            let relative = format!("{prefix}{name}");
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{relative} is a symbolic link"),
                ));
            }
            if kind.is_dir() {
                pending.push((entry.path(), format!("{relative}/")));
            } else {
                files.push((relative, entry.path()));
            }
        }
    }
    files.sort();
    let mut manifest = HashingWriter::new(io::sink());
    let mut size = 0_u64;
    for (relative, path) in files {
        let mut file = HashingWriter::new(io::sink());
        io::copy(&mut File::open(path)?, &mut file)?;
        size = size.saturating_add(file.written());
        writeln!(manifest, "{}  {relative}", file.sha256_hex())?;
    }
    Ok(AcquiredArtifact {
        path: directory.to_path_buf(),
        size,
        md5: manifest.md5_hex(),
        sha1: manifest.sha1_hex(),
        sha256: manifest.sha256_hex(),
        sha512: manifest.sha512_hex(),
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{
        NoTools, ToolArgument, ToolError, ToolHost, ToolProgramError, acquired_directory,
        is_tool_value,
    };
    use crate::{ProgramError, ProviderProgram};

    const PROGRAM: &str = r#"
runtime = "tool-v1"
capabilities = []

[games]
example = "123456"

[provider]
schema = 1
id = "example-tool"
name = "Example tool"
[provider.source]
type = "prefixed"
prefix = "example-tool:"
[provider.acquisition]
type = "external_tool"
[provider.policy]
requires_auth = false
respects_distribution_flag = false
tos_url = "https://www.example.test/terms"
ack_required = true

[tool]
arguments = ["fetch", "--game", "{game}", "--item", "{item}", "--into", "{output}"]
output = ["content", "{game}", "{item}"]
timeout = 1800
"#;

    fn validated(document: &str) -> Result<ProviderProgram, ProgramError> {
        let program: ProviderProgram = toml::from_str(document).unwrap();
        program.validate().map(|()| program)
    }

    #[test]
    fn a_tool_program_becomes_an_invocation_of_literal_and_whole_token_arguments() {
        let program = validated(PROGRAM).unwrap();
        let invocation = program
            .tool
            .as_ref()
            .unwrap()
            .invocation("example-tool", "123456", "987");
        assert_eq!(
            invocation.arguments,
            [
                ToolArgument::Literal("fetch".to_owned()),
                ToolArgument::Literal("--game".to_owned()),
                ToolArgument::Literal("123456".to_owned()),
                ToolArgument::Literal("--item".to_owned()),
                ToolArgument::Literal("987".to_owned()),
                ToolArgument::Literal("--into".to_owned()),
                ToolArgument::Output,
            ]
        );
        assert_eq!(invocation.output, ["content", "123456", "987"]);
        assert_eq!(invocation.timeout.as_secs(), 1800);
    }

    #[test]
    fn tool_programs_are_refused_for_each_rule_they_break() {
        let refused = |from: &str, to: &str, expected: ProgramError| {
            let document = PROGRAM.replacen(from, to, 1);
            assert_ne!(document, PROGRAM, "{from}");
            let error = validated(&document).unwrap_err();
            assert_eq!(error.to_string(), expected.to_string(), "{from} -> {to}");
        };
        let tool = |error| ProgramError::Tool(error);
        refused(
            "\"{game}\", \"--item\"",
            "\"--game={game}\", \"--item\"",
            tool(ToolProgramError::UnknownSlot("--game={game}".to_owned())),
        );
        refused(
            "\"{output}\"]",
            "\"{home}\"]",
            tool(ToolProgramError::UnknownSlot("{home}".to_owned())),
        );
        refused(
            "\"--into\", \"{output}\"",
            "\"--into\", \"{output}\", \"{output}\"",
            tool(ToolProgramError::MissingSlot),
        );
        refused(
            "\"--item\", \"{item}\"",
            "\"--item\", \"all\"",
            tool(ToolProgramError::MissingSlot),
        );
        refused(
            "output = [\"content\"",
            "output = [\"..\"",
            tool(ToolProgramError::InvalidSegment("..".to_owned())),
        );
        refused(
            "\"content\", \"{game}\"",
            "\"{output}\", \"{game}\"",
            tool(ToolProgramError::OutputInOutput),
        );
        refused(
            "timeout = 1800",
            "timeout = 0",
            tool(ToolProgramError::InvalidTimeout),
        );
        refused(
            "requires_auth = false",
            "requires_auth = true",
            tool(ToolProgramError::RequiresAuth),
        );
        refused(
            "ack_required = true",
            "ack_required = false",
            tool(ToolProgramError::NoAcknowledgement),
        );
        refused(
            "capabilities = []",
            "capabilities = [\"search\"]",
            tool(ToolProgramError::UnexpectedSection),
        );
        refused(
            "example = \"123456\"",
            "example = \"-rf\"",
            ProgramError::InvalidGame("example".to_owned()),
        );
        refused(
            "type = \"external_tool\"",
            "type = \"user_action\"",
            tool(ToolProgramError::NotExternalTool),
        );
        refused(
            "runtime = \"tool-v1\"",
            "runtime = \"direct-url-v1\"",
            tool(ToolProgramError::ToolOnOtherRuntime),
        );
        let without_tool = PROGRAM.split("[tool]").next().unwrap();
        assert_eq!(
            validated(without_tool).unwrap_err().to_string(),
            tool(ToolProgramError::MissingTool).to_string()
        );
    }

    #[test]
    fn tool_values_can_be_neither_options_nor_paths() {
        for accepted in ["123456", "item.v2", "a+b_c-d"] {
            assert!(is_tool_value(accepted), "{accepted}");
        }
        for refused in ["", "-rf", "--help", "..", ".hidden", "a/b", "a b", "a\\b"] {
            assert!(!is_tool_value(refused), "{refused:?}");
        }
    }

    #[test]
    fn a_directory_digest_depends_on_its_content_and_paths_alone() {
        let write = |root: &Path, files: &[(&str, &str)]| {
            for (path, content) in files {
                let path = root.join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, content).unwrap();
            }
        };
        let first = tempfile::tempdir().unwrap();
        write(first.path(), &[("b/two.txt", "two"), ("one.txt", "one")]);
        let second = tempfile::tempdir().unwrap();
        write(second.path(), &[("one.txt", "one"), ("b/two.txt", "two")]);
        let (first, second) = (
            acquired_directory(first.path()).unwrap(),
            acquired_directory(second.path()).unwrap(),
        );
        assert_eq!(first.sha256, second.sha256);
        assert_eq!(first.sha512, second.sha512);
        assert_eq!(first.size, 6);

        let renamed = tempfile::tempdir().unwrap();
        write(
            renamed.path(),
            &[("one.txt", "one"), ("b/three.txt", "two")],
        );
        assert_ne!(
            acquired_directory(renamed.path()).unwrap().sha256,
            first.sha256
        );
    }

    #[test]
    fn without_a_host_no_tool_runs() {
        let program = validated(PROGRAM).unwrap();
        let invocation = program
            .tool
            .as_ref()
            .unwrap()
            .invocation("example-tool", "123456", "987");
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            NoTools.run(&invocation, dir.path()),
            Err(ToolError::NoHost { tool }) if tool == "example-tool"
        ));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
