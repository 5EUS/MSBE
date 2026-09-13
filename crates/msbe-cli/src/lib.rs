//! The `msbe` command-line interface, built as a library so it can be tested in-process.
//!
//! Results go to stdout, as text or, with `--format json`, as JSON. Diagnostics always go to
//! stderr, so piped output stays clean. See `docs/09-interfaces.md`.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use clap::{Parser, Subcommand, ValueEnum};
use msbe_core::{
    ExclusionReason,
    config::Home,
    instance::{
        Artifact, DEFAULT_PROFILE, DeployPlan, DeployReport, Instance, InstanceError, ModExclusion,
        Name, NewInstance, Profile, ProfileTarget, Provenance, Status,
    },
};
use msbe_fsops::{Backend, Digest, NoopObserver, Operation, RelPath, Store};
use msbe_pack::{
    CaptureKind, CaptureRequest, DiffKind, Direction, ExportRequest, ImportAction, ImportItem,
    ImportRequest, IssueCode, PackError, PackIssue, Resolution, Silent, UpdateRequest,
    inclusion::PreviewGroup,
};
use msbe_plan_schema::Side;
use msbe_provider_api::{
    AdapterError, HttpClient, HttpError, ManifestError, PackOptions, PackWarning, PackageId,
    Target, Update, UpdateCheck,
    model::{Request, Selection},
    resolve::{
        InstallPlan, InstalledRelease, ProjectRequest, Requirement, ResolveError, Resolver,
        Substitution,
    },
};
use msbe_providers::{
    AuthoringError, Providers, RegistryError, Routed, SignerKey, codecs_directory, sign_codec,
    trust_file, verify_codec,
};
use serde::Serialize;

#[cfg(test)]
mod end_to_end_tests;
#[cfg(test)]
mod fake_modrinth;

/// Opens a network client on first use, so commands that never touch the network never load
/// the platform's certificates.
pub type Connect<'a> = &'a dyn Fn() -> Result<Box<dyn HttpClient>, HttpError>;

/// Process exit codes. Scripts depend on these, so they are a compatibility contract.
pub mod exit {
    /// Success.
    pub const OK: u8 = 0;
    /// A failure without a more specific code.
    pub const FAILURE: u8 = 1;
    /// The command line could not be parsed.
    pub const USAGE: u8 = 2;
    /// Mods in a profile claim the same path with different contents, or a pack change no longer
    /// applies.
    pub const CONFLICT: u8 = 4;
    /// Provider or distribution policy refused the operation.
    pub const POLICY: u8 = 6;
    /// Deployed files no longer match what was deployed.
    pub const INTEGRITY: u8 = 7;
}

