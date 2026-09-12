//! Sandboxed WebAssembly host for plan step extensions, ABI `msbe-plan-step-1`.
//!
//! A `run-extension` step hands one mod to a WebAssembly module and gets operations back. The
//! module has no filesystem, network, process, clock or randomness. The host links only the imports
//! its plan declaration grants, so a module that asks for anything else fails to load: an ungranted
//! capability does not exist in its world. Every operation it returns is checked against the
//! declaration before core turns it into a deployment claim, and fuel, a memory ceiling and a read
//! budget bound every run deterministically.
//!
//! This crate knows nothing about instances, stores or games. Core serves it a mod through
//! [`Archive`] and the installation through [`Game`], and records what it returns.
//!
//! See `docs/18-wasm-extensions.md` and `docs/02-plan-system.md` §2.5.

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use msbe_plan_schema::{EmitKind, ExtensionCapability, is_relative_path, matches_glob};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use wasmtime::{
    Caller, Config, Engine, ExternType, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
    Trap,
};

/// The ABI this host implements.
pub const ABI: &str = "msbe-plan-step-1";

/// The version a module's `msbe_abi_version` export must return.
pub const ABI_VERSION: i32 = 1;

const NOT_FOUND: i64 = -1;
const LIMIT: i64 = -2;
const UNSAFE_PATH: i64 = -3;
const BUDGET: i64 = -4;
const QUESTION_PENDING: i64 = -5;
const DENIED: i64 = -6;
const INVALID: i64 = -7;

/// The most bytes one path, question or log line may be.
const MESSAGE_LIMIT: usize = 64 << 10;
/// The most log lines one run keeps.
const LOG_LINES: usize = 256;
/// The longest prompt or free-text answer.
const TEXT_LIMIT: usize = 4096;

/// An import's module and name, and the capability that links it. `None` is always linked.
type Import = (&'static str, &'static str, Option<ExtensionCapability>);

/// Every import the host can link.
const IMPORTS: [Import; 6] = [
    ("msbe_host", "take", None),
    ("msbe_host", "log", None),
    (
        "msbe_archive",
        "entries",
        Some(ExtensionCapability::ArchiveRead),
    ),
    (
        "msbe_archive",
        "read",
        Some(ExtensionCapability::ArchiveRead),
    ),
    ("msbe_game", "read", Some(ExtensionCapability::GameRead)),
    ("msbe_ui", "ask", Some(ExtensionCapability::UiPrompt)),
];

/// The exports every module must provide.
const EXPORTS: [&str; 4] = ["memory", "msbe_abi_version", "msbe_alloc", "msbe_run"];

/// Resource ceilings for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Fuel, the deterministic stand-in for a timeout: an extension that exhausts it traps.
    pub fuel: u64,
    /// Linear memory the module may grow to, in bytes.
    pub memory: usize,
    /// Bytes the module may read from the mod and the game in one run.
    pub read: u64,
    /// Operations one run may return.
    pub operations: usize,
    /// Bytes of generated text one run may write.
    pub write: usize,
    /// Distinct unanswered questions one run may ask.
    pub questions: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel: 2_000_000_000,
            memory: 256 << 20,
            read: 1 << 30,
            operations: 100_000,
            write: 16 << 20,
            questions: 256,
        }
    }
}

/// What a module may do, taken from its plan declaration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Grant {
    capabilities: BTreeSet<ExtensionCapability>,
    game_read: Vec<String>,
    emit: BTreeSet<EmitKind>,
}

impl Grant {
    /// A grant of `capabilities`, reading game files that match `game_read`, and returning `emit`
    /// operations. The `game_read` globs must already have their placeholders filled in.
    pub fn new(
        capabilities: impl IntoIterator<Item = ExtensionCapability>,
        game_read: impl IntoIterator<Item = String>,
        emit: impl IntoIterator<Item = EmitKind>,
    ) -> Self {
        Self {
            capabilities: capabilities.into_iter().collect(),
            game_read: game_read.into_iter().collect(),
            emit: emit.into_iter().collect(),
        }
    }

    fn allows(&self, capability: Option<ExtensionCapability>) -> bool {
        capability.is_none_or(|capability| self.capabilities.contains(&capability))
    }
}

/// One file of a mod: its path inside the mod and its size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// The file's path inside the mod.
    pub path: String,
    /// The file's size in bytes.
    pub size: u64,
}

/// The files of the mod a step runs over.
pub trait Archive {
    /// Every file, in lexical path order.
    fn entries(&self) -> Vec<Entry>;

    /// The bytes of the file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when the file is absent or cannot be read.
    fn read(&self, path: &str) -> Result<Vec<u8>, ReadError>;
}

/// A game file an extension may read: its size, and an identity for its contents that core records
/// as a derivation input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameFile {
    /// The file's size in bytes.
    pub size: u64,
    /// A stable identity for the file's contents, such as its digest.
    pub identity: String,
}

/// The installation, as it was before MSBE changed anything.
pub trait Game {
    /// The file at the instance-relative `path`, or `None` when the game has none.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when the file cannot be inspected.
    fn stat(&self, path: &str) -> Result<Option<GameFile>, ReadError>;

    /// The bytes of the file at `path`, which [`Game::stat`] found.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when the file is absent or cannot be read.
    fn read(&self, path: &str) -> Result<Vec<u8>, ReadError>;
}

/// Why serving an extension a file failed.
#[derive(Debug, Error)]
pub enum ReadError {
    /// The file does not exist.
    #[error("the file does not exist")]
    NotFound,
    /// The file could not be read.
    #[error("{0}")]
    Failed(String),
}

/// An installer question, as an extension asked it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    /// Stable identifier the answer is recorded under.
    pub id: String,
    /// What the user is asked.
    pub prompt: String,
    /// The kind of answer expected.
    pub kind: QuestionKind,
    /// The choices, for choice and multi-choice questions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<Choice>,
    /// The answer used when none is recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

