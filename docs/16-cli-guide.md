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

The game version is optional: without one, a provider that filters by game version offers
releases for every version. For a game whose plan declares editions or storefronts, `--edition`
and `--storefront` name the installation's; each must be declared by the plan and supported by
the loader, and providers that list them filter by them.

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

**Installer answers.** A plan's `run-extension` step can ask questions about a mod, such as which
optional files to install ([18](18-wasm-extensions.md)). Deployment answers them from answers
recorded on the mod, or each question's default, and fails listing any question that has neither.
Record answers by step and question; an empty answer removes one:

```sh
"$MSBE" profile answer mc better-textures install/textures=high
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

`update` applies to mods from a provider with an update protocol, such as Modrinth and
Thunderstore, and preserves the release channel. A file its provider does not let MSBE download,
such as one its author keeps off third-party tools, is never fetched: `add` and `update` fail
with the policy exit code and name the page to download it from, and the saved file can then be
added as a local file.
`deploy --dry-run` reports placements, removals, unchanged files, and excluded archive
contents. `verify` exits `7` if a deployed file is missing or has changed.

### 4.5 Recover and return to vanilla

```sh
"$MSBE" journal mc
"$MSBE" rollback mc
"$MSBE" rollback mc --to 3
"$MSBE" purge mc
```

`journal` lists the deployments still in effect, newest first. `rollback` undoes the latest
deployment, and `rollback --to` undoes every deployment after the one named, so that one is in
effect again. `purge` undoes all deployments and returns the
instance to the state recorded before MSBE first changed it. Mutable paths declared by a
plan preserve runtime changes rather than being overwritten or removed.

## 5. Current No Man's Sky and Blade & Sorcery workflows

### 5.1 No Man's Sky

The local-only No Man's Sky plan accepts local files and archives, preserving each
non-hygiene source path beneath `GAMEDATA/MODS`. This supports generated `.pak` files and
AMUMSS Lua, EXML, MBIN, and asset source trees. Acquisition and the optional pak-check
component are not implemented.

```sh
"$MSBE" instance add nms \
  --root "$HOME/Games/No Man's Sky" \
  --plan plans/nomanssky/plan.toml \
  --loader none
"$MSBE" add nms "$HOME/Downloads/example-mod.zip"
"$MSBE" deploy nms
```

Text documentation and AMUMSS status files are quarantined and reported in the deployment
preview, rather than copied into the game's mod directory.

### 5.2 Blade & Sorcery

The Blade & Sorcery plan places each mod folder beneath
`BladeAndSorcery_Data/StreamingAssets/Mods`, where the game loads it. An archive may carry the mod
folder at its root, inside `Mods/`, or inside the full `BladeAndSorcery_Data/StreamingAssets/Mods/`
path; each lands the same. An archive whose `manifest.json` sits at its root names no folder and is
not supported. The plan covers the PC VR game; the Nomad edition keeps mods on the headset.

```sh
"$MSBE" instance add bas \
  --root "$HOME/.local/share/Steam/steamapps/common/Blade & Sorcery" \
  --plan plans/bladeandsorcery/plan.toml \
  --loader native
