# 16 - CLI guide

This guide is the operational reference for `msbe`. It distinguishes commands that work
in the current M1/M2 implementation from the documented CLI surface planned for later
milestones. Do not rely on a command marked **Planned** in scripts or automation.

## 1. Status and installation

**Current status:** the M1 Minecraft workflow is implemented for local files, `.zip`
archives, HTTPS URLs, and Modrinth. The verified workflow covers instances, profiles,
adds, deployment, verification, rollback, and purge. The daemon runs automatically over
a local Unix socket on macOS and Linux. Windows named-pipe transport is planned but not
implemented.

Build both local binaries before using the working tree:

```sh
cargo build -p msbe-cli -p msbe-daemon
MSBE="$PWD/target/debug/msbe"
```

To install the CLI outside the checkout, build and install both binaries from the same
revision. The automatic daemon launcher expects `msbe-daemon` beside `msbe`.

```sh
cargo install --path crates/msbe-cli
cargo install --path crates/msbe-daemon
```

`msbe --help` is the authoritative syntax for the installed revision.

## 2. Safety model

`msbe` changes a game directory only during `deploy`, `rollback`, or `purge`. Those
operations run through the local daemon and are journaled. Start with `deploy --dry-run`
and use a disposable game copy when evaluating a new plan or mod collection.

State is outside the game directory. Set `MSBE_HOME` or pass `--home DIR` to isolate an
experiment:

```sh
export MSBE_HOME="$HOME/.local/share/msbe-test"
```

On macOS, the default state location is `~/Library/Application Support/msbe`. Store
shards default beside the game directory so hardlink or reflink materialization can be
used when supported by the filesystem.

## 3. Global options

| Option              | Status                            | Meaning                                                                  |
| ------------------- | --------------------------------- | ------------------------------------------------------------------------ |
| `--home DIR`        | **Implemented**                   | Use `DIR` for MSBE state instead of `MSBE_HOME` or the platform default. |
| `--format human`    | **Implemented**                   | Print human-readable results. This is the default.                       |
| `--format json`     | **Implemented**                   | Print machine-readable JSON to stdout.                                   |
| `--dry-run`         | **Implemented where shown below** | Calculate and report changes without applying them.                      |
| `--non-interactive` | **Planned**                       | Never prompt for missing answers.                                        |
| `--answers FILE`    | **Planned**                       | Provide saved wizard answers.                                            |
| `--instance NAME`   | **Planned**                       | Override the active instance.                                            |
| `--profile NAME`    | **Planned**                       | Override the active profile.                                             |

Implemented exit codes are `0` for success, `1` for a general failure, `2` for usage,
`4` for a deploy conflict, and `7` for failed verification. Codes `3`, `5`, and `6` are
planned with their corresponding solver, wizard, and provider-policy features.

## 4. Current Minecraft workflow

The first-party plan supports Fabric, Quilt, NeoForge, Forge, Bukkit, and Paper. Fabric,
Quilt, and NeoForge extract `.jar` files from an added file or archive and deploy them
flat into `mods/`. For Forge 1.6-1.12 packs, regular jars go to `mods/`, jars packaged
beneath `coremods/` go to `coremods/` for ASM transformation, and `config/**/*.cfg` files
go to the mutable `config/` tree so game-created edits are retained. Bukkit and Paper are
server-only targets; their jars deploy flat into `plugins/`. Loader bootstrap is not
implemented: register a game directory that already has the chosen loader.

### 4.1 Register an instance

```sh
"$MSBE" instance add mc \
  --root "$HOME/Library/Application Support/minecraft" \
  --plan plans/minecraft/plan.toml \
  --loader fabric \
  --game-version 1.21.1
```

Use `--store DIR` to choose a store shard explicitly. `--loader-version` and `--side`
set defaults for the initial profile target; `--side` defaults to `client`. Use `instance
set` to change the game version, or those defaults for legacy profiles.