impl Question {
    /// Whether `answer` is valid: a choice's identifier, distinct comma-separated choices, `true`
    /// or `false`, or text without control characters, according to the question's kind.
    pub fn accepts(&self, answer: &str) -> bool {
        let is_choice = |id: &str| self.choices.iter().any(|choice| choice.id == id);
        match self.kind {
            QuestionKind::Choice => is_choice(answer),
            QuestionKind::Multi => {
                let mut seen = BTreeSet::new();
                answer.is_empty() || answer.split(',').all(|id| is_choice(id) && seen.insert(id))
            }
            QuestionKind::Boolean => matches!(answer, "true" | "false"),
            QuestionKind::Text => {
                answer.len() <= TEXT_LIMIT && !answer.chars().any(char::is_control)
            }
        }
    }

    fn is_valid(&self) -> bool {
        let offers_choices = matches!(self.kind, QuestionKind::Choice | QuestionKind::Multi);
        let mut ids = BTreeSet::new();
        is_identifier(&self.id)
            && !self.prompt.trim().is_empty()
            && self.prompt.len() <= TEXT_LIMIT
            && offers_choices != self.choices.is_empty()
            && self
                .choices
                .iter()
                .all(|choice| is_identifier(&choice.id) && ids.insert(choice.id.as_str()))
            && self
                .default
                .as_deref()
                .is_none_or(|default| self.accepts(default))
    }
}

impl fmt::Display for Question {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.id, self.prompt)?;
        let choices = self
            .choices
            .iter()
            .map(|choice| choice.id.as_str())
            .collect::<Vec<_>>()
            .join("|");
        match self.kind {
            QuestionKind::Choice => write!(formatter, ": one of {choices}"),
            QuestionKind::Multi => write!(formatter, ": comma-separated from {choices}"),
            QuestionKind::Boolean => formatter.write_str(": true or false"),
            QuestionKind::Text => formatter.write_str(": text"),
        }
    }
}

/// The kind of answer a [`Question`] expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuestionKind {
    /// Exactly one choice.
    Choice,
    /// Any number of choices.
    Multi,
    /// `true` or `false`.
    Boolean,
    /// Free text.
    Text,
}

/// One answer a choice question offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    /// Stable identifier recorded as the answer.
    pub id: String,
    /// What the user sees.
    pub label: String,
}

/// One run of a step over one mod.
#[derive(Debug, Clone, Copy)]
pub struct Invocation<'a> {
    /// The plan step's identifier.
    pub step: &'a str,
    /// The mod's name in the profile.
    pub module: &'a str,
    /// The selected loader.
    pub loader: &'a str,
    /// The instance's game version, when it has one.
    pub game_version: Option<&'a str>,
    /// The step's parameters, with placeholders filled in.
    pub parameters: &'a BTreeMap<String, String>,
    /// Instance-relative directories operations may write beneath.
    pub roots: &'a [String],
    /// The mod's recorded answers for this step, by question.
    pub answers: &'a BTreeMap<String, String>,
}

/// An operation an extension returned, checked against its declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOperation {
    /// Places the mod's file at `source` at the instance-relative `path`.
    Place {
        /// The file's path inside the mod.
        source: String,
        /// The instance-relative destination.
        path: String,
    },
    /// Writes generated bytes to the instance-relative `path`.
    WriteFile {
        /// The instance-relative destination.
        path: String,
        /// The file's contents.
        contents: Vec<u8>,
    },
}

/// What one run produced, and everything it depended on beyond the mod's own files.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outcome {
    /// Checked operations, in the order returned.
    pub operations: Vec<StepOperation>,
    /// Game files the extension looked at, with each one's identity, or `None` for an absent one.
    pub game_inputs: BTreeMap<String, Option<String>>,
    /// Answers the extension consulted, including defaults, by question.
    pub answers: BTreeMap<String, String>,
    /// Diagnostic lines the extension logged.
    pub log: Vec<String>,
}