"$MSBE" add bas "$HOME/Downloads/example-mod.zip"
"$MSBE" deploy bas
```

Text documentation is quarantined and reported in the deployment preview. An installed provider
program that lists `bladeandsorcery` among its games can also queue downloads for the instance.

## 6. Implemented command reference

| Command                                                                                                                                          | Status          | Notes                                                                       |
| ------------------------------------------------------------------------------------------------------------------------------------------------ | --------------- | --------------------------------------------------------------------------- |
| `instance add NAME --root DIR --plan FILE --loader ID [--loader-version VERSION] [--side client\|server] [--game-version VERSION] [--edition ID] [--storefront ID] [--store DIR]` | **Implemented** | Register an existing game instance.                                         |
| `instance set NAME [--game-version VERSION] [--edition ID] [--storefront ID] [--loader-version VERSION] [--side client\|server]`                   | **Implemented** | Change instance facts and legacy-profile defaults.                          |
| `instance list`                                                                                                                                  | **Implemented** | List registered instances.                                                  |
| `instance remove NAME`                                                                                                                           | **Implemented** | Restore managed files and remove MSBE instance data; keeps the game folder. |
| `profile new INSTANCE NAME [--from PROFILE]`                                                                                                     | **Implemented** | Create or copy a profile.                                                   |
| `profile remove INSTANCE NAME`                                                                                                                   | **Implemented** | Delete an inactive profile and its lockfile.                                |
| `profile list INSTANCE`                                                                                                                          | **Implemented** | List profiles and the deployed profile.                                     |
| `profile show INSTANCE [NAME]`                                                                                                                   | **Implemented** | Show selected mods.                                                         |
| `profile order INSTANCE MOD... [-p PROFILE]`                                                                                                     | **Implemented** | Set the order mods apply in.                                                |
| `profile set-target INSTANCE [NAME] --loader ID [--loader-version VERSION] --side client\|server`                                                | **Implemented** | Set a profile compatibility target.                                         |
| `profile answer INSTANCE MOD STEP/QUESTION=ANSWER... [-p PROFILE]`                                                                               | **Implemented** | Record installer answers for a mod's run-extension step.                    |
| `search INSTANCE QUERY... [-p PROFILE] [--limit N]`                                                                                              | **Implemented** | Search Modrinth for a profile target.                                       |
| `add INSTANCE SOURCE... [-p PROFILE] [--with-deps]`                                                                                              | **Implemented** | Add local, archive, URL, or Modrinth content.                               |
| `update INSTANCE [MOD...] [-p PROFILE] [--dry-run]`                                                                                              | **Implemented** | Update Modrinth mods.                                                       |
| `remove INSTANCE MOD [-p PROFILE]`                                                                                                               | **Implemented** | Remove a selection from a profile.                                          |
| `deploy INSTANCE [-p PROFILE] [--dry-run]`                                                                                                       | **Implemented** | Apply a journaled profile diff.                                             |
| `lock INSTANCE [-p PROFILE]`                                                                                                                     | **Implemented** | Write `locks/<profile>.toml` with resolved state.                           |
| `pack config list INSTANCE [-p PROFILE]`                                                                                                         | **Implemented** | List pack-owned config paths and exact digests.                             |
| `pack config show INSTANCE PATH [-p PROFILE]`                                                                                                    | **Implemented** | Read a pack-owned text config.                                              |
| `pack config set INSTANCE PATH (--content TEXT\|--file FILE) [-p PROFILE]`                                                                       | **Implemented** | Add or replace a pack-owned config.                                         |
| `pack config remove INSTANCE PATH [-p PROFILE]`                                                                                                  | **Implemented** | Remove a pack-owned config.                                                 |
| `pack validate INSTANCE [-p PROFILE]`                                                                                                            | **Implemented** | Validate and write the canonical lockfile.                                  |
| `pack formats [--direction import\|export] [--game GAME]`                                                                                        | **Implemented** | List the formats reviewed codecs provide.                                   |
| `pack options CODEC [--preset PRESET] [--direction import\|export]`                                                                              | **Implemented** | Show a codec's option schema and normalized values.                         |
| `pack export INSTANCE OUTPUT --codec CODEC [--preset PRESET] [--options FILE] [-p PROFILE] [--dry-run]`                                          | **Implemented** | Preview and write a policy-gated pack.                                      |
| `pack import INSTANCE INPUT [--codec CODEC] [--options FILE] [-p PROFILE] [--dry-run]`                                                           | **Implemented** | Import a pack into a new or empty profile as its pack layer.                |
| `pack update INSTANCE INPUT [--codec CODEC] [-p PROFILE] [--resolve CONFLICT=keep\|drop]... [--dry-run]`                                         | **Implemented** | Replace the pack layer and reapply the profile's changes.                   |
| `pack capture INSTANCE [-p PROFILE] [--path PATH]... [--dry-run]`                                                                                | **Implemented** | Adopt in-game changes beneath the plan's mutable roots.                     |
| `snapshot create INSTANCE OUTPUT`                                                                                                                | **Implemented** | Back up instance state and every referenced blob; never distributable.      |
| `snapshot restore INPUT [--dry-run]`                                                                                                             | **Implemented** | Restore a snapshot's MSBE state; deploy separately.                         |
| `bisect start INSTANCE [-p PROFILE]`                                                                                                             | **Implemented** | Create a resumable module bisection session.                                |
| `bisect run INSTANCE`                                                                                                                            | **Implemented** | Deploy the current trial subset for manual testing.                         |
| `bisect result INSTANCE --bad\|--good`                                                                                                           | **Implemented** | Record the trial verdict and select the next half.                          |
| `bisect finish INSTANCE`                                                                                                                         | **Implemented** | Restore the source profile and clean up.                                    |
| `rollback INSTANCE [--to TXN]`                                                                                                                   | **Implemented** | Undo the latest deployment, or every deployment after TXN.                  |
| `journal INSTANCE`                                                                                                                               | **Implemented** | List the deployments still in effect, newest first.                         |
| `conflicts INSTANCE [-p PROFILE]`                                                                                                                | **Implemented** | Name the mods that place different contents at one path.                    |
| `purge INSTANCE`                                                                                                                                 | **Implemented** | Undo all deployment history.                                                |
| `verify INSTANCE`                                                                                                                                | **Implemented** | Report deployment drift.                                                    |
| `status INSTANCE`                                                                                                                                | **Implemented** | Show instance and deployment state.                                         |
| `extension keygen SIGNER KEY_FILE`                                                                                                               | **Implemented** | Create a private signing key and print the trust entry for it.              |
| `extension sign INPUT --key KEY_FILE --version VERSION [--id ID] [--output DIRECTORY]`                                                           | **Implemented** | Sign a WebAssembly pack codec or a provider program, writing its envelope.  |
| `extension verify ENVELOPE`                                                                                                                      | **Implemented** | Check a signed codec or program against the local trust root.               |
| `extension list`                                                                                                                                 | **Implemented** | List installed codecs and programs, and why any is refused.                 |
| `download add INSTANCE SOURCE... [-p PROFILE] [--with-deps]`                                                                                     | **Implemented** | Queue provider content in the daemon to download and add.                   |
| `download list`                                                                                                                                  | **Implemented** | Show the queue and what each download waits for.                            |
| `download pause [ID]`, `download resume [ID]`                                                                                                    | **Implemented** | Hold or release one download, or the whole queue.                           |
| `download cancel ID`, `download retry ID`, `download move ID POSITION`                                                                           | **Implemented** | Cancel, retry or reorder one download.                                      |
| `download confirm ID INSTANCE [-p PROFILE]`                                                                                                      | **Implemented** | Choose the profile for a download a link started.                           |
| `download clear`                                                                                                                                 | **Implemented** | Remove completed, failed and cancelled downloads.                           |
| `handoff URI`                                                                                                                                    | **Implemented** | Submit a provider link to the download queue.                               |
| `handler status [SCHEME]`                                                                                                                        | **Implemented** | Show which application opens provider links.                                |
| `handler register SCHEME [--replace]`                                                                                                            | **Implemented** | Open a scheme's links with MSBE; never silently.                            |
| `handler unregister SCHEME`                                                                                                                      | **Implemented** | Give a scheme back to the application MSBE replaced.                        |
| `browser status`                                                                                                                                 | **Implemented** | Show the MSBE browser and how many downloads wait on a page.                |
| `browser open [ID] [--auto-advance\|--no-auto-advance]`                                                                                          | **Implemented** | Send the MSBE browser to a waiting page; it captures what the page hands over.|
| `browser close`                                                                                                                                  | **Implemented** | Close the MSBE browser.                                                     |
| `tool list`                                                                                                                                      | **Implemented** | List tool providers and the program registered for each.                    |
| `tool register PROVIDER PROGRAM --accept-terms`                                                                                                  | **Implemented** | Register an installed tool program by SHA-256 and accept the terms.         |
| `tool forget PROVIDER`                                                                                                                           | **Implemented** | Forget the program registered for a tool provider.                          |
| `auth status`                                                                                                                                    | **Implemented** | Show sign-in, terms and quota for providers that need them.                 |
| `auth acknowledge PROVIDER`                                                                                                                      | **Implemented** | Accept a provider's current terms.                                          |
| `auth login PROVIDER --token-from-stdin\|--token-file FILE`                                                                                      | **Implemented** | Check a key with its provider and keep it.                                  |
| `auth logout PROVIDER`                                                                                                                           | **Implemented** | Forget the key kept for a provider.                                         |

Pack commands are codec-driven: `pack formats` and `pack options` list what the reviewed registry
provides, and the CLI names no format itself. Every pack command previews first. `--dry-run`
prints the complete preview, including each file's group, requirements, environment inputs,
blockers and warnings, and exits with its first blocker's code: 6 for a distribution refusal, 7 for
an integrity or environment mismatch, 4 for a layer conflict. Without `--dry-run` that preview
runs, and a preview with blockers writes nothing.

```sh
"$MSBE" pack export mc "$HOME/mc.msbepack" --codec msbe-native --preset portable --dry-run
"$MSBE" pack import mc "$HOME/modpack.mrpack" --profile modpack
"$MSBE" pack update mc "$HOME/modpack-2.mrpack" --profile modpack --resolve mod:sodium=keep
"$MSBE" pack capture mc --profile modpack --dry-run
"$MSBE" snapshot create mc "$HOME/mc.msbesnapshot"
```

Export presets are `thin`, `portable`, `complete` and `public-distribution`; `--options` reads a
TOML table typed by the codec's schema. Import records the pack as the profile's pack layer, and
later `add`, `remove` and config changes form the changes layer that `pack update` reapplies. A
change the new version invalidates must be resolved before the update runs. Modrinth pack import
honors each manifest file's `path`; the archive `overrides/` directory is not yet imported. A
snapshot is a private backup, not a pack: `pack import` refuses one. See
[17 - Pack formats and native bundles](17-pack-formats-and-native-bundles.md).

Extension commands publish WebAssembly pack codecs and provider programs
([18 §18.3](18-wasm-extensions.md)). A publisher creates a key once, signs each release, and shares
the trust entry `keygen` prints. A program also needs `programs = ["<provider id>"]` in that entry,
which `sign` prints. A user adds the entry to `extensions/trust.toml` in the MSBE home and checks
the extension with `verify`. They then copy a codec's envelope and module into `extensions/codecs/`,
or a program's envelope into `extensions/providers/`. `list` shows what is installed. An extension
that fails a check is skipped with its reason, and never stops the others. Pass absolute paths,
since the daemon resolves them.

```sh
"$MSBE" extension keygen example-publisher "$HOME/.msbe-keys/example-publisher.toml"
"$MSBE" extension sign "$PWD/pack-list.wasm" --key "$HOME/.msbe-keys/example-publisher.toml" --version 1.0.0
"$MSBE" extension verify "$PWD/pack-list.toml"
"$MSBE" extension sign "$PWD/program.toml" --key "$HOME/.msbe-keys/example-publisher.toml" --version 0.1.0 --output "$PWD/signed"
"$MSBE" extension verify "$PWD/signed/example.toml"
"$MSBE" extension list
```

A key file is created readable only by its owner and is never replaced; `sign` refuses a key file
other users can read. `sign` loads the module in the sandbox first, so only a module MSBE can run as a
codec is signed.

Download commands run in the daemon, which owns the queue and keeps working through it while no
client is open ([03 §3.4](03-architecture.md)). `download add` queues each source for a profile and
returns at once. The daemon resolves it, downloads its files, and adds them to the profile together,
with their dependencies when `--with-deps` is given. `download list` shows what each download waits
for. A file its provider hands over through the browser waits for you, with the page to start it on;
the link that page hands over reaches the queue through `msbe handoff`
([06 §6.7](06-providers-and-policy.md)). A link nothing waits on becomes a download of its own, and
`download confirm` adds it to a profile. Download and handoff commands use the running daemon's data
directory, and refuse a different `--home`.

```sh
"$MSBE" download add mc modrinth:sodium modrinth:iris --with-deps
"$MSBE" download list
"$MSBE" download pause
"$MSBE" download confirm 7 mc --profile default
```

Handler commands decide whether MSBE opens the links provider pages hand files over in
([07 §7.4](07-browser-and-secrets.md)). `handler status` lists each scheme an enabled provider hands
links over in, and which application opens it. `handler register` makes `msbe handoff` open a
scheme's links for the current user. While another application opens them it refuses and names that
application: `--replace` takes the scheme over, and `handler unregister` gives it back, whichever
data directory is in use. On macOS the application bundle declares its schemes instead, and a link
without a handler can be pasted into Desktop's Downloads page.

```sh
"$MSBE" handler status
"$MSBE" handler register example --replace
"$MSBE" handler unregister example
```

Browser commands drive the MSBE browser, an optional component installed beside the daemon
([07 §7.2](07-browser-and-secrets.md)). `browser open` sends it to the next page a download waits on,
or to download `ID`'s page. It captures the link or the file that page hands over and adds it to the
queue; it never clicks for you. With `--auto-advance` it goes to the next waiting page after each
capture, and `--auto-advance` alone while it shows a waiting page only changes that setting. A file
whose page hands over the file itself waits for the browser, or for you to add the saved file with
`add` and cancel the download.

```sh
"$MSBE" browser open --auto-advance
"$MSBE" browser status
"$MSBE" browser close
```

Tool commands register the programs you installed for providers that fetch content with an external
tool ([06 §6.5](06-providers-and-policy.md#65-steam-workshop-steamcmd-or-user-supplied-content)).
`tool register` records the program's SHA-256 and accepts the provider's terms, which `tool list`
shows; it refuses without `--accept-terms`. The download queue runs the program only while it still
has that SHA-256, so a program that changed must be registered again. Queue items with `download
add`; `add` and `update` refuse a file only a tool can fetch. Pass an absolute path, since the daemon
resolves it.

```sh
"$MSBE" tool list
"$MSBE" tool register example-tool "$HOME/tools/example-tool" --accept-terms
"$MSBE" download add mygame example-tool:123456
```

## 7. Planned command surface

These commands are part of the documented product direction, but they are not available
in the current binary. Their names and arguments can change before implementation.

| Area                      | Planned commands                                                                      | Target milestone          |
| ------------------------- | ------------------------------------------------------------------------------------- | ------------------------- |
| Instance lifecycle        | `instance detect`, `instance show`, `instance use`                                    | Future M1/M2 follow-up    |
| Profiles                  | `profile switch`, `copy`, `diff`, `export`, `import`                                  | M2                        |
| Resolution                | `lock`, `sync`, conflict resolution, ordering controls                                | M2                        |
| Plans and registry        | `plan`, `registry`                                                                    | M7                        |
| Diagnostics and store     | `doctor`, `store`, `bundle`                                                           | M2 and later              |
| Daemon control            | `daemon start`, `stop`, `status`; Windows named-pipe transport                        | M1 follow-up              |
| Steam Workshop            | Local directory import; optional item-ID metadata; a tool program for a user-installed SteamCMD| Future, subject to policy |
| Game launch               | `launch`                                                                              | Future                    |

M2 also adds loader bootstrap, structured config merge, pack import, lockfiles, and reproducible
cross-platform deployment. Target pre-filtering and the Modrinth PubGrub graph solver are
implemented; artifact-derived Fabric/NeoForge range metadata remains pending.
Later
milestones add legacy Minecraft topologies, a No Man's Sky validation plan, the
acquisition stack, the desktop UI, a signed registry, and Bethesda/KSP support. See
[13 - Roadmap](13-roadmap.md) for the milestone definitions and completion criteria.

### 7.1 Planned Steam Workshop acquisition and imports

Steam Workshop acquisition is **not implemented**, but the seam it will use is: a provider program
on the `tool-v1` runtime runs a program the user registered with `msbe tool register`. No SteamCMD
program ships. That program may invoke a SteamCMD binary supplied by the user to acquire content
their account is entitled to receive.
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

The daemon discovers game support from `plans/<game-id>/plan.toml` beneath its run
directory. Use `msbe-daemon --plans DIR` to select a different developer registry. The
Desktop lists those games and never asks users for a plan path. `plan.load` and
`plan.unload` are developer RPC methods for refreshing one entry without restarting;
existing instances retain their pinned support definition when an entry is unloaded.

The built-in provider catalog recognizes the `modrinth:` source prefix and HTTPS direct URLs.
Provider manifests are versioned and constrained configuration, not downloaded code: they can
describe source recognition, HTTPS metadata origins, policy, and a supported acquisition
primitive. They resolve only through reviewed runtimes. A provider that requires a key or accepted
terms is refused until `msbe auth login` and `msbe auth acknowledge` record them. See [06 - Providers & policy](06-providers-and-policy.md#63-provider-manifests)
for the contribution and safety model.

For unsupported behavior, consult [09 - Interfaces](09-interfaces.md) for the intended
full interface and [13 - Roadmap](13-roadmap.md) for its implementation status.