```sh
"$MSBE" instance set mc --game-version 1.21.1
"$MSBE" instance list
"$MSBE" status mc
```

### 4.2 Manage profiles

Every instance begins with an empty `default` profile. A profile becomes active by
deploying it; switching is a diff deployment, not a separate command.

```sh
"$MSBE" profile new mc performance
"$MSBE" profile new mc experiment --from performance
"$MSBE" profile set-target mc performance --loader quilt --side client
"$MSBE" profile list mc
"$MSBE" profile show mc performance
```

Each profile owns its target: loader, optional loader version, and side. The game version
is an instance fact. New profiles begin with the instance defaults, and copied profiles
keep the source target. Provider searches, adds, and updates reject candidates outside this
target before version selection. Quilt declares Fabric as a capability, so a Quilt target
can use Fabric provider releases. Compatibility-layer widening beyond declared loader
capabilities is not implemented.

Where the plan makes order matter, a profile applies its mods in an explicit order, and a later
mod wins where two change the same thing. New mods apply after the ones already there.
`profile order` puts the named mods first, in the order given, and keeps the rest after them:

```sh
"$MSBE" profile order mc modloader optifine
```

**Legacy jarmods.** An instance added with `--loader jarmod --game-version 1.5.2` builds a
separate launcher version, `versions/1.5.2-msbe`, from the vanilla `versions/1.5.2` jar and
version manifest. Those must already be installed, and MSBE only reads them. Each jarmod `.zip`
is injected into the jar in profile order and `META-INF/` is removed; the manifest gets the new
id and no client download, so the launcher does not replace the jar. Select `1.5.2-msbe` in the
launcher to play; `purge` removes the version again.

### 4.3 Add mods

A source may be a local `.jar`, a `.zip` archive, an HTTPS URL, or a Modrinth project.
A URL can include a required SHA-256 or SHA-512 fragment. Quote URL fragments so the
shell does not interpret `#`.

```sh
"$MSBE" add mc "$HOME/Downloads/example-mod.jar"
"$MSBE" add mc "$HOME/Downloads/modpack-files.zip"
"$MSBE" add mc 'https://example.invalid/mod.jar#sha512=<hex>'
"$MSBE" search mc sodium
"$MSBE" search mc sodium --profile performance
"$MSBE" add mc modrinth:sodium
"$MSBE" add mc modrinth:iris --with-deps
```

Use `--profile NAME` or `-p NAME` with `add` to add to a non-default profile.

```sh
"$MSBE" add mc modrinth:sodium --profile performance
```

`--with-deps` collects the full target-compatible Modrinth dependency graph and resolves it
with PubGrub. Exact Modrinth release requirements can backtrack to an older compatible release.
Mods already in the profile stay at their installed release. A requirement is also met by a
mod that provides or replaces the required one, such as Quilted Fabric API for Fabric API; the
report lists these under `substituted`, and adding a second implementation of the same API
fails with an explanation.
Fabric/NeoForge dependency ranges embedded in artifacts are not yet read.

### 4.4 Update, remove, and deploy

```sh
"$MSBE" update mc --dry-run
"$MSBE" update mc sodium
"$MSBE" remove mc sodium --profile performance
"$MSBE" deploy mc --profile performance --dry-run
"$MSBE" deploy mc --profile performance
"$MSBE" verify mc
```

`update` applies only to Modrinth-provenanced mods and preserves the release channel.
`deploy --dry-run` reports placements, removals, unchanged files, and excluded archive
contents. `verify` exits `7` if a deployed file is missing or has changed.

### 4.5 Recover and return to vanilla

```sh
"$MSBE" rollback mc
"$MSBE" purge mc
```

`rollback` undoes the latest deployment. `purge` undoes all deployments and returns the
instance to the state recorded before MSBE first changed it. Mutable paths declared by a
plan preserve runtime changes rather than being overwritten or removed.

## 5. Current No Man's Sky workflow

The local-only No Man's Sky plan accepts local `.pak` files and archives containing them.
It extracts eligible files and deploys them flat to `GAMEDATA/MODS`; acquisition and the
optional pak-check component are not implemented.