/// Why an extension could not be loaded or run, or why its result was refused.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HostError {
    /// The module is not valid WebAssembly, or the host could not prepare it.
    #[error("the extension module is invalid: {0}")]
    InvalidModule(String),
    /// The module imports something the host never provides.
    #[error("the extension imports {module}.{name}, which the host does not provide")]
    UnknownImport {
        /// The import module.
        module: String,
        /// The import name.
        name: String,
    },
    /// The module imports a capability its declaration does not grant.
    #[error(
        "the extension imports {module}.{name}, which needs the {capability} capability it was not granted"
    )]
    UngrantedImport {
        /// The import module.
        module: String,
        /// The import name.
        name: String,
        /// The capability the import belongs to.
        capability: &'static str,
    },
    /// The module lacks a required export.
    #[error("the extension does not export {0}")]
    MissingExport(&'static str),
    /// The module implements another ABI version.
    #[error("the extension implements step ABI version {0}, not {ABI_VERSION}")]
    AbiVersion(i32),
    /// The run exhausted its fuel.
    #[error("the extension ran out of fuel")]
    OutOfFuel,
    /// The module trapped.
    #[error("the extension trapped: {0}")]
    Trap(String),
    /// The module's response broke the ABI.
    #[error("the extension returned an invalid response: {0}")]
    InvalidResponse(String),
    /// The extension reported a failure.
    #[error("the extension failed ({kind}): {message}")]
    Extension {
        /// The failure kind the extension reported.
        kind: String,
        /// Its message.
        message: String,
    },
    /// The extension asked questions that have neither a recorded answer nor a default.
    #[error("installer questions need answers: {}", describe(.0))]
    QuestionsRequired(Vec<Question>),
    /// A recorded answer is not valid for the question asked.
    #[error("the recorded answer {answer:?} is not valid for question {question}")]
    InvalidAnswer {
        /// The question.
        question: String,
        /// The recorded answer.
        answer: String,
    },
    /// The extension returned an operation kind its declaration does not allow.
    #[error("the extension returned a {0} operation, which its declaration does not allow")]
    UndeclaredOperation(&'static str),
    /// An operation's path is not a safe relative path.
    #[error("the extension returned the unsafe path {0:?}")]
    UnsafePath(String),
    /// An operation writes outside the step's roots.
    #[error("the extension may not write {0:?}: it is outside the step's roots")]
    OutsideRoots(String),
    /// A place operation names a file the mod does not have.
    #[error("the extension placed {0:?}, which is not a file of the mod")]
    UnknownSource(String),
    /// Two operations write the same path.
    #[error("the extension wrote {0:?} more than once")]
    DuplicatePath(String),
    /// A run exceeded one of its limits.
    #[error("the extension exceeded its {0} limit")]
    Limit(&'static str),
}

/// A compiled step extension, checked against its grant and ready to run over mods.
#[derive(Debug)]
pub struct StepExtension {
    engine: Engine,
    module: Module,
    grant: Grant,
    limits: Limits,
}

impl StepExtension {
    /// Compiles `bytes` and checks that the module asks only for what `grant` allows, exports the
    /// step ABI, and implements its version.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::UngrantedImport`] or [`HostError::UnknownImport`] for an import outside
    /// the grant, [`HostError::MissingExport`] or [`HostError::AbiVersion`] for a module that does
    /// not implement the ABI, or [`HostError::InvalidModule`] for invalid WebAssembly.
    pub fn load(bytes: &[u8], grant: Grant, limits: Limits) -> Result<Self, HostError> {
        let engine = engine()?;
        let module = Module::new(&engine, bytes).map_err(invalid_module)?;
        validate_imports(&module, &grant)?;
        if let Some(missing) = EXPORTS
            .into_iter()
            .find(|name| module.get_export(name).is_none())
        {
            return Err(HostError::MissingExport(missing));
        }
        let extension = Self {
            engine,
            module,
            grant,
            limits,
        };
        let mut store = extension.store(State::new(
            &limits,
            Box::new(Nothing),
            Box::new(Nothing),
            Vec::new(),
            BTreeMap::new(),
        ))?;
        let instance = extension.instantiate(&mut store)?;
        let version = instance
            .get_typed_func::<(), i32>(&mut store, "msbe_abi_version")
            .map_err(invalid_module)?
            .call(&mut store, ())
            .map_err(|error| trap(&error))?;
        if version != ABI_VERSION {
            return Err(HostError::AbiVersion(version));
        }
        Ok(extension)
    }

    /// Runs the extension over one mod in a fresh instance.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::QuestionsRequired`] when the extension asked questions with neither a
    /// recorded answer nor a default, [`HostError::InvalidAnswer`] for a recorded answer the
    /// question refuses, a limit, trap or response error, or an error for any returned operation
    /// its declaration does not allow.
    pub fn run(
        &self,
        invocation: &Invocation<'_>,
        archive: Box<dyn Archive>,
        game: Box<dyn Game>,
    ) -> Result<Outcome, HostError> {
        let state = State::new(
            &self.limits,
            archive,
            game,
            self.grant.game_read.clone(),
            invocation.answers.clone(),
        );
        let mut store = self.store(state)?;
        let instance = self.instantiate(&mut store)?;
        let request = serde_json::to_vec(&json!({
            "step": invocation.step,
            "module": invocation.module,
            "loader": invocation.loader,
            "game_version": invocation.game_version,
            "parameters": invocation.parameters,
            "roots": invocation.roots,
        }))
        .map_err(invalid_module)?;
        let response = call(
            &mut store,
            &instance,
            &request,
            self.limits.write.saturating_add(MESSAGE_LIMIT << 8),
        );
        let state = store.into_data();
        if let Some((question, answer)) = state.invalid_answer {
            return Err(HostError::InvalidAnswer { question, answer });
        }
        if !state.pending.is_empty() {
            return Err(HostError::QuestionsRequired(
                state.pending.into_values().collect(),
            ));
        }
        let operations = self.check(response?, &state.entries, invocation.roots)?;
        Ok(Outcome {
            operations,
            game_inputs: state.game_inputs,
            answers: state.consulted,
            log: state.log,
        })
    }

    fn store(&self, state: State) -> Result<Store<State>, HostError> {
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        store.set_fuel(self.limits.fuel).map_err(invalid_module)?;
        Ok(store)
    }

    fn instantiate(&self, store: &mut Store<State>) -> Result<wasmtime::Instance, HostError> {
        let mut linker = Linker::new(&self.engine);
        link_host(&mut linker)?;
        if self.grant.allows(Some(ExtensionCapability::ArchiveRead)) {
            link_archive(&mut linker)?;
        }
        if self.grant.allows(Some(ExtensionCapability::GameRead)) {
            link_game(&mut linker)?;
        }
        if self.grant.allows(Some(ExtensionCapability::UiPrompt)) {
            link_ui(&mut linker)?;
        }
        linker
            .instantiate(store, &self.module)
            .map_err(|error| trap(&error))
    }

    /// Checks every returned operation against the grant, the mod and the step's roots.
    fn check(
        &self,
        response: Value,
        entries: &BTreeMap<String, u64>,
        roots: &[String],
    ) -> Result<Vec<StepOperation>, HostError> {
        let response: Response = serde_json::from_value(response)
            .map_err(|error| HostError::InvalidResponse(error.to_string()))?;
        if response.operations.len() > self.limits.operations {
            return Err(HostError::Limit("operations"));
        }
        let mut paths = BTreeSet::new();
        let mut written = 0_usize;
        let mut checked = Vec::with_capacity(response.operations.len());
        for operation in response.operations {
            let (kind, path) = match &operation {
                Returned::Place { path, .. } => (EmitKind::Place, path),
                Returned::WriteFile { path, .. } => (EmitKind::WriteFile, path),
            };
            if !self.grant.emit.contains(&kind) {
                return Err(HostError::UndeclaredOperation(kind.as_str()));
            }
            if !is_relative_path(path) {
                return Err(HostError::UnsafePath(path.clone()));
            }
            if !roots.iter().any(|root| {
                path.strip_prefix(root.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
            }) {
                return Err(HostError::OutsideRoots(path.clone()));
            }
            if !paths.insert(path.clone()) {
                return Err(HostError::DuplicatePath(path.clone()));
            }
            checked.push(match operation {
                Returned::Place { source, path } => {
                    if !entries.contains_key(&source) {
                        return Err(HostError::UnknownSource(source));
                    }
                    StepOperation::Place { source, path }
                }
                Returned::WriteFile { path, text } => {
                    written = written.saturating_add(text.len());
                    if written > self.limits.write {
                        return Err(HostError::Limit("write"));
                    }
                    StepOperation::WriteFile {
                        path,
                        contents: text.into_bytes(),
                    }
                }
            });
        }
        Ok(checked)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    operations: Vec<Returned>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum Returned {
    Place { source: String, path: String },
    WriteFile { path: String, text: String },
}

/// Everything one store holds for the host functions of one run.
struct State {
    limits: StoreLimits,
    staged: Vec<u8>,
    read_left: u64,
    archive: Box<dyn Archive>,
    entries: BTreeMap<String, u64>,
    game: Box<dyn Game>,
    game_read: Vec<String>,
    answers: BTreeMap<String, String>,
    question_limit: usize,
    consulted: BTreeMap<String, String>,
    pending: BTreeMap<String, Question>,
    invalid_answer: Option<(String, String)>,
    game_inputs: BTreeMap<String, Option<String>>,
    log: Vec<String>,
}

impl State {
    fn new(
        limits: &Limits,
        archive: Box<dyn Archive>,
        game: Box<dyn Game>,
        game_read: Vec<String>,
        answers: BTreeMap<String, String>,
    ) -> Self {
        let entries = archive
            .entries()
            .into_iter()
            .map(|entry| (entry.path, entry.size))
            .collect();
        Self {
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.memory)
                .memories(1)
                .tables(16)
                .instances(1)
                .build(),
            staged: Vec::new(),
            read_left: limits.read,
            archive,
            entries,
            game,
            game_read,
            answers,
            question_limit: limits.questions,
            consulted: BTreeMap::new(),
            pending: BTreeMap::new(),
            invalid_answer: None,
            game_inputs: BTreeMap::new(),
            log: Vec::new(),
        }
    }

    /// Stages `bytes` for the guest's next `take`, returning their length.
    fn stage(&mut self, bytes: Vec<u8>) -> i64 {
        let length = i64::try_from(bytes.len()).unwrap_or(BUDGET);
        self.staged = bytes;
        length
    }

    /// Stages bytes that were read, charging them to the read budget.
    fn charge(&mut self, bytes: Vec<u8>, limit: u64) -> i64 {
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size > limit {
            return LIMIT;
        }
        if size > self.read_left {
            return BUDGET;
        }
        self.read_left -= size;
        self.stage(bytes)
    }

    fn read_archive(&mut self, path: &str, limit: u64) -> i64 {
        let Some(&size) = self.entries.get(path) else {
            return NOT_FOUND;
        };
        if size > limit {
            return LIMIT;
        }
        if size > self.read_left {
            return BUDGET;
        }
        match self.archive.read(path) {
            Ok(bytes) => self.charge(bytes, limit),
            Err(ReadError::NotFound) => NOT_FOUND,
            Err(ReadError::Failed(_)) => INVALID,
        }
    }

    fn read_game(&mut self, path: &str, limit: u64) -> i64 {
        if !self.game_read.iter().any(|glob| matches_glob(glob, path)) {
            return DENIED;
        }
        let file = match self.game.stat(path) {
            Ok(Some(file)) => file,
            Ok(None) | Err(ReadError::NotFound) => {
                self.game_inputs.insert(path.to_owned(), None);
                return NOT_FOUND;
            }
            Err(ReadError::Failed(_)) => return INVALID,
        };
        self.game_inputs
            .insert(path.to_owned(), Some(file.identity));
        if file.size > limit {
            return LIMIT;
        }
        if file.size > self.read_left {
            return BUDGET;
        }
        match self.game.read(path) {
            Ok(bytes) => self.charge(bytes, limit),
            Err(ReadError::NotFound) => NOT_FOUND,
            Err(ReadError::Failed(_)) => INVALID,
        }
    }

    fn ask(&mut self, bytes: &[u8]) -> i64 {
        let Ok(question) = serde_json::from_slice::<Question>(bytes) else {
            return INVALID;
        };
        if !question.is_valid() {
            return INVALID;
        }
        let answer = match self.answers.get(&question.id) {
            Some(answer) if question.accepts(answer) => answer.clone(),
            Some(answer) => {
                self.invalid_answer
                    .get_or_insert_with(|| (question.id.clone(), answer.clone()));
                return INVALID;
            }
            None => {
                if let Some(default) = question.default.clone() {
                    default
                } else {
                    if !self.pending.contains_key(&question.id)
                        && self.pending.len() >= self.question_limit
                    {
                        return LIMIT;
                    }
                    self.pending.insert(question.id.clone(), question);
                    return QUESTION_PENDING;
                }
            }
        };
        self.consulted.insert(question.id, answer.clone());
        self.stage(answer.into_bytes())
    }
}

/// An empty mod and installation, used only to check a module's ABI version.
struct Nothing;

impl Archive for Nothing {
    fn entries(&self) -> Vec<Entry> {
        Vec::new()
    }

    fn read(&self, _: &str) -> Result<Vec<u8>, ReadError> {
        Err(ReadError::NotFound)
    }
}

impl Game for Nothing {
    fn stat(&self, _: &str) -> Result<Option<GameFile>, ReadError> {
        Ok(None)
    }

    fn read(&self, _: &str) -> Result<Vec<u8>, ReadError> {
        Err(ReadError::NotFound)
    }
}

fn engine() -> Result<Engine, HostError> {
    let mut config = Config::new();
    config.consume_fuel(true);
    // Identical inputs must give identical output: no shared-memory threads, and relaxed SIMD
    // results and NaN bit patterns fixed across machines.
    config.wasm_threads(false);
    config.relaxed_simd_deterministic(true);
    config.cranelift_nan_canonicalization(true);
    Engine::new(&config).map_err(invalid_module)
}

/// Refuses any import the host does not provide, or that needs a capability `grant` lacks.
fn validate_imports(module: &Module, grant: &Grant) -> Result<(), HostError> {
    for import in module.imports() {
        let known = IMPORTS
            .iter()
            .find(|(module, name, _)| *module == import.module() && *name == import.name());
        let Some((_, _, capability)) = known.filter(|_| matches!(import.ty(), ExternType::Func(_)))
        else {
            return Err(HostError::UnknownImport {
                module: import.module().to_owned(),
                name: import.name().to_owned(),
            });
        };
        if !grant.allows(*capability) {
            return Err(HostError::UngrantedImport {
                module: import.module().to_owned(),
                name: import.name().to_owned(),
                capability: capability.map_or("", ExtensionCapability::as_str),
            });
        }
    }
    Ok(())
}

fn link_host(linker: &mut Linker<State>) -> Result<(), HostError> {
    linker
        .func_wrap(
            "msbe_host",
            "take",
            |mut caller: Caller<'_, State>, pointer: i32, length: i32| -> i32 {
                let bytes = std::mem::take(&mut caller.data_mut().staged);
                let capacity = usize::try_from(length).unwrap_or(0);
                if bytes.len() > capacity || !write_memory(&mut caller, pointer, &bytes) {
                    code32(LIMIT)
                } else {
                    i32::try_from(bytes.len()).unwrap_or(i32::MAX)
                }
            },
        )
        .map_err(invalid_module)?;
    linker
        .func_wrap(
            "msbe_host",
            "log",
            |mut caller: Caller<'_, State>, pointer: i32, length: i32| {
                let length = length.clamp(0, i32::try_from(MESSAGE_LIMIT).unwrap_or(i32::MAX));
                if let Some(bytes) = read_memory(&mut caller, pointer, length) {
                    let log = &mut caller.data_mut().log;
                    if log.len() < LOG_LINES {
                        log.push(String::from_utf8_lossy(&bytes).into_owned());
                    }
                }
            },
        )
        .map_err(invalid_module)?;
    Ok(())
}

fn link_archive(linker: &mut Linker<State>) -> Result<(), HostError> {
    linker
        .func_wrap(
            "msbe_archive",
            "entries",
            |mut caller: Caller<'_, State>| -> i64 {
                let state = caller.data_mut();
                let entries: Vec<Entry> = state
                    .entries
                    .iter()
                    .map(|(path, size)| Entry {
                        path: path.clone(),
                        size: *size,
                    })
                    .collect();
                serde_json::to_vec(&entries).map_or(LIMIT, |bytes| state.stage(bytes))
            },
        )
        .map_err(invalid_module)?;
    linker
        .func_wrap(
            "msbe_archive",
            "read",
            |mut caller: Caller<'_, State>, pointer: i32, length: i32, limit: i64| -> i64 {
                let Some(path) = read_path(&mut caller, pointer, length) else {
                    return UNSAFE_PATH;
                };
                caller
                    .data_mut()
                    .read_archive(&path, u64::try_from(limit).unwrap_or(0))
            },
        )
        .map_err(invalid_module)?;
    Ok(())
}

