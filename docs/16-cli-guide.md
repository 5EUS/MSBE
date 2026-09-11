# 16 - CLI guide

This guide is the operational reference for `msbe`. It distinguishes commands that work
in the current M1 implementation from the documented CLI surface planned for later
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

The first-party plan currently supports Fabric, Quilt, and NeoForge. It extracts `.jar`
files from an added file or archive and deploys them flat into `mods/`. Loader bootstrap
is not implemented: register a game directory that already has the chosen loader.

### 4.1 Register an instance

```sh
"$MSBE" instance add mc \
  --root "$HOME/Library/Application Support/minecraft" \
  --plan plans/minecraft/plan.toml \
  --loader fabric \
  --game-version 1.21.1
```

Use `--store DIR` to choose a store shard explicitly. Use `instance set` to set or
change the game version used for provider compatibility filtering.

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
"$MSBE" profile list mc
"$MSBE" profile show mc performance
```

### 4.3 Add mods

A source may be a local `.jar`, a `.zip` archive, an HTTPS URL, or a Modrinth project.
A URL can include a required SHA-256 or SHA-512 fragment. Quote URL fragments so the
shell does not interpret `#`.

```sh
"$MSBE" add mc "$HOME/Downloads/example-mod.jar"
"$MSBE" add mc "$HOME/Downloads/modpack-files.zip"
"$MSBE" add mc 'https://example.invalid/mod.jar#sha512=<hex>'
"$MSBE" search mc sodium
"$MSBE" add mc modrinth:sodium
"$MSBE" add mc modrinth:iris --with-deps
```

Use `--profile NAME` or `-p NAME` with `add` to add to a non-default profile.

```sh
"$MSBE" add mc modrinth:sodium --profile performance
```

`--with-deps` follows required Modrinth dependencies. It is not yet a general dependency
solver; conflicting version requirements are M2 work.

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

## 5. Implemented command reference

| Command                                                                                       | Status          | Notes                                         |
| --------------------------------------------------------------------------------------------- | --------------- | --------------------------------------------- |
| `instance add NAME --root DIR --plan FILE --loader ID [--game-version VERSION] [--store DIR]` | **Implemented** | Register an existing game instance.           |
| `instance set NAME --game-version VERSION`                                                    | **Implemented** | Set provider compatibility target.            |
| `instance list`                                                                               | **Implemented** | List registered instances.                    |
| `profile new INSTANCE NAME [--from PROFILE]`                                                  | **Implemented** | Create or copy a profile.                     |
| `profile list INSTANCE`                                                                       | **Implemented** | List profiles and the deployed profile.       |
| `profile show INSTANCE [NAME]`                                                                | **Implemented** | Show selected mods.                           |
| `search INSTANCE QUERY... [--limit N]`                                                        | **Implemented** | Search Modrinth for the instance target.      |
| `add INSTANCE SOURCE... [-p PROFILE] [--with-deps]`                                           | **Implemented** | Add local, archive, URL, or Modrinth content. |
| `update INSTANCE [MOD...] [-p PROFILE] [--dry-run]`                                           | **Implemented** | Update Modrinth mods.                         |
| `remove INSTANCE MOD [-p PROFILE]`                                                            | **Implemented** | Remove a selection from a profile.            |
| `deploy INSTANCE [-p PROFILE] [--dry-run]`                                                    | **Implemented** | Apply a journaled profile diff.               |
| `rollback INSTANCE`                                                                           | **Implemented** | Undo the latest deployment.                   |
| `purge INSTANCE`                                                                              | **Implemented** | Undo all deployment history.                  |
| `verify INSTANCE`                                                                             | **Implemented** | Report deployment drift.                      |
| `status INSTANCE`                                                                             | **Implemented** | Show instance and deployment state.           |

## 6. Planned command surface

These commands are part of the documented product direction, but they are not available
in the current binary. Their names and arguments can change before implementation.

| Area                      | Planned commands                                                      | Target milestone       |
| ------------------------- | --------------------------------------------------------------------- | ---------------------- |
| Instance lifecycle        | `instance detect`, `instance show`, `instance remove`, `instance use` | Future M1/M2 follow-up |
| Profiles                  | `profile switch`, `copy`, `diff`, `export`, `import`, `remove`        | M2                     |
| Resolution                | `lock`, `sync`, `conflicts`, ordering controls                        | M2                     |
| Plans and registry        | `plan`, `registry`                                                    | M7                     |
| Diagnostics and store     | `doctor`, `store`, `bundle`, `bisect`                                 | M2 and later           |
| Daemon control            | `daemon start`, `stop`, `status`; Windows named-pipe transport        | M1 follow-up           |
| Credentials and downloads | `auth`, `download`, Nexus `nxm://`, browser assistance                | M5                     |
| Game launch               | `launch`                                                              | Future                 |

M2 also adds target pre-filtering, PubGrub solving, loader bootstrap, structured config
merge, pack import, lockfiles, and reproducible cross-platform deployment. Later
milestones add legacy Minecraft topologies, a No Man's Sky validation plan, the
acquisition stack, the desktop UI, a signed registry, and Bethesda/KSP support. See
[13 - Roadmap](13-roadmap.md) for the milestone definitions and completion criteria.

## 7. Automation and troubleshooting

Use JSON output for scripts and check the exit code:

```sh
"$MSBE" --format json deploy mc --dry-run
"$MSBE" --format json verify mc
```

The local daemon starts automatically on macOS and Linux. It uses a user-only Unix socket
at `$XDG_RUNTIME_DIR/msbe.sock`, or the temporary-directory fallback when that variable
is unset. When developing from the checkout, build `msbe` and `msbe-daemon` together so
the launcher can find its sibling binary.

For unsupported behavior, consult [09 - Interfaces](09-interfaces.md) for the intended
full interface and [13 - Roadmap](13-roadmap.md) for its implementation status.