```sh
"$MSBE" instance add nms \
  --root "$HOME/Games/No Man's Sky" \
  --plan plans/nomanssky/plan.toml \
  --loader none
"$MSBE" add nms "$HOME/Downloads/example.pak"
"$MSBE" deploy nms
```

## 6. Implemented command reference

| Command                                                                                                                                          | Status          | Notes                                               |
| ------------------------------------------------------------------------------------------------------------------------------------------------ | --------------- | --------------------------------------------------- |
| `instance add NAME --root DIR --plan FILE --loader ID [--loader-version VERSION] [--side client\|server] [--game-version VERSION] [--store DIR]` | **Implemented** | Register an existing game instance.                 |
| `instance set NAME [--game-version VERSION] [--loader-version VERSION] [--side client\|server]`                                                  | **Implemented** | Change instance facts and legacy-profile defaults.  |
| `instance list`                                                                                                                                  | **Implemented** | List registered instances.                          |
| `profile new INSTANCE NAME [--from PROFILE]`                                                                                                     | **Implemented** | Create or copy a profile.                           |
| `profile remove INSTANCE NAME`                                                                                                                   | **Implemented** | Delete an inactive profile and its lockfile.        |
| `profile list INSTANCE`                                                                                                                          | **Implemented** | List profiles and the deployed profile.             |
| `profile show INSTANCE [NAME]`                                                                                                                   | **Implemented** | Show selected mods.                                 |
| `profile order INSTANCE MOD... [-p PROFILE]`                                                                                                     | **Implemented** | Set the order mods apply in.                        |
| `profile set-target INSTANCE [NAME] --loader ID [--loader-version VERSION] --side client\|server`                                                | **Implemented** | Set a profile compatibility target.                 |
| `search INSTANCE QUERY... [-p PROFILE] [--limit N]`                                                                                              | **Implemented** | Search Modrinth for a profile target.               |
| `add INSTANCE SOURCE... [-p PROFILE] [--with-deps]`                                                                                              | **Implemented** | Add local, archive, URL, or Modrinth content.       |
| `update INSTANCE [MOD...] [-p PROFILE] [--dry-run]`                                                                                              | **Implemented** | Update Modrinth mods.                               |
| `remove INSTANCE MOD [-p PROFILE]`                                                                                                               | **Implemented** | Remove a selection from a profile.                  |
| `deploy INSTANCE [-p PROFILE] [--dry-run]`                                                                                                       | **Implemented** | Apply a journaled profile diff.                     |
| `lock INSTANCE [-p PROFILE]`                                                                                                                     | **Implemented** | Write `locks/<profile>.toml` with resolved state.   |
| `pack export INSTANCE --output FILE [-p PROFILE]`                                                                                                | **Implemented** | Export verified files as `.mrpack` overrides.       |
| `pack import INSTANCE FILE [-p PROFILE] [--with-deps]`                                                                                           | **Implemented** | Acquire compatible verified `.mrpack` files.        |
| `bisect start INSTANCE [-p PROFILE]`                                                                                                             | **Implemented** | Create a resumable module bisection session.        |
| `bisect run INSTANCE`                                                                                                                            | **Implemented** | Deploy the current trial subset for manual testing. |
| `bisect result INSTANCE --bad\|--good`                                                                                                           | **Implemented** | Record the trial verdict and select the next half.  |
| `bisect finish INSTANCE`                                                                                                                         | **Implemented** | Restore the source profile and clean up.            |
| `rollback INSTANCE`                                                                                                                              | **Implemented** | Undo the latest deployment.                         |
| `purge INSTANCE`                                                                                                                                 | **Implemented** | Undo all deployment history.                        |
| `verify INSTANCE`                                                                                                                                | **Implemented** | Report deployment drift.                            |
| `status INSTANCE`                                                                                                                                | **Implemented** | Show instance and deployment state.                 |

