# Contributing

## Setup

```sh
sh scripts/development/install-git-hooks.sh   # core.hooksPath = .githooks
sh scripts/development/setup-vscode.sh        # optional: launch, tasks, settings, extensions
cargo build --workspace                       # Rust half
dotnet build dotnet/MSBE.slnx                 # .NET half (needs the SDK in dotnet/global.json)
```

`.vscode/` is gitignored except `extensions.json`, so `setup-vscode.sh` is the
committed source of truth. It never overwrites a file you have customised unless you
pass `--force`, which backs the original up to `.bak` first. The generated tasks mirror
CI (`check: everything`), and rust-analyzer runs clippy with the workspace lints, so the
editor and CI never disagree about what is an error.

Run everything CI runs, locally, before opening a PR:

```sh
sh scripts/development/check.sh
```

## The compile-time posture

This project pushes as much review burden as possible into the compiler, because a
rule a human has to remember is a rule that gets skipped at 1am. Almost nothing here
is a style preference; each rule exists because of a specific failure mode.

### Rust

Lints are defined once in the workspace `Cargo.toml` and inherited by every crate.
An exception is stated **at the call site** with `#[expect(lint, reason = "...")]`,
never by loosening the workspace table. `clippy::allow_attributes` is denied so that
`#[allow]` is not an option: `#[expect]` fails the build if the lint stops firing, so
stale suppressions cannot accumulate.

| Rule | Why |
|---|---|
| `unwrap_used`, `expect_used`, `panic`, `indexing_slicing` | A panic mid-deploy is a half-applied transaction. Errors propagate; they do not abort. |
| `unsafe_code = "deny"`, `forbid` in `msbe-archive` | Archive parsing is the hostile-input boundary. |
| `disallowed-types`: `HashMap`, `HashSet` | Iteration order is nondeterministic, and lockfiles and `OperationSet`s are compared byte-for-byte. Use `BTreeMap`/`IndexMap`. |
| `disallowed-methods`: `fs::remove_file`, `fs::remove_dir_all`, `fs::rename` | Destructive and unjournaled. Emit an operation and let the applier run it, so it can be rolled back. |
| `disallowed-methods`: `SystemTime::now` | Plan evaluation must be deterministic and lockfiles must not embed wall-clock time. Inject a clock. |
| `disallowed-methods`: `process::Command::new` | Spawning a process is a capability, not an ambient ability. |
| `disallowed-methods`: `env::var`, `env::var_os` | Configuration is read in one place, `msbe_core::config`, so precedence and platform defaults cannot fork. |
| `print_stdout`, `print_stderr` | Libraries log; they do not print. The two binaries opt in with a reason. |

### C#

| Rule | Why |
|---|---|
| `PublishAot`, trim/AOT/single-file analyzers, `TreatWarningsAsErrors` | AOT breakage is invisible to `dotnet build` and only appears at publish. CI publishes every RID for this reason. |
| Compiled bindings only (`x:DataType`) | Reflection bindings fail at runtime under NativeAOT. `scripts/development/check-xaml.sh` fails the build for a `{Binding}` in a view with no `x:DataType`. |
| `BannedSymbols.txt` | Reflection instantiation, `System.Reflection.Emit`, reflection-based `JsonSerializer`, `Task.Result`/`.Wait()`, `DateTime.Now`, MD5/SHA1. |
| `Directory.Build.targets` assertions | A single `.csproj` cannot quietly opt out of the posture; doing so is `MSBE0001`–`MSBE0005`. |
| Central package management + lock files | CI restores with `--locked-mode`, so dependency drift is a diff rather than a surprise. |

## Conventions

**Rust** — one crate per bounded responsibility; `msbe-core` must compile and pass its
tests with **zero plans installed**. A game, store, loader or file-format name appearing
in `msbe-core` is a review blocker.

**C#** — mirrors the layout that works well in `RoofingOrderTracker2`:

- Large view models split into feature partials: `MainViewModel.cs` holds shell-wide
  state only, and each surface gets `MainViewModel.<Feature>.cs`. A member belongs in
  the partial named for the surface it serves.
- Views under `Views/{Shell,Pages,Components}`, each a paired `X.axaml` + `X.axaml.cs`.
  Pages are `*PageView`; reusable controls are `*View` and expose `StyledProperty`.
- Styles live in `Styles/AppStyles.axaml`, not scattered through views, so a visual
  change is one file and one review.
- XML docs on every public and internal member; `this.` qualification; `using`
  directives outside the namespace, `System` first.

## Checking Windows from Linux

`ring`, which `msbe-http` uses for TLS, needs the MSVC toolchain to build for Windows, so a
Linux machine cannot lint the crates that depend on it for that target. The others can be:

```sh
rustup target add x86_64-pc-windows-msvc
cargo clippy -p msbe-fsops -p msbe-archive -p msbe-plan-schema -p msbe-core \
  -p msbe-provider-api -p msbe-provider-modrinth -p msbe-providers \
  --all-targets --all-features --target x86_64-pc-windows-msvc -- -D warnings
```

CI's Windows runners build and test everything, including `msbe-http` and `msbe-cli`.

## Commits and PRs

Toolchain bumps (`rust-toolchain.toml`, `global.json`, analyzer versions) go in their
own commit. New lints landing alongside new logic makes a diff unreviewable.