/// A game-agnostic mod manager.
#[derive(Debug, Parser)]
#[command(name = "msbe", version)]
struct Cli {
    /// MSBE's data directory. Defaults to `$MSBE_HOME`, then the platform data directory.
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,
    /// How to print results.
    #[arg(long, global = true, value_enum, default_value_t = Format::Human)]
    format: Format,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Human,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TargetSide {
    Client,
    Server,
}

impl From<TargetSide> for Side {
    fn from(side: TargetSide) -> Self {
        match side {
            TargetSide::Client => Self::Client,
            TargetSide::Server => Self::Server,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Register, configure and list game instances.
    #[command(subcommand)]
    Instance(InstanceCommand),
    /// Create, list and inspect profiles.
    #[command(subcommand)]
    Profile(ProfileCommand),
    /// Import, update, export and capture packs through reviewed codecs.
    #[command(subcommand)]
    Pack(PackCommand),
    /// Back up and restore an instance's MSBE state. Snapshots are private backups, not packs.
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Diagnose one broken module through deterministic trial deployments.
    #[command(subcommand)]
    Bisect(BisectCommand),
    /// Create signing keys, sign WebAssembly pack codecs, and check signed codecs against the local
    /// trust root.
    #[command(subcommand)]
    Extension(ExtensionCommand),
    /// Add mods to a profile from local files, .zip archives, provider references, or https URLs.
    Add {
        /// The instance.
        instance: String,
        /// Files, .zip archives, <provider>:<project>[@<version>] references, or https:// URLs,
        /// optionally pinned with #sha256=<hex> or #sha512=<hex>.
        #[arg(required = true, value_name = "SOURCE")]
        sources: Vec<String>,
        /// The profile to add to.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
        /// Also add every required dependency of mods from providers that publish them.
        #[arg(long)]
        with_deps: bool,
    },
    /// Search every provider that supports it for mods compatible with a profile target.
    Search {
        /// The instance.
        instance: String,
        /// Words to search for.
        #[arg(required = true)]
        query: Vec<String>,
        /// The profile whose target filters the results.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
        /// The most results to show.
        #[arg(long, default_value_t = 10)]
        limit: u8,
    },
    /// Move mods to newer compatible releases from the providers they came from.
    Update {
        /// The instance.
        instance: String,
        /// Mods to update. Defaults to every mod in the profile a provider can update.
        #[arg(value_name = "MOD")]
        modules: Vec<String>,
        /// The profile to update.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
        /// Show what would change without downloading or changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove a mod from a profile.
    Remove {
        /// The instance.
        instance: String,
        /// The mod to remove.
        module: String,
        /// The profile to remove it from.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
    },
    /// Make the instance match a profile, in one journaled transaction.
    Deploy {
        /// The instance.
        instance: String,
        /// The profile to deploy.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
        /// Show what would change without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Write a portable, reproducible lockfile for a profile.
    Lock {
        /// The instance.
        instance: String,
        /// The profile to lock.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
    },
    /// Undo the most recent deployment.
    Rollback {
        /// The instance.
        instance: String,
    },
    /// Undo every deployment, returning the instance to its state before MSBE.
    Purge {
        /// The instance.
        instance: String,
    },
    /// Re-hash deployed files and report any that changed or disappeared.
    Verify {
        /// The instance.
        instance: String,
    },
    /// Show an instance, recovering an interrupted transaction first.
    Status {
        /// The instance.
        instance: String,
    },
}

#[derive(Debug, Subcommand)]
enum BisectCommand {
    /// Start a resumable bisection for a profile.
    Start {
        instance: String,
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
    },
    /// Deploy the current trial subset for manual testing.
    Run { instance: String },
    /// Record whether the current trial reproduces the problem.
    Result {
        instance: String,
        #[arg(long, conflicts_with = "good")]
        bad: bool,
        #[arg(long, conflicts_with = "bad")]
        good: bool,
    },
    /// Restore the original profile and remove the bisection session.
    Finish { instance: String },
}

#[derive(Debug, Subcommand)]
enum PackCommand {
    /// Manage files owned by a profile rather than by an installed mod.
    #[command(subcommand)]
    Config(PackConfigCommand),
    /// Validate a profile and write its canonical lockfile.
    Validate {
        /// The instance.
        instance: String,
        /// The profile to validate.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
    },
    /// List the pack formats reviewed codecs provide.
    Formats {
        /// Only formats that support this direction.
        #[arg(long, value_enum)]
        direction: Option<PackDirection>,
        /// Only formats that support this game plan.
        #[arg(long)]
        game: Option<String>,
    },
    /// Show a codec's option schema and normalized values.
    Options {
        /// The codec ID, such as msbe-native.
        codec: String,
        /// Apply a preset's values.
        #[arg(long)]
        preset: Option<String>,
        /// The direction whose schema to show.
        #[arg(long, value_enum, default_value_t = PackDirection::Export)]
        direction: PackDirection,
    },
    /// Import a pack into a new or empty profile, recording it as the profile's pack layer.
    Import {
        /// The instance.
        instance: String,
        /// The pack file.
        input: PathBuf,
        /// The codec ID. Detected from the input when omitted.
        #[arg(long)]
        codec: Option<String>,
        /// A TOML file of codec option values.
        #[arg(long, value_name = "FILE")]
        options: Option<PathBuf>,
        /// The profile to create or fill.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
        /// Show the preview without acquiring or writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Replace a profile's pack layer with another version and reapply its changes.
    Update {
        /// The instance.
        instance: String,
        /// The new pack version.
        input: PathBuf,
        /// The codec ID. Detected from the input when omitted.
        #[arg(long)]
        codec: Option<String>,
        /// The profile to update.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
        /// Resolve a conflict, such as mod:sodium=keep or config:config/a.toml=drop.
        #[arg(long = "resolve", value_name = "CONFLICT=keep|drop")]
        resolutions: Vec<String>,
        /// Show the preview without acquiring or writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Export a profile through a reviewed codec.
    Export {
        /// The instance.
        instance: String,
        /// The pack file to write.
        output: PathBuf,
        /// The codec ID, such as msbe-native.
        #[arg(long)]
        codec: String,
        /// A preset to start from: thin, portable, complete or public-distribution.
        #[arg(long)]
        preset: Option<String>,
        /// A TOML file of option values, applied over the preset.
        #[arg(long, value_name = "FILE")]
        options: Option<PathBuf>,
        /// The profile to export.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
        /// Show the preview without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Adopt in-game changes beneath the plan's mutable roots into the deployed profile.
    Capture {
        /// The instance.
        instance: String,
        /// The deployed profile.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
        /// Capture only these paths.
        #[arg(long = "path", value_name = "PATH")]
        paths: Vec<String>,
        /// Show each change and its diff without recording anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PackDirection {
    Import,
    Export,
}

impl From<PackDirection> for Direction {
    fn from(direction: PackDirection) -> Self {
        match direction {
            PackDirection::Import => Self::Import,
            PackDirection::Export => Self::Export,
        }
    }
}

#[derive(Debug, Subcommand)]
enum SnapshotCommand {
    /// Write a snapshot of an instance, including content no export may embed.
    Create {
        /// The instance.
        instance: String,
        /// The snapshot file to write.
        output: PathBuf,
    },
    /// Restore an instance from a snapshot. Deployment remains a separate step.
    Restore {
        /// The snapshot file.
        input: PathBuf,
        /// Show what would be restored without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ExtensionCommand {
    /// Generate a signing key, and print the trust entry that lets MSBE accept its signatures.
    Keygen {
        /// The signer ID every envelope the key signs names.
        signer: String,
        /// The key file to create. An existing file is never replaced.
        key: PathBuf,
    },
    /// Sign a WebAssembly pack codec, writing its envelope beside the module.
    Sign {
        /// The .wasm module.
        module: PathBuf,
        /// The key file to sign with.
        #[arg(long, value_name = "FILE")]
        key: PathBuf,
        /// The version to publish the codec as.
        #[arg(long)]
        version: String,
        /// The extension ID. Defaults to the codec ID the module declares.
        #[arg(long)]
        id: Option<String>,
    },
    /// Check a signed codec against the local trust root, as installing it would, without
    /// installing it.
    Verify {
        /// The envelope document beside the module.
        envelope: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum PackConfigCommand {
    /// List pack-owned config files.
    List {
        /// The instance.
        instance: String,
        /// The profile.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
    },
    /// Show one pack-owned text config.
    Show {
        /// The instance.
        instance: String,
        /// Game-relative config path.
        path: String,
        /// The profile.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
    },
    /// Add or replace a pack-owned config.
    Set {
        /// The instance.
        instance: String,
        /// Game-relative config path.
        path: String,
        /// Text content supplied directly by an editor.
        #[arg(long, conflicts_with = "file", required_unless_present = "file")]
        content: Option<String>,
        /// Local file whose exact bytes should be stored.
        #[arg(long, conflicts_with = "content")]
        file: Option<PathBuf>,
        /// The profile.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
    },
    /// Remove a pack-owned config.
    Remove {
        /// The instance.
        instance: String,
        /// Game-relative config path.
        path: String,
        /// The profile.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
    },
}

#[derive(Debug, Subcommand)]
enum InstanceCommand {
    /// Register a game directory as an instance.
    Add {
        /// A name for the instance.
        name: String,
        /// The game's directory.
        #[arg(long)]
        root: PathBuf,
        /// The plan manifest describing how the game installs mods.
        #[arg(long)]
        plan: PathBuf,
        /// The loader to deploy with, as declared by the plan.
        #[arg(long)]
        loader: String,
        /// The loader version providers should match, when they publish loader-version metadata.
        #[arg(long)]
        loader_version: Option<String>,
        /// Whether this instance targets a player client or dedicated server.
        #[arg(long, value_enum, default_value_t = TargetSide::Client)]
        side: TargetSide,
        /// The game version, such as 1.21.1. Needed by providers that filter by game version.
        #[arg(long)]
        game_version: Option<String>,
        /// The store shard. Defaults to .msbe/store beside the game directory, which keeps it
        /// on the same volume.
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// Change an instance's settings.
    Set {
        /// The instance.
        name: String,
        /// The game version providers should match, such as 1.21.1.
        #[arg(long)]
        game_version: Option<String>,
        /// The loader version providers should match, when they publish loader-version metadata.
        #[arg(long)]
        loader_version: Option<String>,
        /// Whether this instance targets a player client or dedicated server.
        #[arg(long, value_enum)]
        side: Option<TargetSide>,
    },
    /// List instances.
    List,
    /// Unregister an instance after restoring its managed files.
    Remove {
        /// The instance.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum ProfileCommand {
    /// Create a profile, optionally copying another.
    New {
        /// The instance.
        instance: String,
        /// The new profile's name.
        name: String,
        /// A profile to copy.
        #[arg(long)]
        from: Option<String>,
    },
    /// Delete an inactive profile.
    Remove {
        /// The instance.
        instance: String,
        /// The profile to delete.
        name: String,
    },
    /// List an instance's profiles; the deployed one is marked with `*`.
    List {
        /// The instance.
        instance: String,
    },
    /// Show the mods in a profile.
    Show {
        /// The instance.
        instance: String,
        /// The profile.
        #[arg(default_value = DEFAULT_PROFILE)]
        name: String,
    },
    /// Set the order a profile's mods apply in. Where two mods change the same thing, such as a
    /// class two jarmods replace, the later one wins. Mods not named keep their relative order
    /// after the named ones.
    Order {
        /// The instance.
        instance: String,
        /// Mods in the order they apply.
        #[arg(required = true, value_name = "MOD")]
        mods: Vec<String>,
        /// The profile.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
    },
    /// Set the loader compatibility target for one profile.
    SetTarget {
        /// The instance.
        instance: String,
        /// The profile.
        #[arg(default_value = DEFAULT_PROFILE)]
        name: String,
        /// The loader declared by the plan.
        #[arg(long)]
        loader: String,
        /// The loader version providers should match, when known.
        #[arg(long)]
        loader_version: Option<String>,
        /// Whether this profile targets a player client or dedicated server.
        #[arg(long, value_enum, default_value_t = TargetSide::Client)]
        side: TargetSide,
    },
    /// Record the answers a mod's run-extension step asks for. An empty answer removes a recorded
    /// one. Answers are checked against the step's questions at the next deploy.
    Answer {
        /// The instance.
        instance: String,
        /// The mod.
        module: String,
        /// Answers, such as `install/textures=high`.
        #[arg(required = true, value_name = "STEP/QUESTION=ANSWER")]
        answers: Vec<String>,
        /// The profile.
        #[arg(long, short, default_value = DEFAULT_PROFILE)]
        profile: String,
    },
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Instance(#[from] InstanceError),
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error(transparent)]
    Provider(#[from] RegistryError),
    #[error(transparent)]
    Authoring(#[from] AuthoringError),
    #[error(transparent)]
    Pack(#[from] PackError),
    #[error(transparent)]
    Fs(#[from] msbe_fsops::Error),
    #[error("cannot read options {}: {source}", .path.display())]
    ReadOptions {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("options {} are not a TOML table: {reason}", .path.display())]
    OptionsDocument { path: PathBuf, reason: String },
    #[error("--options needs --codec, whose schema types the values")]
    OptionsNeedCodec,
    #[error("{0:?} is not a resolution; use CONFLICT=keep or CONFLICT=drop")]
    Resolution(String),
    #[error("cannot resolve {}: {source}", .path.display())]
    Path {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("pass exactly one of --bad or --good")]
    BisectVerdict,
    #[error(
        "instance {0} has no game version; set one with `msbe instance set {0} --game-version <version>`"
    )]
    GameVersionRequired(Name),
    #[error("cannot create scratch space for downloads: {0}")]
    Scratch(#[source] io::Error),
    #[error("cannot read config {}: {source}", .path.display())]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot remove instance data at {}: {source}", .path.display())]
    RemoveInstance {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("pack-owned config {0} is not UTF-8 text")]
    ConfigNotText(RelPath),
    #[error("{0:?} is not an answer; use STEP/QUESTION=ANSWER")]
    AnswerSyntax(String),
    #[error("cannot write output: {0}")]
    Output(#[from] io::Error),
    #[error("cannot encode output: {0}")]
    Json(#[from] serde_json::Error),
}

struct Console<'a> {
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
    format: Format,
    connect: Connect<'a>,
}

impl Console<'_> {
    fn emit<T: Serialize>(
        &mut self,
        value: &T,
        human: impl FnOnce(&mut dyn Write, &T) -> io::Result<()>,
    ) -> Result<(), CliError> {
        match self.format {
            Format::Json => {
                serde_json::to_writer_pretty(&mut *self.out, value)?;
                writeln!(self.out)?;
            }
            Format::Human => human(self.out, value)?,
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct ProfileListing {
    profiles: Vec<Name>,
    deployed: Option<Name>,
}

#[derive(Serialize)]
struct RolledBack<T> {
    rolled_back: T,
}

#[derive(Default, Serialize)]
struct AddReport {
    added: Vec<Name>,
    skipped: Vec<Name>,
    unresolved: Vec<Requirement>,
    incompatible: Vec<Requirement>,
    substituted: Vec<Substitution>,
}

#[derive(Serialize)]
struct ModUpdate {
    module: Name,
    from: String,
    to: String,
}

#[derive(Default, Serialize)]
struct UpdateReport {
    dry_run: bool,
    updated: Vec<ModUpdate>,
    current: Vec<Name>,
    no_compatible_version: Vec<Name>,
    unlisted: Vec<Name>,
    /// Mods no registered provider can update.
    not_updatable: Vec<Name>,
    unresolved: Vec<Requirement>,
    incompatible: Vec<Requirement>,
}

#[derive(Serialize)]
struct PackConfigSummary {
    path: RelPath,
    digest: Digest,
}

#[derive(Serialize)]
struct PackConfigDocument {
    path: RelPath,
    digest: Digest,
    content: String,
}

#[derive(Serialize)]
struct PackValidationReport {
    lockfile: PathBuf,
    files: usize,
    configs: usize,
    mods: usize,
}

/// A mod in a profile and the provenance recorded for it.
type Tracked<'p> = (&'p Name, &'p Provenance);

/// A mod in a profile, its provenance, and the update chosen for it.
type Pending<'p> = (&'p Name, &'p Provenance, Update);

/// Runs the CLI with `args`, which include the program name, and returns the exit code.
pub fn run<I, T>(args: I, out: &mut dyn Write, err: &mut dyn Write) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    run_with(args, out, err, &|| {
        msbe_http::UreqClient::connect().map(|client| -> Box<dyn HttpClient> { Box::new(client) })
    })
}

/// Runs the CLI like [`run`], opening network clients through `connect`.
pub fn run_with<I, T>(args: I, out: &mut dyn Write, err: &mut dyn Write, connect: Connect<'_>) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let sink: &mut dyn Write = if error.use_stderr() { err } else { out };
            // A failed write to a closed terminal has nowhere left to be reported.
            drop(write!(sink, "{}", error.render()));
            return u8::try_from(error.exit_code()).unwrap_or(exit::USAGE);
        }
    };
    let mut console = Console {
        out,
        err,
        format: cli.format,
        connect,
    };
    match execute(&cli, &mut console) {
        Ok(code) => code,
        Err(error) => {
            // Likewise: if stderr is gone, the exit code is all that can still be reported.
            drop(report(&error, console.err));
            match &error {
                CliError::Instance(InstanceError::Conflicts(_)) => exit::CONFLICT,
                CliError::Pack(error) => pack_exit(error.code()),
                _ => exit::FAILURE,
            }
        }
    }
}

fn execute(cli: &Cli, console: &mut Console<'_>) -> Result<u8, CliError> {
    let home = match &cli.home {
        Some(dir) => Home::at(dir),
        None => Home::discover()?,
    };
    // Extension commands never need the installed extensions, and must work to repair them.
    if let Command::Extension(command) = &cli.command {
        return extension_command(&home, command, console);
    }
    let providers = Providers::installed(&home)?;
    match &cli.command {
        Command::Instance(command) => instance_command(&home, command, console),
        Command::Profile(command) => profile_command(&home, command, console),
        Command::Pack(command) => pack_command(&providers, &home, command, console),
        Command::Snapshot(command) => snapshot_command(&home, command, console),
        Command::Bisect(command) => bisect_command(&home, command, console),
        Command::Extension(command) => extension_command(&home, command, console),
        Command::Add {
            instance,
            sources,
            profile,
            with_deps,
        } => add(
            &providers, &home, instance, profile, sources, *with_deps, console,
        ),
        Command::Search {
            instance,
            query,
            profile,
            limit,
        } => search(&providers, &home, instance, profile, query, *limit, console),
        Command::Update {
            instance,
            modules,
            profile,
            dry_run,
        } => update(
            &providers, &home, instance, profile, modules, *dry_run, console,
        ),
        Command::Remove {
            instance,
            module,
            profile,
        } => remove(&home, instance, profile, module, console),
        Command::Deploy {
            instance,
            profile,
            dry_run,
        } => deploy(&home, instance, profile, *dry_run, console),
        Command::Lock { instance, profile } => lock(&home, instance, profile, console),
        Command::Rollback { instance } => rollback(&home, instance, console),
        Command::Purge { instance } => purge(&home, instance, console),
        Command::Verify { instance } => verify(&home, instance, console),
        Command::Status { instance } => status(&home, instance, console),
    }
}

fn bisect_command(
    home: &Home,
    command: &BisectCommand,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    match command {
        BisectCommand::Start { instance, profile } => {
            let opened = open(home, instance, console)?;
            let session = opened.start_bisect(&Name::new(profile)?)?;
            console.emit(&session, |out, session| {
                writeln!(
                    out,
                    "Started bisection with {} candidates; run the {} trial.",
                    session.candidates.len(),
                    session.trial_profile
                )
            })?;
        }
        BisectCommand::Run { instance } => {
            let mut opened = open(home, instance, console)?;
            console.emit(&opened.run_bisect(&mut NoopObserver)?, print_deploy)?;
        }
        BisectCommand::Result {
            instance,
            bad,
            good,
        } => {
            if !bad && !good {
                return Err(CliError::BisectVerdict);
            }
            let opened = open(home, instance, console)?;
            let session = opened.record_bisect(*bad)?;
            console.emit(&session, |out, session| match session.candidates.first() {
                Some(culprit) if session.trial.is_empty() => writeln!(out, "Bisection identified {culprit}. Run `msbe bisect finish {instance}` to restore the profile."),
                _ => writeln!(out, "{} candidates remain; run the next {} trial.", session.candidates.len(), session.trial_profile),
            })?;
        }
        BisectCommand::Finish { instance } => {
            let mut opened = open(home, instance, console)?;
            console.emit(&opened.finish_bisect(&mut NoopObserver)?, print_deploy)?;
        }
    }
    Ok(exit::OK)
}

fn pack_command(
    providers: &Providers,
    home: &Home,
    command: &PackCommand,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    match command {
        PackCommand::Config(command) => pack_config(home, command, console),
        PackCommand::Validate { instance, profile } => {
            pack_validate(home, instance, profile, console)
        }
        PackCommand::Formats { direction, game } => pack_formats(
            providers,
            direction.map(Direction::from),
            game.as_deref(),
            console,
        ),
        PackCommand::Options {
            codec,
            preset,
            direction,
        } => pack_options(
            providers,
            codec,
            (*direction).into(),
            preset.as_deref(),
            console,
        ),
        PackCommand::Import {
            instance,
            input,
            codec,
            options,
            profile,
            dry_run,
        } => {
            let options = match (codec, options) {
                (Some(codec), options) => {
                    options_file(providers, codec, Direction::Import, options.as_deref())?
                }
                (None, Some(_)) => return Err(CliError::OptionsNeedCodec),
                (None, None) => PackOptions::new(),
            };
            let request = ImportRequest {
                instance: Name::new(instance)?,
                profile: Name::new(profile)?,
                input: absolute(input)?,
                codec: codec.clone(),
                options,
            };
            pack_import(providers, home, request, *dry_run, console)
        }
        PackCommand::Update {
            instance,
            input,
            codec,
            profile,
            resolutions,
            dry_run,
        } => {
            let request = UpdateRequest {
                instance: Name::new(instance)?,
                profile: Name::new(profile)?,
                input: absolute(input)?,
                codec: codec.clone(),
                resolutions: parse_resolutions(resolutions)?,
            };
            pack_update(providers, home, request, *dry_run, console)
        }
        PackCommand::Export {
            instance,
            output,
            codec,
            preset,
            options,
            profile,
            dry_run,
        } => {
            let request = ExportRequest {
                instance: Name::new(instance)?,
                profile: Name::new(profile)?,
                codec: codec.clone(),
                preset: preset.clone(),
                options: options_file(providers, codec, Direction::Export, options.as_deref())?,
                output: absolute(output)?,
            };
            pack_export(providers, home, request, *dry_run, console)
        }
        PackCommand::Capture {
            instance,
            profile,
            paths,
            dry_run,
        } => {
            let request = CaptureRequest {
                instance: Name::new(instance)?,
                profile: Name::new(profile)?,
                paths: paths
                    .iter()
                    .map(String::as_str)
                    .map(RelPath::new)
                    .collect::<Result<_, _>>()?,
            };
            pack_capture(home, request, *dry_run, console)
        }
    }
}

fn pack_config(
    home: &Home,
    command: &PackConfigCommand,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    match command {
        PackConfigCommand::List { instance, profile } => {
            let opened = open(home, instance, console)?;
            let configs = opened
                .profile(&Name::new(profile)?)?
                .configs
                .into_iter()
                .map(|(path, digest)| PackConfigSummary { path, digest })
                .collect::<Vec<_>>();
            console.emit(&configs, |out, configs| {
                for config in configs {
                    writeln!(out, "{}  {}", config.path, config.digest)?;
                }
                Ok(())
            })?;
        }
        PackConfigCommand::Show {
            instance,
            path,
            profile,
        } => {
            let opened = open(home, instance, console)?;
            let profile = opened.profile(&Name::new(profile)?)?;
            let path = RelPath::new(path)?;
            let digest = *profile
                .configs
                .get(&path)
                .ok_or_else(|| InstanceError::UnknownConfig(path.clone()))?;
            let blob = Store::open(opened.config().store.clone())?.blob_path(&digest);
            let bytes =
                fs::read(&blob).map_err(|source| CliError::ReadConfig { path: blob, source })?;
            let content =
                String::from_utf8(bytes).map_err(|_| CliError::ConfigNotText(path.clone()))?;
            let document = PackConfigDocument {
                path,
                digest,
                content,
            };
            console.emit(&document, |out, document| {
                write!(out, "{}", document.content)
            })?;
        }
        PackConfigCommand::Set {
            instance,
            path,
            content,
            file,
            profile,
        } => {
            let opened = open(home, instance, console)?;
            let path = RelPath::new(path)?;
            let contents = match file {
                Some(file) => fs::read(file).map_err(|source| CliError::ReadConfig {
                    path: file.clone(),
                    source,
                })?,
                None => content.as_deref().unwrap_or_default().as_bytes().to_vec(),
            };
            let digest =
                opened.set_profile_config(&Name::new(profile)?, path.clone(), &contents)?;
            let summary = PackConfigSummary { path, digest };
            console.emit(&summary, |out, summary| {
                writeln!(out, "Saved pack-owned config {}.", summary.path)
            })?;
        }
        PackConfigCommand::Remove {
            instance,
            path,
            profile,
        } => {
            let opened = open(home, instance, console)?;
            let path = RelPath::new(path)?;
            opened.remove_profile_config(&Name::new(profile)?, &path)?;
            console.emit(&path, |out, path| {
                writeln!(out, "Removed pack-owned config {path}.")
            })?;
        }
    }
    Ok(exit::OK)
}

fn pack_validate(
    home: &Home,
    instance: &str,
    profile: &str,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let opened = open(home, instance, console)?;
    let profile = Name::new(profile)?;
    let configs = opened.profile(&profile)?.configs.len();
    let lockfile = opened.write_lockfile(&profile)?;
    let report = PackValidationReport {
        lockfile: home
            .instance(opened.name())
            .join("locks")
            .join(format!("{profile}.toml")),
        files: lockfile.deployment.len(),
        configs,
        mods: lockfile.mods.len(),
    };
    console.emit(&report, |out, report| {
        writeln!(
            out,
            "Validated {} mod(s), {} config(s), and {} file(s); wrote {}.",
            report.mods,
            report.configs,
            report.files,
            report.lockfile.display()
        )
    })?;
    Ok(exit::OK)
}

fn pack_formats(
    providers: &Providers,
    direction: Option<Direction>,
    game: Option<&str>,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let descriptors = msbe_pack::codecs(providers, direction, game);
    console.emit(&descriptors, |out, descriptors| {
        for descriptor in descriptors {
            let directions = match (descriptor.directions.import, descriptor.directions.export) {
                (true, true) => "import, export",
                (true, false) => "import",
                _ => "export",
            };
            writeln!(
                out,
                "{:<18} {} (.{}) - {directions}",
                descriptor.id,
                descriptor.name,
                descriptor.extensions.join(", .")
            )?;
        }
        Ok(())
    })?;
    Ok(exit::OK)
}

fn pack_options(
    providers: &Providers,
    codec: &str,
    direction: Direction,
    preset: Option<&str>,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let options = msbe_pack::codec_options(providers, codec, direction, preset)?;
    console.emit(&options, |out, options| {
        writeln!(out, "{} ({}) options:", options.codec, options.name)?;
        for field in &options.schema.fields {
            let value = options.values.get(&field.key).map(ToString::to_string);
            writeln!(
                out,
                "  {} = {}  # {}",
                field.key,
                value.unwrap_or_default(),
                field.description
            )?;
        }
        let presets: Vec<&str> = options
            .schema
            .presets
            .iter()
            .map(|preset| preset.id.as_str())
            .collect();
        writeln!(out, "Presets: {}", presets.join(", "))
    })?;
    Ok(exit::OK)
}

fn pack_import(
    providers: &Providers,
    home: &Home,
    request: ImportRequest,
    dry_run: bool,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let preview = msbe_pack::preview_import(providers, home, request)?;
    if dry_run {
        console.emit(&preview, |out, preview| {
            let creates = if preview.creates_profile {
                " (creates the profile)"
            } else {
                ""
            };
            writeln!(
                out,
                "Import of {} into {}/{} through {}{creates}:",
                preview.request.input.display(),
                preview.request.instance,
                preview.request.profile,
                preview.codec
            )?;
            print_import_items(out, &preview.items)?;
            print_issues(out, &preview.blockers, &preview.warnings)
        })?;
        return Ok(blocked_exit(&preview.blockers));
    }
    let connect = console.connect;
    let report = msbe_pack::execute_import(providers, home, &preview, connect, &Silent)?;
    console.emit(&report, |out, report| {
        writeln!(
            out,
            "Imported {} mod(s) and {} pack-owned file(s) into {}/{} through {}.",
            report.added.len(),
            report.configs,
            report.instance,
            report.profile,
            report.codec
        )
    })?;
    Ok(exit::OK)
}

fn pack_update(
    providers: &Providers,
    home: &Home,
    request: UpdateRequest,
    dry_run: bool,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let preview = msbe_pack::preview_update(providers, home, request)?;
    if dry_run {
        console.emit(&preview, |out, preview| {
            writeln!(
                out,
                "Update of {}/{} from {} to {}:",
                preview.request.instance,
                preview.request.profile,
                preview
                    .previous_version
                    .as_deref()
                    .unwrap_or("an unversioned pack"),
                preview
                    .origin
                    .version
                    .as_deref()
                    .unwrap_or("an unversioned pack")
            )?;
            print_import_items(out, &preview.items)?;
            for change in &preview.changes {
                writeln!(out, "  change      {}", change.id())?;
            }
            for conflict in &preview.conflicts {
                let resolution = match conflict.resolution {
                    Some(Resolution::Keep) => "keep",
                    Some(Resolution::Drop) => "drop",
                    None => "unresolved",
                };
                writeln!(
                    out,
                    "  conflict    {}: {} [{resolution}]",
                    conflict.id, conflict.reason
                )?;
            }
            print_issues(out, &preview.blockers, &preview.warnings)
        })?;
        return Ok(blocked_exit(&preview.blockers));
    }
    let connect = console.connect;
    let report = msbe_pack::execute_update(providers, home, &preview, connect, &Silent)?;
    console.emit(&report, |out, report| {
        writeln!(
            out,
            "Updated {}/{} to {}: {} change(s) reapplied, {} dropped.",
            report.instance,
            report.profile,
            report.version.as_deref().unwrap_or("the new version"),
            report.applied.len(),
            report.dropped.len()
        )
    })?;
    Ok(exit::OK)
}

fn pack_export(
    providers: &Providers,
    home: &Home,
    request: ExportRequest,
    dry_run: bool,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let preview = msbe_pack::preview_export(providers, home, request)?;
    if dry_run {
        console.emit(&preview, |out, preview| {
            writeln!(
                out,
                "Export of {}/{} through {} ({}) to {}:",
                preview.request.instance,
                preview.request.profile,
                preview.request.codec,
                preview.codec_name,
                preview.request.output.display()
            )?;
            for item in &preview.items {
                writeln!(out, "  {:<18} {}", group_label(item.group), item.path)?;
            }
            writeln!(
                out,
                "{} requirement(s), {} environment input(s), {} embedded byte(s).",
                preview.requirements.len(),
                preview.environment.len(),
                preview.embedded_bytes
            )?;
            for observation in &preview.observations {
                writeln!(
                    out,
                    "Relied on {} as observed {}.",
                    observation.subject, observation.observed_at
                )?;
            }
            print_issues(out, &preview.blockers, &preview.warnings)
        })?;
        return Ok(blocked_exit(&preview.blockers));
    }
    let report = msbe_pack::execute_export(providers, home, &preview, &Silent)?;
    console.emit(&report, |out, report| {
        writeln!(
            out,
            "Exported {} entries ({} embedded blob(s), {} requirement(s)) to {}.",
            report.entries,
            report.embedded,
            report.requirements,
            report.output.display()
        )
    })?;
    Ok(exit::OK)
}

fn pack_capture(
    home: &Home,
    request: CaptureRequest,
    dry_run: bool,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let preview = msbe_pack::preview_capture(home, request)?;
    if dry_run {
        console.emit(&preview, |out, preview| {
            if preview.items.is_empty() {
                return writeln!(out, "Nothing to capture.");
            }
            for item in &preview.items {
                let kind = match item.kind {
                    CaptureKind::Changed => "changed",
                    CaptureKind::New => "new",
                };
                writeln!(out, "{kind} {} ({} bytes)", item.path, item.size)?;
                for line in item.diff.iter().flatten() {
                    let marker = match line.kind {
                        DiffKind::Context => ' ',
                        DiffKind::Added => '+',
                        DiffKind::Removed => '-',
                    };
                    writeln!(out, "  {marker}{}", line.text)?;
                }
            }
            Ok(())
        })?;
        return Ok(exit::OK);
    }
    let report = msbe_pack::execute_capture(home, &preview, &Silent)?;
    console.emit(&report, |out, report| {
        writeln!(
            out,
            "Captured {} file(s) into {}/{}.",
            report.captured.len(),
            report.instance,
            report.profile
        )
    })?;
    Ok(exit::OK)
}

/// A generated signing key, and how to trust it.
#[derive(Serialize)]
struct KeyReport {
    signer: String,
    key: PathBuf,
    public_key: String,
    trust: PathBuf,
    trust_entry: String,
}

fn extension_command(
    home: &Home,
    command: &ExtensionCommand,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    match command {
        ExtensionCommand::Keygen { signer, key } => {
            let path = absolute(key)?;
            let generated = SignerKey::generate(signer)?;
            generated.write_new(&path)?;
            let report = KeyReport {
                signer: generated.signer().to_owned(),
                key: path,
                public_key: generated.public_key(),
                trust: trust_file(home),
                trust_entry: generated.trust_entry(&[]),
            };
            console.emit(&report, |out, report| {
                writeln!(
                    out,
                    "Wrote a signing key for {} to {}. Keep it private: anyone who holds it can sign as {}.",
                    report.signer,
                    report.key.display(),
                    report.signer
                )?;
                writeln!(out, "Public key: {}", report.public_key)?;
                writeln!(
                    out,
                    "To trust it, add this entry to {}, with `providers = [...]` naming any provider its codecs bind to:\n",
                    report.trust.display()
                )?;
                write!(out, "{}", report.trust_entry)
            })?;
        }
        ExtensionCommand::Sign {
            module,
            key,
            version,
            id,
        } => {
            let key = SignerKey::read(&absolute(key)?)?;
            let signed = sign_codec(&absolute(module)?, &key, version, id.as_deref())?;
            let install = codecs_directory(home);
            console.emit(&signed, |out, signed| {
                writeln!(
                    out,
                    "Signed codec {} as {} {} by {}, and wrote {}.",
                    signed.codec,
                    signed.id,
                    signed.version,
                    signed.signer,
                    signed.envelope.display()
                )?;
                if let Some(provider) = &signed.provider {
                    writeln!(
                        out,
                        "It binds to provider {provider}, so its signer's trust entry needs providers = [\"{provider}\"]."
                    )?;
                }
                writeln!(
                    out,
                    "Install it by copying the envelope and {} into {}.",
                    signed.module.display(),
                    install.display()
                )
            })?;
        }
        ExtensionCommand::Verify { envelope } => {
            let verified = verify_codec(home, &absolute(envelope)?)?;
            console.emit(&verified, |out, verified| {
                writeln!(
                    out,
                    "{} is codec {} ({} {}), signed by {} and trusted by {}.",
                    verified.envelope.display(),
                    verified.codec,
                    verified.id,
                    verified.version,
                    verified.signer,
                    verified.trust.display()
                )
            })?;
        }
    }
    Ok(exit::OK)
}

fn snapshot_command(
    home: &Home,
    command: &SnapshotCommand,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    match command {
        SnapshotCommand::Create { instance, output } => {
            let report = msbe_pack::create_snapshot(
                home,
                &Name::new(instance)?,
                &absolute(output)?,
                &Silent,
            )?;
            console.emit(&report, |out, report| {
                writeln!(
                    out,
                    "Wrote a snapshot of {} ({} profile(s), {} blob(s)) to {}. Snapshots are private backups, not packs.",
                    report.instance,
                    report.profiles.len(),
                    report.blobs,
                    report.path.display()
                )
            })?;
            Ok(exit::OK)
        }
        SnapshotCommand::Restore { input, dry_run } => {
            let preview = msbe_pack::preview_restore(home, &absolute(input)?)?;
            if *dry_run {
                console.emit(&preview, |out, preview| {
                    writeln!(
                        out,
                        "Restoring would recreate {} for {} with {} profile(s) and {} blob(s).",
                        preview.instance,
                        preview.root.display(),
                        preview.profiles.len(),
                        preview.blobs
                    )?;
                    print_issues(out, &preview.blockers, &[])
                })?;
                return Ok(blocked_exit(&preview.blockers));
            }
            let report = msbe_pack::restore_snapshot(home, &preview, &Silent)?;
            console.emit(&report, |out, report| {
                writeln!(
                    out,
                    "Restored {} with {} profile(s); deploy a profile to apply it.",
                    report.instance,
                    report.profiles.len()
                )
            })?;
            Ok(exit::OK)
        }
    }
}

/// Reads a TOML options file and types its values by `codec`'s schema.
fn options_file(
    providers: &Providers,
    codec: &str,
    direction: Direction,
    path: Option<&Path>,
) -> Result<PackOptions, CliError> {
    let Some(path) = path else {
        return Ok(PackOptions::new());
    };
    let text = fs::read_to_string(path).map_err(|source| CliError::ReadOptions {
        path: path.to_path_buf(),
        source,
    })?;
    let document: toml::Table =
        toml::from_str(&text).map_err(|error| CliError::OptionsDocument {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    Ok(msbe_pack::options_document(
        providers,
        codec,
        direction,
        &serde_json::to_value(document)?,
    )?)
}

fn parse_resolutions(raw: &[String]) -> Result<BTreeMap<String, Resolution>, CliError> {
    raw.iter()
        .map(|entry| match entry.rsplit_once('=') {
            Some((conflict, "keep")) => Ok((conflict.to_owned(), Resolution::Keep)),
            Some((conflict, "drop")) => Ok((conflict.to_owned(), Resolution::Drop)),
            _ => Err(CliError::Resolution(entry.clone())),
        })
        .collect()
}

fn absolute(path: &Path) -> Result<PathBuf, CliError> {
    std::path::absolute(path).map_err(|source| CliError::Path {
        path: path.to_path_buf(),
        source,
    })
}

/// The exit code for a pack failure, by its stable code.
const fn pack_exit(code: IssueCode) -> u8 {
    match code {
        IssueCode::LayerConflict => exit::CONFLICT,
        IssueCode::DistributionForbidden
        | IssueCode::DistributionUnknown
        | IssueCode::UntrustedExtension => exit::POLICY,
        IssueCode::IntegrityMismatch
        | IssueCode::EnvironmentMismatch
        | IssueCode::DerivationMismatch => exit::INTEGRITY,
        _ => exit::FAILURE,
    }
}

/// The exit code a preview reports: success, or the code of its first blocker.
fn blocked_exit(blockers: &[PackIssue]) -> u8 {
    blockers
        .first()
        .map_or(exit::OK, |issue| pack_exit(issue.code))
}

fn print_issues(
    out: &mut dyn Write,
    blockers: &[PackIssue],
    warnings: &[PackWarning],
) -> io::Result<()> {
    for issue in blockers {
        writeln!(out, "blocked: {:?}: {}", issue.code, issue.message)?;
    }
    for warning in warnings {
        writeln!(out, "warning: {}", warning.message)?;
    }
    Ok(())
}

fn print_import_items(out: &mut dyn Write, items: &[ImportItem]) -> io::Result<()> {
    for item in items {
        let action = match item.action {
            ImportAction::InStore => "in store",
            ImportAction::Embedded => "embedded",
            ImportAction::Acquire => "acquire",
            ImportAction::UserAction => "user action",
            ImportAction::Reuse => "reuse",
            ImportAction::Derive => "derive",
            ImportAction::Missing => "missing",
        };
        writeln!(out, "  {action:<11} {}", item.subject)?;
    }
    Ok(())
}

const fn group_label(group: PreviewGroup) -> &'static str {
    match group {
        PreviewGroup::ProviderReference => "reference",
        PreviewGroup::UserAction => "user action",
        PreviewGroup::EnvironmentInput => "environment input",
        PreviewGroup::EmbeddedConfig => "embedded config",
        PreviewGroup::EmbeddedLocal => "embedded local",
        PreviewGroup::EmbeddedOther => "embedded",
        PreviewGroup::Derived => "derived",
        PreviewGroup::PolicyBlocker => "blocked",
        PreviewGroup::Omitted => "omitted",
    }
}

fn open(home: &Home, name: &str, console: &mut Console<'_>) -> Result<Instance, CliError> {
    let instance = Instance::open(home, &Name::new(name)?)?;
    for txn in instance.recovered() {
        writeln!(
            console.err,
            "Recovered interrupted transaction {txn}; it was undone completely."
        )?;
    }
    Ok(instance)
}

fn instance_command(
    home: &Home,
    command: &InstanceCommand,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    match command {
        InstanceCommand::Add {
            name,
            root,
            plan,
            loader,
            loader_version,
            side,
            game_version,
            store,
        } => {
            let name = Name::new(name)?;
            let instance = Instance::create(
                home,
                &NewInstance {
                    name: &name,
                    root,
                    plan,
                    loader,
                    loader_version: loader_version.as_deref(),
                    side: (*side).into(),
                    game_version: game_version.as_deref(),
                    store: store.as_deref(),
                },
            )?;
            console.emit(&instance.status()?, |out, status| {
                writeln!(
                    out,
                    "Added instance {} at {}",
                    status.name,
                    status.root.display()
                )?;
                print_settings(out, status)?;
                writeln!(out, "  profile       {DEFAULT_PROFILE} (empty)")
            })?;
        }
        InstanceCommand::Set {
            name,
            game_version,
            loader_version,
            side,
        } => {
            let mut instance = open(home, name, console)?;
            if let Some(game_version) = game_version {
                instance.set_game_version(Some(game_version))?;
            }
            if loader_version.is_some() || side.is_some() {
                let current_loader_version = instance.config().loader_version.clone();
                let current_side = instance.config().side;
                instance.set_target(
                    loader_version
                        .as_deref()
                        .or(current_loader_version.as_deref()),
                    side.map_or(current_side, Into::into),
                )?;
            }
            console.emit(&instance.status()?, |out, status| {
                writeln!(out, "Updated instance {}", status.name)?;
                print_settings(out, status)
            })?;
        }
        InstanceCommand::List => {
            console.emit(&Instance::list(home)?, |out, names| {
                for name in names {
                    writeln!(out, "{name}")?;
                }
                Ok(())
            })?;
        }
        InstanceCommand::Remove { name } => {
            let name = Name::new(name)?;
            let mut instance = Instance::open(home, &name)?;
            instance.purge()?;
            let path = home.instance(&name);
            #[expect(
                clippy::disallowed_methods,
                reason = "the purged instance's state is MSBE-owned metadata outside the game root"
            )]
            let removed = fs::remove_dir_all(&path);
            removed.map_err(|source| CliError::RemoveInstance { path, source })?;
            console.emit(&name, |out, name| writeln!(out, "Removed instance {name}."))?;
        }
    }
    Ok(exit::OK)
}

fn profile_command(
    home: &Home,
    command: &ProfileCommand,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    match command {
        ProfileCommand::New {
            instance,
            name,
            from,
        } => {
            let opened = open(home, instance, console)?;
            let source = from.as_deref().map(Name::new).transpose()?;
            let name = Name::new(name)?;
            let profile = opened.create_profile(name.clone(), source.as_ref())?;
            console.emit(&name, |out, name| {
                writeln!(
                    out,
                    "Created profile {name} with {} mod(s).",
                    profile.mods.len()
                )
            })?;
        }
        ProfileCommand::Remove { instance, name } => {
            let opened = open(home, instance, console)?;
            let name = Name::new(name)?;
            opened.remove_profile(&name)?;
            console.emit(&name, |out, name| writeln!(out, "Deleted profile {name}."))?;
        }
        ProfileCommand::List { instance } => {
            let opened = open(home, instance, console)?;
            let listing = ProfileListing {
                profiles: opened.profiles()?,
                deployed: opened.deployed_profile().cloned(),
            };
            console.emit(&listing, |out, listing| {
                for profile in &listing.profiles {
                    let marker = if listing.deployed.as_ref() == Some(profile) {
                        "*"
                    } else {
                        " "
                    };
                    writeln!(out, "{marker} {profile}")?;
                }
                Ok(())
            })?;
        }
        ProfileCommand::Show { instance, name } => profile_show(home, instance, name, console)?,
        ProfileCommand::Order {
            instance,
            mods,
            profile,
        } => profile_order(home, instance, profile, mods, console)?,
        ProfileCommand::SetTarget {
            instance,
            name,
            loader,
            loader_version,
            side,
        } => {
            let opened = open(home, instance, console)?;
            let name = Name::new(name)?;
            opened.set_profile_target(
                &name,
                ProfileTarget {
                    loader: loader.clone(),
                    loader_version: loader_version.clone(),
                    side: (*side).into(),
                },
            )?;
            let target = opened.profile_target(&name)?;
            console.emit(&target, |out, target| {
                writeln!(out, "Updated target for {name}:")?;
                writeln!(out, "  loader        {}", target.loader)?;
                writeln!(
                    out,
                    "  loader version {}",
                    target.loader_version.as_deref().unwrap_or("not set")
                )?;
                writeln!(out, "  side          {:?}", target.side)
            })?;
        }
        ProfileCommand::Answer {
            instance,
            module,
            answers,
            profile,
        } => profile_answer(home, instance, module, answers, profile, console)?,
    }
    Ok(exit::OK)
}

/// Prints the mods a profile selects, with its resolved target.
fn profile_show(
    home: &Home,
    instance: &str,
    name: &str,
    console: &mut Console<'_>,
) -> Result<(), CliError> {
    let opened = open(home, instance, console)?;
    let name = Name::new(name)?;
    let mut profile = opened.profile(&name)?;
    profile.target = Some(opened.profile_target(&name)?);
    console.emit(&profile, |out, profile| {
        if profile.mods.is_empty() {
            return writeln!(out, "No mods.");
        }
        for (module, entry) in profile.ordered() {
            let source = entry.provider.as_ref().map_or_else(
                || format!("from {}", entry.origin),
                |provider| format!("{} {}", provider.provider, provider.version_number),
            );
            writeln!(out, "{module}  {} file(s), {source}", entry.files.len())?;
        }
        Ok(())
    })?;
    Ok(())
}

/// Records installer answers given as `STEP/QUESTION=ANSWER`, and prints every answer each named
/// step now has for the mod.
fn profile_answer(
    home: &Home,
    instance: &str,
    module: &str,
    answers: &[String],
    profile: &str,
    console: &mut Console<'_>,
) -> Result<(), CliError> {
    let mut by_step: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for raw in answers {
        let (step, question, answer) = raw
            .split_once('=')
            .and_then(|(key, answer)| {
                let (step, question) = key.split_once('/')?;
                (!step.is_empty() && !question.is_empty()).then_some((step, question, answer))
            })
            .ok_or_else(|| CliError::AnswerSyntax(raw.clone()))?;
        by_step
            .entry(step.to_owned())
            .or_default()
            .insert(question.to_owned(), answer.to_owned());
    }
    let opened = open(home, instance, console)?;
    let profile = Name::new(profile)?;
    let module = Name::new(module)?;
    let mut recorded = BTreeMap::new();
    for (step, answers) in &by_step {
        recorded.insert(
            step.clone(),
            opened.set_answers(&profile, &module, step, answers)?,
        );
    }
    console.emit(&recorded, |out, recorded| {
        writeln!(out, "Recorded answers for {module}:")?;
        for (step, answers) in recorded {
            for (question, answer) in answers {
                writeln!(out, "  {step}/{question} = {answer}")?;
            }
        }
        Ok(())
    })?;
    Ok(())
}

/// Sets the order a profile's mods apply in, and prints the whole order.
fn profile_order(
    home: &Home,
    instance: &str,
    profile: &str,
    mods: &[String],
    console: &mut Console<'_>,
) -> Result<(), CliError> {
    let opened = open(home, instance, console)?;
    let profile = Name::new(profile)?;
    let wanted = mods
        .iter()
        .map(String::as_str)
        .map(Name::new)
        .collect::<Result<Vec<Name>, _>>()?;
    let order = opened.set_order(&profile, &wanted)?;
    console.emit(&order, |out, order| {
        writeln!(out, "{profile} applies its mods in this order:")?;
        for (position, module) in order.iter().enumerate() {
            writeln!(out, "  {}. {module}", position + 1)?;
        }
        Ok(())
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "the established command handler signature needs the catalog alongside command inputs"
)]
fn add(
    providers: &Providers,
    home: &Home,
    instance: &str,
    profile: &str,
    sources: &[String],
    with_deps: bool,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let opened = open(home, instance, console)?;
    let profile = Name::new(profile)?;
    let mut artifacts = Vec::new();
    let mut projects = Vec::new();
    let mut files = Vec::new();
    for source in sources {
        match providers.request(source) {
            Ok(Routed {
                provider,
                request: Request::Project { reference, version },
            }) => projects.push(ProjectRequest {
                provider,
                reference,
                version,
            }),
            Ok(Routed {
                request: Request::File(selection),
                ..
            }) => files.push(*selection),
            Err(RegistryError::Manifest(ManifestError::UnknownSource(_))) => {
                artifacts.push(Artifact {
                    path: PathBuf::from(source),
                    module: None,
                    provider: None,
                    source: None,
                });
            }
            Err(error) => return Err(error.into()),
        }
    }

    let mut report = AddReport::default();
    // Downloads must outlive the ingest below, so the scratch directory lives until the end.
    let scratch = if projects.is_empty() && files.is_empty() {
        None
    } else {
        Some(tempfile::tempdir_in(opened.scratch_dir()).map_err(CliError::Scratch)?)
    };
    if let Some(scratch) = &scratch {
        let existing = opened.profile(&profile)?;
        let client = (console.connect)()?;
        let mut fetch = Fetch {
            providers,
            http: client.as_ref(),
            existing: &existing,
            scratch: scratch.path(),
            artifacts: &mut artifacts,
            report: &mut report,
        };
        if !projects.is_empty() {
            let target = target(&opened, &profile)?;
            let plan = Resolver {
                adapters: providers,
                http: client.as_ref(),
                target: &target,
                overlay: providers.overlay(),
            }
            .plan_install(&projects, with_deps, &installed_releases(&existing))?;
            fetch.plan(plan)?;
        }
        fetch.selections(&files)?;
    }
    report.added = opened.add_artifacts(&profile, &artifacts)?;

    console.emit(&report, |out, report| {
        if report.added.is_empty() {
            writeln!(out, "Nothing new to add to {}/{profile}.", opened.name())?;
        } else {
            writeln!(
                out,
                "Added {} mod(s) to {}/{profile}: {}",
                report.added.len(),
                opened.name(),
                join(&report.added)
            )?;
        }
        if !report.skipped.is_empty() {
            writeln!(out, "Already in the profile: {}", join(&report.skipped))?;
        }
        for substitution in &report.substituted {
            writeln!(
                out,
                "{} requires {}; {} stands in for it.",
                substitution.requirement.declared_by,
                substitution.requirement.package(),
                substitution.supplied_by
            )?;
        }
        print_requirements(
            out,
            &report.unresolved,
            &report.incompatible,
            "add it too, or rerun with --with-deps",
        )
    })?;
    Ok(exit::OK)
}

/// The profile's provider releases, which resolution keeps as they are.
fn installed_releases(profile: &Profile) -> Vec<InstalledRelease> {
    profile
        .mods
        .values()
        .filter_map(|entry| entry.provider.as_ref())
        .map(|provenance| InstalledRelease {
            package: package_of(provenance),
            release: provenance.version.clone(),
        })
        .collect()
}

/// The identity of the project a mod came from.
fn package_of(provenance: &Provenance) -> PackageId {
    PackageId {
        provider: provenance.provider.clone(),
        project: provenance.project.clone(),
    }
}

/// Downloads for `add`, collected as artifacts, skipping projects the profile already has.
struct Fetch<'a> {
    providers: &'a Providers,
    http: &'a dyn HttpClient,
    existing: &'a Profile,
    scratch: &'a Path,
    artifacts: &'a mut Vec<Artifact>,
    report: &'a mut AddReport,
}

impl Fetch<'_> {
    fn plan(&mut self, plan: InstallPlan) -> Result<(), CliError> {
        self.selections(&plan.selections)?;
        self.report.unresolved = plan.unresolved;
        self.report.incompatible = plan.incompatible;
        self.report.substituted = plan.substitutions;
        Ok(())
    }

    fn selections(&mut self, selections: &[Selection]) -> Result<(), CliError> {
        for selection in selections {
            self.selection(selection)?;
        }
        Ok(())
    }

    fn selection(&mut self, selection: &Selection) -> Result<(), CliError> {
        let project = &selection.project.id;
        if let Some(name) = installed_from(self.existing, &project.provider, &project.project) {
            self.report.skipped.push(name.clone());
            return Ok(());
        }
        let adapter = self.providers.adapter(&project.provider)?;
        let acquired = adapter.acquire(self.http, &selection.file, self.scratch)?;
        self.artifacts.push(Artifact {
            provider: Some(adapter.provenance(&selection.release, &acquired)),
            module: selection
                .project
                .slug
                .as_deref()
                .map(Name::sanitize)
                .transpose()?,
            path: acquired.path,
            source: None,
        });
        Ok(())
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the established command handler signature needs the providers alongside command inputs"
)]
fn update(
    providers: &Providers,
    home: &Home,
    instance: &str,
    profile: &str,
    modules: &[String],
    dry_run: bool,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let opened = open(home, instance, console)?;
    let profile = Name::new(profile)?;
    let selection = opened.profile(&profile)?;
    let mut report = UpdateReport {
        dry_run,
        ..UpdateReport::default()
    };
    let tracked = tracked_mods(providers, &selection, &profile, modules, &mut report)?;
    if !tracked.is_empty() {
        let target = target(&opened, &profile)?;
        let client = (console.connect)()?;
        let resolver = Resolver {
            adapters: providers,
            http: client.as_ref(),
            target: &target,
            overlay: providers.overlay(),
        };
        let updates = find_updates(providers, &resolver, &tracked, &mut report)?;
        report_relationships(&resolver, &selection, &updates, &mut report)?;
        if !dry_run && !updates.is_empty() {
            // Downloads must outlive the ingest below.
            let scratch = tempfile::tempdir_in(opened.scratch_dir()).map_err(CliError::Scratch)?;
            let mut artifacts = Vec::with_capacity(updates.len());
            for (module, provenance, update) in &updates {
                let adapter = providers.adapter(&provenance.provider)?;
                let acquired = adapter.acquire(client.as_ref(), &update.file, scratch.path())?;
                artifacts.push(Artifact {
                    provider: Some(adapter.provenance(&update.release, &acquired)),
                    module: Some((*module).clone()),
                    path: acquired.path,
                    source: None,
                });
            }
            opened.replace_artifacts(&profile, &artifacts)?;
        }
    }
    console.emit(&report, |out, report| {
        print_update(out, report, opened.name(), &profile)
    })?;
    Ok(exit::OK)
}

/// The profile's mods to check, limited to `modules` when any are named. Mods in scope that no
/// provider can update are recorded in the report.
fn tracked_mods<'p>(
    providers: &Providers,
    selection: &'p Profile,
    profile: &Name,
    modules: &[String],
    report: &mut UpdateReport,
) -> Result<Vec<Tracked<'p>>, CliError> {
    let wanted = modules
        .iter()
        .map(String::as_str)
        .map(Name::new)
        .collect::<Result<BTreeSet<Name>, _>>()?;
    if let Some(module) = wanted
        .iter()
        .find(|module| !selection.mods.contains_key(*module))
    {
        return Err(InstanceError::UnknownMod {
            profile: profile.clone(),
            module: module.clone(),
        }
        .into());
    }
    let mut tracked = Vec::new();
    for (module, entry) in &selection.mods {
        if !wanted.is_empty() && !wanted.contains(module) {
            continue;
        }
        let updatable = entry.provider.as_ref().filter(|provenance| {
            providers
                .adapter(&provenance.provider)
                .is_ok_and(|adapter| adapter.as_updates().is_some())
        });
        match updatable {
            Some(provenance) => tracked.push((module, provenance)),
            None => report.not_updatable.push(module.clone()),
        }
    }
    Ok(tracked)
}

/// Checks every tracked mod with the provider it came from, returns those with an update, and
/// records the rest.
fn find_updates<'p>(
    providers: &Providers,
    resolver: &Resolver<'_>,
    tracked: &[Tracked<'p>],
    report: &mut UpdateReport,
) -> Result<Vec<Pending<'p>>, CliError> {
    let mut by_provider: BTreeMap<&str, Vec<Tracked<'p>>> = BTreeMap::new();
    for &(module, provenance) in tracked {
        by_provider
            .entry(provenance.provider.as_str())
            .or_default()
            .push((module, provenance));
    }
    let mut checks: BTreeMap<&Name, UpdateCheck> = BTreeMap::new();
    for (provider, group) in by_provider {
        let Some(updates) = providers.adapter(provider)?.as_updates() else {
            continue;
        };
        let installed: Vec<&Provenance> = group.iter().map(|&(_, provenance)| provenance).collect();
        let found = updates.check(resolver.http, &installed, resolver.target)?;
        checks.extend(group.iter().map(|&(module, _)| module).zip(found));
    }
    let mut updates = Vec::new();
    for &(module, provenance) in tracked {
        match checks.remove(module) {
            Some(UpdateCheck::Available(update)) => {
                report.updated.push(ModUpdate {
                    module: module.clone(),
                    from: provenance.version_number.clone(),
                    to: update.release.number.clone(),
                });
                updates.push((module, provenance, *update));
            }
            Some(UpdateCheck::Current) => report.current.push(module.clone()),
            Some(UpdateCheck::Incompatible) => report.no_compatible_version.push(module.clone()),
            Some(UpdateCheck::Unlisted) | None => report.unlisted.push(module.clone()),
        }
    }
    Ok(updates)
}

/// Records requirements the new releases add that the profile does not meet.
fn report_relationships(
    resolver: &Resolver<'_>,
    selection: &Profile,
    updates: &[Pending<'_>],
    report: &mut UpdateReport,
) -> Result<(), CliError> {
    let installed: BTreeSet<PackageId> = selection
        .mods
        .values()
        .filter_map(|entry| entry.provider.as_ref())
        .map(package_of)
        .collect();
    // A requirement is met by the required project, or by one that provides or replaces it.
    let met = |required: &PackageId| {
        installed.contains(required)
            || resolver
                .overlay
                .suppliers(required)
                .any(|supplier| installed.contains(supplier))
    };
    for (module, _, update) in updates {
        let relationships = resolver.relationships(&update.release, module.as_str())?;
        report.unresolved.extend(
            relationships
                .required
                .into_iter()
                .filter(|requirement| !met(&requirement.package())),
        );
        report.incompatible.extend(
            relationships
                .incompatible
                .into_iter()
                .filter(|requirement| installed.contains(&requirement.package())),
        );
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "command handlers receive their parsed inputs separately"
)]
fn search(
    providers: &Providers,
    home: &Home,
    instance: &str,
    profile: &str,
    query: &[String],
    limit: u8,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let opened = open(home, instance, console)?;
    let profile = Name::new(profile)?;
    let target = target(&opened, &profile)?;
    let client = (console.connect)()?;
    let query = query.join(" ");
    let mut hits = Vec::new();
    for provider in providers.searchable() {
        hits.extend(providers.search(provider, client.as_ref(), &query, &target, limit)?);
    }
    console.emit(&hits, |out, hits| {
        if hits.is_empty() {
            return writeln!(out, "No compatible mods found.");
        }
        for hit in hits {
            writeln!(
                out,
                "{}:{:<28} {} ({} downloads)",
                hit.provider, hit.reference, hit.title, hit.downloads
            )?;
            let summary: String = hit.description.chars().take(96).collect();
            writeln!(out, "    {summary}")?;
        }
        Ok(())
    })?;
    Ok(exit::OK)
}

fn remove(
    home: &Home,
    instance: &str,
    profile: &str,
    module: &str,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let opened = open(home, instance, console)?;
    let profile = Name::new(profile)?;
    let module = Name::new(module)?;
    opened.remove_mod(&profile, &module)?;
    console.emit(&module, |out, module| {
        writeln!(
            out,
            "Removed {module} from {}/{profile}. Deploy the profile to apply it.",
            opened.name()
        )
    })?;
    Ok(exit::OK)
}

fn deploy(
    home: &Home,
    instance: &str,
    profile: &str,
    dry_run: bool,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let mut opened = open(home, instance, console)?;
    let profile = Name::new(profile)?;
    if dry_run {
        console.emit(&opened.plan_deploy(&profile)?, print_plan)?;
    } else {
        console.emit(&opened.deploy(&profile, &mut NoopObserver)?, print_deploy)?;
    }
    Ok(exit::OK)
}

fn lock(
    home: &Home,
    instance: &str,
    profile: &str,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let opened = open(home, instance, console)?;
    let profile = Name::new(profile)?;
    let lockfile = opened.write_lockfile(&profile)?;
    console.emit(&lockfile, |out, lockfile| {
        writeln!(
            out,
            "Locked {} module(s) for {} {}.",
            lockfile.mods.len(),
            lockfile.plan.id,
            lockfile.plan.version
        )
    })?;
    Ok(exit::OK)
}

fn rollback(home: &Home, instance: &str, console: &mut Console<'_>) -> Result<u8, CliError> {
    let mut opened = open(home, instance, console)?;
    let undone = RolledBack {
        rolled_back: opened.rollback()?,
    };
    console.emit(&undone, |out, undone| match undone.rolled_back {
        Some(txn) => writeln!(out, "Rolled back transaction {txn}."),
        None => writeln!(out, "Nothing to roll back."),
    })?;
    Ok(exit::OK)
}

fn purge(home: &Home, instance: &str, console: &mut Console<'_>) -> Result<u8, CliError> {
    let mut opened = open(home, instance, console)?;
    let undone = RolledBack {
        rolled_back: opened.purge()?,
    };
    console.emit(&undone, |out, undone| {
        if undone.rolled_back.is_empty() {
            writeln!(out, "Nothing to purge.")
        } else {
            writeln!(
                out,
                "Purged {} transaction(s); {} is back to its state before MSBE.",
                undone.rolled_back.len(),
                opened.name()
            )
        }
    })?;
    Ok(exit::OK)
}

fn verify(home: &Home, instance: &str, console: &mut Console<'_>) -> Result<u8, CliError> {
    let opened = open(home, instance, console)?;
    let report = opened.verify()?;
    console.emit(&report, |out, report| {
        if report.is_clean() && report.changed_at_runtime.is_empty() {
            return writeln!(out, "All {} deployed file(s) match.", report.checked);
        }
        if report.is_clean() {
            return writeln!(
                out,
                "No drift in {} deployed file(s); {} mutable file(s) changed at runtime, as expected.",
                report.checked,
                report.changed_at_runtime.len()
            );
        }
        for path in &report.missing {
            writeln!(out, "missing   {path}")?;
        }
        for path in &report.modified {
            writeln!(out, "modified  {path}")?;
        }
        writeln!(
            out,
            "{} of {} deployed file(s) drifted. Deploy the profile again to repair them.",
            report.missing.len() + report.modified.len(),
            report.checked
        )
    })?;
    Ok(if report.is_clean() {
        exit::OK
    } else {
        exit::INTEGRITY
    })
}

fn status(home: &Home, instance: &str, console: &mut Console<'_>) -> Result<u8, CliError> {
    let opened = open(home, instance, console)?;
    console.emit(&opened.status()?, print_status)?;
    Ok(exit::OK)
}

fn target(instance: &Instance, profile: &Name) -> Result<Target, CliError> {
    let profile_target = instance.profile_target(profile)?;
    let game_version = instance
        .config()
        .game_version
        .clone()
        .ok_or_else(|| CliError::GameVersionRequired(instance.name().clone()))?;
    Ok(Target {
        loader: profile_target.loader.clone(),
        provides: instance.target_provides(&profile_target),
        loader_version: profile_target.loader_version,
        game_version,
        side: profile_target.side,
    })
}

/// The mod in `profile` recorded as `project` from `provider`, if any.
fn installed_from<'p>(profile: &'p Profile, provider: &str, project: &str) -> Option<&'p Name> {
    profile
        .mods
        .iter()
        .find(|(_, entry)| {
            entry.provider.as_ref().is_some_and(|recorded| {
                recorded.provider == provider && recorded.project == project
            })
        })
        .map(|(name, _)| name)
}

fn report(error: &CliError, err: &mut dyn Write) -> io::Result<()> {
    writeln!(err, "error: {error}")?;
    match error {
        CliError::Instance(InstanceError::Conflicts(conflicts)) => {
            for conflict in conflicts {
                writeln!(err, "  {}", conflict.path)?;
                for claim in &conflict.claims {
                    writeln!(err, "    claimed by {} ({})", claim.module, claim.blob)?;
                }
            }
        }
        CliError::Pack(error) => {
            for issue in error.issues() {
                writeln!(err, "  {:?}: {}", issue.code, issue.message)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn print_plan(out: &mut dyn Write, plan: &DeployPlan) -> io::Result<()> {
    let placed = plan
        .operations
        .iter()
        .filter(|operation| matches!(operation, Operation::Materialize { .. }))
        .count();
    writeln!(
        out,
        "Deploying {} would place {placed}, remove {}, and leave {} unchanged.",
        plan.profile,
        plan.operations.len() - placed,
        plan.unchanged
    )?;
    for operation in &plan.operations {
        match operation {
            Operation::Materialize { path, .. } => writeln!(out, "  + {path}")?,
            Operation::Remove { path } => writeln!(out, "  - {path}")?,
            Operation::CreateDir { path } => writeln!(out, "  + {path}/")?,
            Operation::RemoveDir { path } => writeln!(out, "  - {path}/")?,
        }
    }
    print_kept(out, &plan.kept)?;
    print_excluded(out, &plan.excluded)
}

fn print_kept(out: &mut dyn Write, kept: &[RelPath]) -> io::Result<()> {
    for path in kept {
        writeln!(
            out,
            "  kept {path}: it changed on disk, so its mod's new default was not applied"
        )?;
    }
    Ok(())
}

fn print_deploy(out: &mut dyn Write, report: &DeployReport) -> io::Result<()> {
    writeln!(
        out,
        "Deployed {} in transaction {}: placed {}, removed {}, unchanged {}.",
        report.profile, report.txn, report.placed, report.removed, report.unchanged
    )?;
    for (backend, count) in &report.backends {
        writeln!(out, "  {count} file(s) via {}", backend_label(*backend))?;
    }
    if report.removed_dirs > 0 {
        writeln!(
            out,
            "  {} empty directory(ies) MSBE had created were removed",
            report.removed_dirs
        )?;
    }
    print_kept(out, &report.kept)?;
    print_excluded(out, &report.excluded)
}

fn print_update(
    out: &mut dyn Write,
    report: &UpdateReport,
    instance: &Name,
    profile: &Name,
) -> io::Result<()> {
    if report.updated.is_empty() {
        writeln!(out, "No updates for {instance}/{profile}.")?;
    } else {
        let verb = if report.dry_run {
            "Would update"
        } else {
            "Updated"
        };
        writeln!(
            out,
            "{verb} {} mod(s) in {instance}/{profile}:",
            report.updated.len()
        )?;
        for update in &report.updated {
            writeln!(out, "  {}  {} -> {}", update.module, update.from, update.to)?;
        }
        if !report.dry_run {
            writeln!(out, "Deploy the profile to apply the update.")?;
        }
    }
    for (label, names) in [
        ("Up to date", &report.current),
        (
            "No compatible version on their release channel",
            &report.no_compatible_version,
        ),
        ("No longer listed by their provider", &report.unlisted),
        (
            "No provider can update these, left alone",
            &report.not_updatable,
        ),
    ] {
        if !names.is_empty() {
            writeln!(out, "{label}: {}", join(names))?;
        }
    }
    print_requirements(
        out,
        &report.unresolved,
        &report.incompatible,
        "add it before deploying",
    )
}

fn print_requirements(
    out: &mut dyn Write,
    unresolved: &[Requirement],
    incompatible: &[Requirement],
    hint: &str,
) -> io::Result<()> {
    for missing in unresolved {
        writeln!(
            out,
            "{} requires {}; {hint}.",
            missing.declared_by,
            missing.package()
        )?;
    }
    for clash in incompatible {
        writeln!(
            out,
            "Warning: {} declares {} incompatible, and both are selected.",
            clash.declared_by,
            clash.package()
        )?;
    }
    Ok(())
}

fn print_excluded(out: &mut dyn Write, excluded: &[ModExclusion]) -> io::Result<()> {
    if excluded.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "Excluded {} file(s); they stay in the store:",
        excluded.len()
    )?;
    for item in excluded {
        writeln!(
            out,
            "  {}: {} ({})",
            item.module,
            item.file.source,
            reason_label(&item.file.reason)
        )?;
    }
    Ok(())
}

fn print_settings(out: &mut dyn Write, status: &Status) -> io::Result<()> {
    writeln!(
        out,
        "  plan          {} {} (loader {})",
        status.plan_id, status.plan_version, status.loader
    )?;
    writeln!(
        out,
        "  game version  {}",
        status.game_version.as_deref().unwrap_or("not set")
    )?;
    writeln!(
        out,
        "  store         {} ({})",
        status.store.display(),
        backend_label(status.backend)
    )
}

fn print_status(out: &mut dyn Write, status: &Status) -> io::Result<()> {
    writeln!(out, "{} at {}", status.name, status.root.display())?;
    print_settings(out, status)?;
    let deployed = status.deployed_profile.as_ref().map_or_else(
        || "nothing".to_owned(),
        |profile| format!("{profile} ({} file(s))", status.deployed_files),
    );
    writeln!(out, "  deployed      {deployed}")?;
    writeln!(out, "  profiles      {}", join(&status.profiles))?;
    writeln!(out, "  transactions  {} live", status.live_transactions)
}

fn join(names: &[Name]) -> String {
    names
        .iter()
        .map(Name::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

const fn backend_label(backend: Backend) -> &'static str {
    match backend {
        Backend::Reflink => "reflink",
        Backend::Hardlink => "hardlink",
        Backend::Copy => "copy",
    }
}

fn reason_label(reason: &ExclusionReason) -> String {
    match reason {
        ExclusionReason::Hygiene => "hygiene rule".to_owned(),
        ExclusionReason::NotAllowed => "not allowed by the plan".to_owned(),
        ExclusionReason::Denied { pattern } => format!("denied by {pattern}"),
        ExclusionReason::Quarantined { pattern } => format!("quarantined by {pattern}"),
    }
}
