//! The `msbe` command-line interface, built as a library so it can be tested in-process.
//!
//! Results go to stdout, as text or, with `--format json`, as JSON. Diagnostics always go to
//! stderr, so piped output stays clean. See `docs/09-interfaces.md`.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
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
use msbe_fsops::{Backend, NoopObserver, Operation, RelPath, Store};
use msbe_plan_schema::Side;
use msbe_provider_api::{
    AdapterError, HttpClient, HttpError, ManifestError, PackageId, Target, Update, UpdateCheck,
    model::{Request, Selection},
    resolve::{
        InstallPlan, InstalledRelease, ProjectRequest, Requirement, ResolveError, Resolver,
        Substitution,
    },
};
use msbe_providers::{
    Providers, RegistryError, Routed,
    pack::{self, ExportFile, PackError},
};
use serde::Serialize;

#[cfg(test)]
mod end_to_end_tests;

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
    /// Mods in a profile claim the same path with different contents.
    pub const CONFLICT: u8 = 4;
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
    /// Import and export portable modpack manifests.
    #[command(subcommand)]
    Pack(PackCommand),
    /// Add mods to a profile from local files, .zip archives, Modrinth, or https URLs.
    Add {
        /// The instance.
        instance: String,
        /// Files, .zip archives, modrinth:<project>[@<version>] references, or https:// URLs,
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
enum PackCommand {
    /// Export a profile's verified files as a Modrinth .mrpack archive.
    Export {
        /// The instance.
        instance: String,
        /// Destination .mrpack archive.
        #[arg(long, short)]
        output: PathBuf,
        /// The profile to export.
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
        /// The game version, such as 1.21.1. Needed to install from Modrinth.
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
    Pack(#[from] PackError),
    #[error(transparent)]
    Fs(#[from] msbe_fsops::Error),
    #[error(
        "instance {0} has no game version; set one with `msbe instance set {0} --game-version <version>`"
    )]
    GameVersionRequired(Name),
    #[error("cannot create scratch space for downloads: {0}")]
    Scratch(#[source] io::Error),
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
    /// Mods no provider can update. The key predates other providers, and scripts read it.
    not_from_modrinth: Vec<Name>,
    unresolved: Vec<Requirement>,
    incompatible: Vec<Requirement>,
}

#[derive(Serialize)]
struct PackExportReport {
    output: PathBuf,
    files: usize,
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
            if matches!(error, CliError::Instance(InstanceError::Conflicts(_))) {
                exit::CONFLICT
            } else {
                exit::FAILURE
            }
        }
    }
}

fn execute(cli: &Cli, console: &mut Console<'_>) -> Result<u8, CliError> {
    let providers = Providers::builtins()?;
    let home = match &cli.home {
        Some(dir) => Home::at(dir),
        None => Home::discover()?,
    };
    match &cli.command {
        Command::Instance(command) => instance_command(&home, command, console),
        Command::Profile(command) => profile_command(&home, command, console),
        Command::Pack(command) => pack_command(&home, command, console),
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

fn pack_command(
    home: &Home,
    command: &PackCommand,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    match command {
        PackCommand::Export {
            instance,
            output,
            profile,
        } => pack_export(home, instance, output, profile, console),
    }
}

fn pack_export(
    home: &Home,
    instance: &str,
    output: &Path,
    profile: &str,
    console: &mut Console<'_>,
) -> Result<u8, CliError> {
    let opened = open(home, instance, console)?;
    let profile = Name::new(profile)?;
    let lockfile = opened.lockfile(&profile)?;
    let game_version = lockfile
        .target
        .game_version
        .as_deref()
        .ok_or_else(|| CliError::GameVersionRequired(opened.config().name.clone()))?;
    let mut dependencies = BTreeMap::from([("minecraft".to_owned(), game_version.to_owned())]);
    if let Some(loader_version) = &lockfile.target.loader_version {
        dependencies.insert(
            loader_dependency(&lockfile.target.loader),
            loader_version.clone(),
        );
    }
    let store = Store::open(opened.config().store.clone())?;
    let files: Vec<ExportFile> = lockfile
        .deployment
        .into_iter()
        .map(|(path, digest)| ExportFile {
            source: store.blob_path(&digest),
            path,
        })
        .collect();
    pack::export_modrinth(
        output,
        Some(&format!("{}/{}", opened.config().name, profile)),
        dependencies,
        &files,
    )?;
    let report = PackExportReport {
        output: output.to_path_buf(),
        files: files.len(),
    };
    console.emit(&report, |out, report| {
        writeln!(
            out,
            "Exported {} verified file(s) to {}.",
            report.files,
            report.output.display()
        )
    })?;
    Ok(exit::OK)
}

fn loader_dependency(loader: &str) -> String {
    match loader {
        "fabric" => "fabric-loader".to_owned(),
        "quilt" => "quilt-loader".to_owned(),
        _ => loader.to_owned(),
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
                    side.map(Into::into).unwrap_or(current_side),
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
        ProfileCommand::Show { instance, name } => {
            let opened = open(home, instance, console)?;
            let profile = opened.profile(&Name::new(name)?)?;
            console.emit(&profile, |out, profile| {
                if profile.mods.is_empty() {
                    return writeln!(out, "No mods.");
                }
                for (module, entry) in &profile.mods {
                    let source = entry.provider.as_ref().map_or_else(
                        || format!("from {}", entry.origin),
                        |provider| format!("{} {}", provider.provider, provider.version_number),
                    );
                    writeln!(out, "{module}  {} file(s), {source}", entry.files.len())?;
                }
                Ok(())
            })?;
        }
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
    }
    Ok(exit::OK)
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
            let project = &selection.project.id;
            if let Some(name) = installed_from(self.existing, &project.provider, &project.project) {
                self.report.skipped.push(name.clone());
                continue;
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
            });
        }
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
            None => report.not_from_modrinth.push(module.clone()),
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
    if let CliError::Instance(InstanceError::Conflicts(conflicts)) = error {
        for conflict in conflicts {
            writeln!(err, "  {}", conflict.path)?;
            for claim in &conflict.claims {
                writeln!(err, "    claimed by {} ({})", claim.module, claim.blob)?;
            }
        }
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
            &report.not_from_modrinth,
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