Modrinth pack import honors each manifest file's `path`. Minecraft plans route JARs to `mods/`
and retain ZIP resource packs and shader packs for `resourcepacks/` and `shaderpacks/` respectively.
The archive `overrides/` directory is not yet imported, so packs that depend on configuration,
scripts, or other overrides remain incomplete.

## 7. Planned command surface

These commands are part of the documented product direction, but they are not available
in the current binary. Their names and arguments can change before implementation.

| Area                      | Planned commands                                                                      | Target milestone          |
| ------------------------- | ------------------------------------------------------------------------------------- | ------------------------- |
| Instance lifecycle        | `instance detect`, `instance show`, `instance remove`, `instance use`                 | Future M1/M2 follow-up    |
| Profiles                  | `profile switch`, `copy`, `diff`, `export`, `import`                                  | M2                        |
| Resolution                | `lock`, `sync`, `conflicts`, ordering controls                                        | M2                        |
| Plans and registry        | `plan`, `registry`                                                                    | M7                        |
| Diagnostics and store     | `doctor`, `store`, `bundle`                                                           | M2 and later              |
| Daemon control            | `daemon start`, `stop`, `status`; Windows named-pipe transport                        | M1 follow-up              |
| Credentials and downloads | `auth`, `download`, Nexus `nxm://`, browser assistance                                | M5                        |
| Steam Workshop            | Opt-in user-installed SteamCMD acquisition or local import; optional item-ID metadata | Future, subject to policy |
| Game launch               | `launch`                                                                              | Future                    |

M2 also adds loader bootstrap, structured config merge, pack import, lockfiles, and reproducible
cross-platform deployment. Target pre-filtering and the Modrinth PubGrub graph solver are
implemented; artifact-derived Fabric/NeoForge range metadata remains pending.
Later
milestones add legacy Minecraft topologies, a No Man's Sky validation plan, the
acquisition stack, the desktop UI, a signed registry, and Bethesda/KSP support. See
[13 - Roadmap](13-roadmap.md) for the milestone definitions and completion criteria.

### 7.1 Planned Steam Workshop acquisition and imports

Steam Workshop acquisition is **not implemented**. The planned opt-in adapter may invoke a
SteamCMD binary supplied by the user to acquire content their account is entitled to receive.
MSBE will not bundle or modify SteamCMD, retain Steam credentials, implement Steam-client or
depot protocols, access manifests, or bypass entitlement, subscription, rate-limit, or
content-owner controls.

MSBE will also accept a user-selected local file, archive, or directory obtained through the
Steam client or another tool the user chooses. It may record a public Workshop URL or item ID as
display-only provenance. See
[06 - Providers & policy](06-providers-and-policy.md#65-steam-workshop-steamcmd-or-user-supplied-content)
for the binding policy boundary.

## 8. Automation and troubleshooting

Use JSON output for scripts and check the exit code:

```sh
"$MSBE" --format json deploy mc --dry-run
"$MSBE" --format json verify mc
```

The local daemon starts automatically on macOS and Linux. It uses a user-only Unix socket
at `$XDG_RUNTIME_DIR/msbe.sock`, or the temporary-directory fallback when that variable
is unset. When developing from the checkout, build `msbe` and `msbe-daemon` together so
the launcher can find its sibling binary.

The built-in provider catalog recognizes the `modrinth:` source prefix and HTTPS direct URLs.
Provider manifests are versioned and constrained configuration, not downloaded code: they can
describe source recognition, HTTPS metadata origins, policy, and a supported acquisition
primitive. M1 resolves them only through reviewed built-in adapters and rejects providers that
require authentication or a persisted policy acknowledgement, because those workflows are not
implemented yet. See [06 - Providers & policy](06-providers-and-policy.md#63-provider-manifests)
for the contribution and safety model.

For unsupported behavior, consult [09 - Interfaces](09-interfaces.md) for the intended
full interface and [13 - Roadmap](13-roadmap.md) for its implementation status.