fn link_game(linker: &mut Linker<State>) -> Result<(), HostError> {
    linker
        .func_wrap(
            "msbe_game",
            "read",
            |mut caller: Caller<'_, State>, pointer: i32, length: i32, limit: i64| -> i64 {
                let Some(path) = read_path(&mut caller, pointer, length) else {
                    return UNSAFE_PATH;
                };
                caller
                    .data_mut()
                    .read_game(&path, u64::try_from(limit).unwrap_or(0))
            },
        )
        .map_err(invalid_module)?;
    Ok(())
}

fn link_ui(linker: &mut Linker<State>) -> Result<(), HostError> {
    linker
        .func_wrap(
            "msbe_ui",
            "ask",
            |mut caller: Caller<'_, State>, pointer: i32, length: i32| -> i64 {
                if usize::try_from(length).is_ok_and(|length| length > MESSAGE_LIMIT) {
                    return LIMIT;
                }
                match read_memory(&mut caller, pointer, length) {
                    Some(bytes) => caller.data_mut().ask(&bytes),
                    None => INVALID,
                }
            },
        )
        .map_err(invalid_module)?;
    Ok(())
}

/// Calls `msbe_run` with `request` and decodes its response.
fn call(
    store: &mut Store<State>,
    instance: &wasmtime::Instance,
    request: &[u8],
    response_limit: usize,
) -> Result<Value, HostError> {
    let size = i32::try_from(request.len()).map_err(|_| HostError::Limit("request"))?;
    let pointer = instance
        .get_typed_func::<i32, i32>(&mut *store, "msbe_alloc")
        .map_err(invalid_module)?
        .call(&mut *store, size)
        .map_err(|error| trap(&error))?;
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or(HostError::MissingExport("memory"))?;
    let address = usize::try_from(pointer)
        .map_err(|_| HostError::InvalidResponse("msbe_alloc returned a negative address".into()))?;
    memory
        .write(&mut *store, address, request)
        .map_err(|error| HostError::InvalidResponse(error.to_string()))?;
    let packed = instance
        .get_typed_func::<(i32, i32), i64>(&mut *store, "msbe_run")
        .map_err(invalid_module)?
        .call(&mut *store, (pointer, size))
        .map_err(|error| trap(&error))?;
    let bits = u64::from_ne_bytes(packed.to_ne_bytes());
    let address = usize::try_from(bits >> 32).map_err(invalid_response)?;
    let length = usize::try_from(bits & u64::from(u32::MAX)).map_err(invalid_response)?;
    if length > response_limit {
        return Err(HostError::Limit("response"));
    }
    let mut bytes = vec![0; length];
    memory
        .read(&*store, address, &mut bytes)
        .map_err(invalid_response)?;
    decode(serde_json::from_slice(&bytes).map_err(invalid_response)?)
}

