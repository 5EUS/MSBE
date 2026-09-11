# MSBE (Modding Should be Easy)

A cross-platform, **game-agnostic** mod manager with a GUI and a first-class CLI.

Most mod managers are built for one game family and encode that game's assumptions
into the tool itself. MSBE inverts that: the tool knows nothing about any game, and
every game's install behaviour is described by a **Plan** — a declarative, signed,
community-contributable document that composes a small, closed vocabulary of steps.

Stack: **Rust** core + daemon + CLI, **C# / Avalonia 12 (NativeAOT)** desktop UI.

## Planning documents

| Doc | Contents |
|---|---|
| [00 — Overview & non-goals](docs/00-overview.md) | What this is, what it refuses to be, prior art |
| [01 — Domain model](docs/01-domain-model.md) | Game, Instance, Profile, Mod, Artifact, Lockfile |
| [02 — The Plan system](docs/02-plan-system.md) | Install-topology axes, manifest schema, step vocabulary, WASM ABI |
| [03 — Architecture](docs/03-architecture.md) | Processes, crates/projects, RPC, AOT constraints |
| [04 — Deployment engine](docs/04-deployment-engine.md) | CAS, materialization backends, journal, rollback, GC |
| [05 — Solver](docs/05-solver.md) | PubGrub, version semantics per ecosystem, error reporting |
| [06 — Providers & policy](docs/06-providers-and-policy.md) | Nexus/Modrinth/CurseForge/Thunderstore, ToS matrix |
| [07 — Browser & secrets](docs/07-browser-and-secrets.md) | Integrated browser, nxm://, assisted downloads, keychain |
| [08 — Platforms & detection](docs/08-platforms-and-detection.md) | Steam/GOG/Epic/Xbox, Proton/Wine, Flatpak |
| [09 — Interfaces](docs/09-interfaces.md) | CLI surface, UI surface, headless/server mode |
| [10 — Registry](docs/10-registry.md) | Plan + overlay metadata repo, signing, contribution flow |
| [11 — Security](docs/11-security.md) | Threat model, archive hardening, sandbox, supply chain |
| [12 — Testing & release](docs/12-testing-and-release.md) | Fixtures, property tests, CI matrix, packaging, updates |
| [13 — Roadmap](docs/13-roadmap.md) | Milestones M0–M8 |
| [14 — Risks & open questions](docs/14-risks.md) | What could sink this, and what still needs deciding |
| [15 — M0 findings](docs/15-m0-findings.md) | De-risking record: what was proven, the C# UI go decision, design changes |

## Building

```sh
sh scripts/development/install-git-hooks.sh   # core.hooksPath = .githooks
sh scripts/development/setup-vscode.sh        # optional: VS Code launch/tasks/settings
cargo build --workspace                       # Rust: core, daemon, CLI
dotnet build dotnet/MSBE.slnx                 # .NET: Avalonia desktop + RPC client
sh scripts/development/check.sh               # everything CI checks, locally
```

Toolchains are pinned: Rust in `rust-toolchain.toml`, the .NET SDK in
`dotnet/global.json`. Bump either in its own commit.

## Trying it

```sh
cargo install --path crates/msbe-cli
msbe instance add mc --root ~/.minecraft --plan plans/minecraft/plan.toml \
  --loader fabric --game-version 1.21.1
msbe search mc shaders                 # Modrinth, filtered to the loader and game version
msbe add mc modrinth:iris --with-deps  # verified downloads, dependencies included
msbe add mc ~/Downloads/my-mod.jar     # local files and .zip archives work too
msbe add mc 'https://example.com/mod.jar#sha256=...'  # direct https URLs, checksum-pinned
msbe update mc --dry-run               # newer compatible versions, same release channel
msbe deploy mc --dry-run   # what would change, and every file left out and why
msbe deploy mc             # one journaled transaction
msbe verify mc             # exits 7 if a deployed file changed or disappeared
msbe purge mc              # back to exactly how the directory was before MSBE
```

State lives in `$MSBE_HOME` (by default the platform data directory), never in the game
directory. `--format json` makes every command scriptable.

## The compile-time posture

Almost every rule in this repo exists because of a specific failure mode, and each is
enforced by the compiler rather than by review. `CONTRIBUTING.md` explains every one;
the short version:

- **Rust** — workspace lints deny `clippy::all`, `pedantic` and `cargo`, plus
  `unwrap_used` / `panic` / `indexing_slicing` (a panic mid-deploy is a half-applied
  transaction) and `unsafe_code`. `clippy.toml` bans `HashMap` / `HashSet` (iteration
  order is nondeterministic, and lockfiles are compared byte-for-byte), the unjournaled
  `fs::remove_*` and `fs::rename` calls, `SystemTime::now`, and `Command::new`.
  Exceptions are stated at the call site with `#[expect(..., reason = "...")]`;
  `#[allow]` is itself denied, so stale suppressions cannot accumulate.
- **C#** — NativeAOT posture from the first commit: trim and AOT analyzers,
  `TreatWarningsAsErrors`, compiled bindings only, source-generated JSON.
  `BannedSymbols.txt` rejects reflection instantiation, `System.Reflection.Emit`,
  reflection-based `JsonSerializer`, `Task.Result` / `.Wait()` and `DateTime.Now`.
  `Directory.Build.targets` fails the build (`MSBE0001`–`MSBE0005`) if any project opts
  out of the posture, and CI publishes all six RIDs on every PR because AOT breakage is
  invisible to `dotnet build`.

## Status

Scaffolding and planning. The Rust workspace builds and lints clean; there is no
behaviour yet. `MSBE` is a working codename.

See [13 — Roadmap](docs/13-roadmap.md) for what lands when.