fn decode(response: Value) -> Result<Value, HostError> {
    let Value::Object(mut response) = response else {
        return Err(invalid_response("the response is not a JSON object"));
    };
    if let Some(ok) = response.remove("ok") {
        return Ok(ok);
    }
    let error = response
        .remove("error")
        .ok_or_else(|| invalid_response("the response has neither ok nor error"))?;
    let text = |field: &str| error.get(field).and_then(Value::as_str).map(str::to_owned);
    Err(HostError::Extension {
        kind: text("kind").unwrap_or_else(|| "extension".to_owned()),
        message: text("message")
            .or_else(|| text("question"))
            .unwrap_or_else(|| error.to_string()),
    })
}

fn read_memory(caller: &mut Caller<'_, State>, pointer: i32, length: i32) -> Option<Vec<u8>> {
    let memory = caller.get_export("memory")?.into_memory()?;
    let mut bytes = vec![0; usize::try_from(length).ok()?];
    memory
        .read(&*caller, usize::try_from(pointer).ok()?, &mut bytes)
        .ok()?;
    Some(bytes)
}

fn write_memory(caller: &mut Caller<'_, State>, pointer: i32, bytes: &[u8]) -> bool {
    let Some(memory) = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
    else {
        return false;
    };
    usize::try_from(pointer).is_ok_and(|address| memory.write(&mut *caller, address, bytes).is_ok())
}

/// A guest path that is valid UTF-8 and a safe relative path.
fn read_path(caller: &mut Caller<'_, State>, pointer: i32, length: i32) -> Option<String> {
    if usize::try_from(length).ok()? > MESSAGE_LIMIT {
        return None;
    }
    let path = String::from_utf8(read_memory(caller, pointer, length)?).ok()?;
    is_relative_path(&path).then_some(path)
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn describe(questions: &[Question]) -> String {
    questions
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn code32(code: i64) -> i32 {
    i32::try_from(code).unwrap_or(i32::MIN)
}

fn invalid_module(error: impl fmt::Display) -> HostError {
    HostError::InvalidModule(error.to_string())
}

fn invalid_response(error: impl fmt::Display) -> HostError {
    HostError::InvalidResponse(error.to_string())
}

fn trap(error: &wasmtime::Error) -> HostError {
    match error.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => HostError::OutOfFuel,
        _ => HostError::Trap(format!("{error:#}")),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fmt::Write as _};

    use msbe_plan_schema::{EmitKind, ExtensionCapability};

    use super::{
        Archive, Entry, Game, GameFile, Grant, HostError, Invocation, Limits, Outcome, ReadError,
        StepExtension, StepOperation,
    };

    const OPTION_INSTALLER: &[u8] = include_bytes!("../tests/fixtures/option-installer.wasm");

    #[derive(Debug, Clone, Default)]
    struct Files(BTreeMap<String, Vec<u8>>);

    impl Files {
        fn of(files: &[(&str, &[u8])]) -> Self {
            Self(
                files
                    .iter()
                    .map(|(path, bytes)| ((*path).to_owned(), bytes.to_vec()))
                    .collect(),
            )
        }
    }

    impl Archive for Files {
        fn entries(&self) -> Vec<Entry> {
            self.0
                .iter()
                .map(|(path, bytes)| Entry {
                    path: path.clone(),
                    size: bytes.len() as u64,
                })
                .collect()
        }

        fn read(&self, path: &str) -> Result<Vec<u8>, ReadError> {
            self.0.get(path).cloned().ok_or(ReadError::NotFound)
        }
    }

    impl Game for Files {
        fn stat(&self, path: &str) -> Result<Option<GameFile>, ReadError> {
            Ok(self.0.get(path).map(|bytes| GameFile {
                size: bytes.len() as u64,
                identity: format!("identity:{path}"),
            }))
        }

        fn read(&self, path: &str) -> Result<Vec<u8>, ReadError> {
            Archive::read(self, path)
        }
    }

    fn wat_string(value: &str) -> String {
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    }

    fn packed(address: u64, length: usize) -> u64 {
        (address << 32) | length as u64
    }

    /// A module with `imports`, `data` at address 0, and a `msbe_run` whose body is `run`.
    fn module(imports: &str, data: &str, run: &str) -> Vec<u8> {
        wat::parse_str(format!(
            r#"(module
                {imports}
                (memory (export "memory") 1)
                (data (i32.const 0) "{}")
                (func (export "msbe_abi_version") (result i32) i32.const 1)
                (func (export "msbe_alloc") (param i32) (result i32) i32.const 32768)
                (func (export "msbe_run") (param i32 i32) (result i64) {run})
            )"#,
            wat_string(data)
        ))
        .unwrap()
    }

    /// A module whose `msbe_run` returns `response` unchanged.
    fn responding(response: &str) -> Vec<u8> {
        module(
            "",
            response,
            &format!("i64.const {}", packed(0, response.len())),
        )
    }

    fn grant(capabilities: &[ExtensionCapability], emit: &[EmitKind]) -> Grant {
        Grant::new(
            capabilities.iter().copied(),
            ["Data/*.esm".to_owned()],
            emit.iter().copied(),
        )
    }

    fn run(
        extension: &StepExtension,
        archive: Files,
        game: Files,
        answers: &[(&str, &str)],
    ) -> Result<Outcome, HostError> {
        let answers: BTreeMap<String, String> = answers
            .iter()
            .map(|(question, answer)| ((*question).to_owned(), (*answer).to_owned()))
            .collect();
        let roots = ["mods".to_owned()];
        extension.run(
            &Invocation {
                step: "install",
                module: "pack",
                loader: "default",
                game_version: None,
                parameters: &BTreeMap::new(),
                roots: &roots,
                answers: &answers,
            },
            Box::new(archive),
            Box::new(game),
        )
    }

    #[test]
    fn imports_outside_the_grant_or_the_host_do_not_load() {
        let game_import = r#"(import "msbe_game" "read" (func (param i32 i32 i64) (result i64)))"#;
        let reads_game = module(game_import, "", "i64.const 0");
        let archive_only = grant(&[ExtensionCapability::ArchiveRead], &[EmitKind::Place]);
        assert!(matches!(
            StepExtension::load(&reads_game, archive_only, Limits::default()),
            Err(HostError::UngrantedImport {
                capability: "game-read",
                ..
            })
        ));
        let granted = grant(&[ExtensionCapability::GameRead], &[EmitKind::Place]);
        assert!(StepExtension::load(&reads_game, granted, Limits::default()).is_ok());

        let random =
            r#"(import "wasi_snapshot_preview1" "random_get" (func (param i32 i32) (result i32)))"#;
        let everything = grant(
            &[
                ExtensionCapability::ArchiveRead,
                ExtensionCapability::GameRead,
                ExtensionCapability::UiPrompt,
            ],
            &[EmitKind::Place],
        );
        assert!(matches!(
            StepExtension::load(
                &module(random, "", "i64.const 0"),
                everything,
                Limits::default()
            ),
            Err(HostError::UnknownImport { .. })
        ));
    }

    #[test]
    fn modules_must_export_the_step_abi_at_its_version() {
        let wrong_version = wat::parse_str(
            r#"(module
                (memory (export "memory") 1)
                (func (export "msbe_abi_version") (result i32) i32.const 2)
                (func (export "msbe_alloc") (param i32) (result i32) i32.const 0)
                (func (export "msbe_run") (param i32 i32) (result i64) i64.const 0)
            )"#,
        )
        .unwrap();
        assert!(matches!(
            StepExtension::load(&wrong_version, Grant::default(), Limits::default()),
            Err(HostError::AbiVersion(2))
        ));
        let no_run = wat::parse_str(
            r#"(module (memory (export "memory") 1)
                (func (export "msbe_abi_version") (result i32) i32.const 1)
                (func (export "msbe_alloc") (param i32) (result i32) i32.const 0))"#,
        )
        .unwrap();
        assert!(matches!(
            StepExtension::load(&no_run, Grant::default(), Limits::default()),
            Err(HostError::MissingExport("msbe_run"))
        ));
    }

    #[test]
    fn returned_operations_are_checked_against_the_declaration_the_mod_and_the_roots() {
        let archive = || Files::of(&[("a.txt", b"a")]);
        let checked = |response: &str, emit: &[EmitKind]| {
            let extension = StepExtension::load(
                &responding(response),
                grant(&[ExtensionCapability::ArchiveRead], emit),
                Limits::default(),
            )
            .unwrap();
            run(&extension, archive(), Files::default(), &[])
        };
        let both = [EmitKind::Place, EmitKind::WriteFile];
        let place = |source: &str, path: &str| {
            format!(
                r#"{{"ok":{{"operations":[{{"kind":"place","source":"{source}","path":"{path}"}}]}}}}"#
            )
        };
        let write =
            r#"{"ok":{"operations":[{"kind":"write-file","path":"mods/a.txt","text":"x"}]}}"#;

        assert!(matches!(
            checked(write, &[EmitKind::Place]),
            Err(HostError::UndeclaredOperation("write-file"))
        ));
        assert!(matches!(
            checked(&place("a.txt", "config/a.txt"), &both),
            Err(HostError::OutsideRoots(_))
        ));
        assert!(matches!(
            checked(&place("a.txt", "mods/../a.txt"), &both),
            Err(HostError::UnsafePath(_))
        ));
        assert!(matches!(
            checked(&place("missing.txt", "mods/a.txt"), &both),
            Err(HostError::UnknownSource(_))
        ));
        let twice = r#"{"ok":{"operations":[
            {"kind":"place","source":"a.txt","path":"mods/a.txt"},
            {"kind":"write-file","path":"mods/a.txt","text":"x"}]}}"#;
        assert!(matches!(
            checked(twice, &both),
            Err(HostError::DuplicatePath(_))
        ));
        let failed = r#"{"error":{"kind":"invalid-archive","message":"no manifest"}}"#;
        assert!(matches!(
            checked(failed, &both),
            Err(HostError::Extension { kind, message }) if kind == "invalid-archive" && message == "no manifest"
        ));

        let outcome = checked(write, &both).unwrap();
        assert_eq!(
            outcome.operations,
            [StepOperation::WriteFile {
                path: "mods/a.txt".to_owned(),
                contents: b"x".to_vec()
            }]
        );
    }

    #[test]
    fn a_runaway_extension_runs_out_of_fuel() {
        let spinning = module("", "", "(loop (br 0)) i64.const 0");
        let limits = Limits {
            fuel: 100_000,
            ..Limits::default()
        };
        let extension = StepExtension::load(&spinning, Grant::default(), limits).unwrap();
        assert!(matches!(
            run(&extension, Files::default(), Files::default(), &[]),
            Err(HostError::OutOfFuel)
        ));
    }

    #[test]
    fn unanswered_questions_stop_the_run_and_consulted_answers_are_recorded() {
        let ok = r#"{"ok":{"operations":[]}}"#;
        let question = r#"{"id":"size","prompt":"Size","kind":"choice","choices":[{"id":"small","label":"Small"},{"id":"large","label":"Large"}]}"#;
        let data = format!("{ok}{question}");
        let asking = module(
            r#"(import "msbe_ui" "ask" (func $ask (param i32 i32) (result i64)))"#,
            &data,
            &format!(
                "(drop (call $ask (i32.const {}) (i32.const {}))) i64.const {}",
                ok.len(),
                question.len(),
                packed(0, ok.len())
            ),
        );
        let extension = StepExtension::load(
            &asking,
            grant(&[ExtensionCapability::UiPrompt], &[EmitKind::Place]),
            Limits::default(),
        )
        .unwrap();

        match run(&extension, Files::default(), Files::default(), &[]) {
            Err(HostError::QuestionsRequired(questions)) => {
                assert_eq!(questions.len(), 1);
                assert!(questions.iter().all(|question| question.id == "size"));
            }
            other => panic!("expected a required question, got {other:?}"),
        }
        let answered = run(
            &extension,
            Files::default(),
            Files::default(),
            &[("size", "large")],
        );
        assert_eq!(
            answered.unwrap().answers,
            BTreeMap::from([("size".to_owned(), "large".to_owned())])
        );
        assert!(matches!(
            run(&extension, Files::default(), Files::default(), &[("size", "huge")]),
            Err(HostError::InvalidAnswer { answer, .. }) if answer == "huge"
        ));
    }

    #[test]
    fn game_reads_are_limited_to_the_granted_globs_and_recorded() {
        let ok = r#"{"ok":{"operations":[]}}"#;
        let paths = ["Data/Base.esm", "Data/Missing.esm", "Secret/key.txt"];
        let data = format!("{ok}{}", paths.concat());
        let mut offset = ok.len();
        let mut reads = String::new();
        for (path, expected) in paths.iter().zip(["4", "-1", "-6"]) {
            write!(
                reads,
                "(if (i64.ne (call $read (i32.const {offset}) (i32.const {}) (i64.const 100)) (i64.const {expected})) (then unreachable))",
                path.len()
            )
            .unwrap();
            offset += path.len();
        }
        let reading = module(
            r#"(import "msbe_game" "read" (func $read (param i32 i32 i64) (result i64)))"#,
            &data,
            &format!("{reads} i64.const {}", packed(0, ok.len())),
        );
        let extension = StepExtension::load(
            &reading,
            grant(&[ExtensionCapability::GameRead], &[EmitKind::Place]),
            Limits::default(),
        )
        .unwrap();
        let game = Files::of(&[("Data/Base.esm", b"base"), ("Secret/key.txt", b"key")]);
        let outcome = run(&extension, Files::default(), game, &[]).unwrap();
        assert_eq!(
            outcome.game_inputs,
            BTreeMap::from([
                (
                    "Data/Base.esm".to_owned(),
                    Some("identity:Data/Base.esm".to_owned())
                ),
                ("Data/Missing.esm".to_owned(), None),
            ])
        );
    }

    #[test]
    fn the_option_installer_places_the_option_the_game_offers_and_the_user_chose() {
        let manifest = br#"{"format":"option-installer","version":1,"always":"core","groups":[
            {"id":"textures","prompt":"Textures","default":"standard","options":[
                {"id":"standard","label":"Standard","directory":"options/standard"},
                {"id":"high","label":"High","directory":"options/high","requires":"Data/High.esm"}]}]}"#;
        let archive = || {
            Files::of(&[
                ("installer.json", manifest),
                ("core/readme.txt", b"core"),
                ("options/standard/texture.dds", b"standard"),
                ("options/high/texture.dds", b"high"),
            ])
        };
        let extension = StepExtension::load(
            OPTION_INSTALLER,
            grant(
                &[
                    ExtensionCapability::ArchiveRead,
                    ExtensionCapability::GameRead,
                    ExtensionCapability::UiPrompt,
                ],
                &[EmitKind::Place, EmitKind::WriteFile],
            ),
            Limits::default(),
        )
        .unwrap();
        let game = Files::of(&[("Data/High.esm", b"esm")]);

        let chosen = run(&extension, archive(), game.clone(), &[("textures", "high")]).unwrap();
        assert!(chosen.operations.contains(&StepOperation::Place {
            source: "options/high/texture.dds".to_owned(),
            path: "mods/texture.dds".to_owned(),
        }));
        assert!(chosen.operations.contains(&StepOperation::Place {
            source: "core/readme.txt".to_owned(),
            path: "mods/readme.txt".to_owned(),
        }));
        assert!(chosen.operations.contains(&StepOperation::WriteFile {
            path: "mods/pack.choices.txt".to_owned(),
            contents: b"textures=high\n".to_vec(),
        }));
        assert_eq!(
            chosen.game_inputs.get("Data/High.esm"),
            Some(&Some("identity:Data/High.esm".to_owned()))
        );

        let defaulted = run(&extension, archive(), game, &[]).unwrap();
        assert_eq!(
            defaulted.answers.get("textures").map(String::as_str),
            Some("standard")
        );
        assert!(matches!(
            run(
                &extension,
                archive(),
                Files::default(),
                &[("textures", "high")]
            ),
            Err(HostError::InvalidAnswer { .. })
        ));
    }
}
