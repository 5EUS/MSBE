# 02 — The Plan System

This is the heart of the project. If this abstraction is right, adding a game is an
afternoon and a pull request. If it is wrong, every new game becomes a special case
and MSBE degenerates into Vortex with extra steps.

## 2.1 Install topologies as orthogonal axes

The mistake would be to enumerate game *types* ("drop-in games", "load-order games").
Real games mix and match. Instead, a Plan is a point in an eight-axis space. Every
modding scheme encountered so far decomposes cleanly into these.

| Axis | Question | Values |
|---|---|---|
| **A — Acquisition** | where do bytes come from? | provider API · direct URL · browser-assisted · local file · git · component upstream |
| **B — Unpacking** | what container? | none (bare jar/dll) · zip/7z/rar/tar · nested · game container (`.pak`, `.bsa`, `.vpk`) · pack format (`.mrpack`, Thunderstore, CurseForge `manifest.json`) |
| **C — Shape** | where do the files *inside* go, and which are **not** content? | strip-root heuristic · glob allow/deny · manifest-driven · interactive wizard (FOMOD/BAIN) · extension-computed |
| **D — Materialization** | how do bytes reach the game dir? | reflink · hardlink · copy · symlink · junction · VFS overlay · in-place container injection · none (loader reads an external dir) |
| **E — Ordering** | who wins a conflict? | unordered · priority overwrite · lexical · explicit order file · topological from deps · game-managed |
| **F — Mutation** | what non-file changes? | structured config merge · INI/registry tweak · binary patch · launch-arg injection · env var |
| **G — Loading regime** | which **Loader**, and how is it established? | `none` · one of several declared loaders (fabric / neoforge / paper / bepinex …) · bootstrap component · in-place patcher · external launcher |
| **H — Validation** | what must hold? | artifact hash · game-version compat · loader compat · engine limits (e.g. plugin count) · anticheat warning |

Three worked examples:

| | No Man's Sky | Minecraft (Fabric) | Skyrim SE |
|---|---|---|---|
| A | Nexus, direct | Modrinth / CurseForge | Nexus (free → browser-assisted) |
| B | zip/7z → `.pak` | bare `.jar` | 7z, often with FOMOD |
| C | strip-root + hygiene filter | none | wizard + hygiene filter |
| D | copy/hardlink → `GAMEDATA/MODS` | hardlink → `mods/` | VFS or hardlink |
| E | lexical (pak load order) | unordered (deps resolve it) | priority + explicit plugin order |
| F | optional ini tweak | TOML config merge | `.ini` edits |
| G | pak-check disable | Fabric loader bootstrap | SKSE |
| H | hash, ~100 pak soft cap | mc-version × loader × dep solve | ESL/ESP count, master graph |

The core implements the axes. A Plan just says which values it picks.

### Axis C in practice: exclusion & hygiene

Shape resolution is not only "where do files go" — it is equally "which files should
never be deployed at all." Real mod archives are full of things that are not the mod:

`README.md` · `LICENSE` · `changelog.txt` · screenshots and preview `.png`s ·
`__MACOSX/` · `.DS_Store` · `Thumbs.db` · `desktop.ini` · `.git/` · source folders ·
`.pdb`/`.map` debug symbols · a duplicate copy of the mod loader · nested
`Optional/`, `Old versions/`, `Docs/` directories · the author's own working notes.

Deploying these is not merely untidy, it actively breaks things:

- **Phantom conflicts.** Two mods that each ship a root `README.txt` are reported as
  conflicting when nothing is actually wrong. At 250 mods this drowns the real
  conflicts in noise, which is how users learn to click through conflict dialogs.
- **Games that scan directories** (anything globbing `mods/*`, and several loaders
  that enumerate everything in their plugin folder) can choke on or try to load
  stray files.
- **Purge accuracy.** Every junk file is another path MSBE must track and remove.
- **`__MACOSX/` and `.DS_Store`** in particular appear in a large fraction of
  community archives and are pure noise on every platform including macOS.

So exclusion is a first-class part of every plan, at three layers:

| Layer | Source | Overridable by |
|---|---|---|
| **Global hygiene set** | `msbe-core` default denylist — OS cruft, VCS dirs, debug symbols | plan, then user |
| **Plan denylist** | `[[steps]] extract.deny` in the plan, e.g. `**/*.txt` for a game where text files are never content | user |
| **Per-mod override** | registry overlay entry or user's own rule for one troublesome mod | — |

Two rules make this safe rather than dangerous:

1. **Excluded files are quarantined, not deleted.** They stay in the CAS tree and are
   reachable — `msbe mod docs <mod>` opens the mod's README, the UI shows a
   "documentation" tab. A conservative filter that hides a file is recoverable; one
   that destroys it is not. This also means we can afford a slightly aggressive
   default set.
2. **Exclusions are reported, not silent.** The resolve step's `OperationSet` carries
   an `excluded[]` list with the rule that matched each path, so
   `msbe plan explain` and the UI's install preview both show exactly what was
   dropped and why. Silent filtering is indistinguishable from a bug when a mod
   genuinely ships a `.txt` that *is* its config.

When several rules match one file, the report names the most specific: a hygiene rule,
then an explicit deny, then a quarantine pattern, and only then a miss against the allow
list.

The defaults therefore lean conservative — exclude what is unambiguously not content
(OS metadata, VCS, symbols) — and leave genuinely game-specific judgements
(`**/*.txt`, `Docs/`, loose `.dll`s) to the plan, where a human who knows the game
made the call and a fixture test pins it.

### One game, many topologies: Minecraft across its eras

Minecraft is not one install model, it is four, and they are structurally unlike each
other. This is why it can validate the abstraction on its own — and why it is the sole
v0.1 game ([13](13-roadmap.md)).

| Era | Install model | Axes stressed |
|---|---|---|
| **Modern** (1.14+, Fabric / NeoForge) | drop a `.jar` into `mods/`; deps declared in `fabric.mod.json` / `neoforge.mods.toml` | B (none) · D (hardlink) · E (dep-resolved) · G (loader bootstrap) |
| **Mid Forge** (1.6–1.12) | `mods/` plus `coremods/`, ASM transformers, `.cfg` configs, weak `mcmod.info` metadata | F (config merge) · E (solver degrades honestly on poor metadata) |
| **Legacy jarmods** (pre-1.6) | class files injected **into** `minecraft.jar`, in order, with `META-INF/` deleted to defeat the signature check | **B (game container) · D (in-place container injection) · F (ordered mutation)** |
| **Server plugins** (Bukkit / Paper) | an entirely different loader for the same game: `plugins/`, its own ecosystem | G · the Instance ↔ Plan relationship itself |

Plus, within any one of them: multiple deploy targets in a single plan (`mods/`,
`config/`, `resourcepacks/`, `shaderpacks/`, per-world `datapacks/`), a real
version×loader compatibility matrix, client-versus-server capability splits, and two
pack formats to import.

The legacy row is the important one. Jarmod injection is the only place in v0.1 that
exercises Axis D's *in-place container injection* — the same shape later needed for
Bethesda BSAs, Cyberpunk `archive/pc/mod`, and Baldur's Gate 3 `.pak` handling. Finding
out in M3 that the schema cannot express it is cheap; finding out in M8 is not.

## 2.2 Resolve / apply split

**No step ever touches the filesystem.** Every step is a pure function

```
(PlanContext, Inputs) -> Result<OperationSet, StepError>
```

where `OperationSet` is an inert, serializable list of operations
(`CreateDir`, `Materialize{blob, path, backend}`, `Backup{path}`, `WriteFile`,
`MergeConfig`, `SetLaunchArg`, `RegisterPlugin`, …). Only the **applier** — one
audited module in `msbe-core` — executes them, and it journals every one.

Consequences, all of which are load-bearing:

- `--dry-run` is the same code path minus the applier call. It cannot drift.
- `msbe profile diff` is a set difference over two `OperationSet`s.
- Rollback is replaying the journal backwards.
- A malicious or buggy plan extension can emit a *bad* operation but cannot perform
  an *unmodelled* one. There is no `std::fs` in the extension sandbox at all.
- Steps are trivially unit-testable against fixture archives with no disk I/O.

## 2.3 Manifest format

TOML, `schema = 1`, stored in the registry as `plans/<id>/plan.toml`.

```toml
schema  = 1
id      = "nomanssky"
name    = "No Man's Sky"
version = "1.4.0"          # plan's own semver
engine  = ">=0.4, <2.0"    # msbe-core compatibility

[game]
detect = [
  { store = "steam", app_id = "275850" },
  { store = "gog",   product_id = "1446213994" },
  { store = "xbox",  package = "HelloGames.NoMansSky" },
  { probe = "file",  path = "Binaries/NMS.exe", version_from = "pe" },
]
anticheat = false
multiplayer_risk = "cosmetic-only"

[paths]
mods   = "GAMEDATA/MODS"
banks  = "GAMEDATA/PCBANKS"
config = "Binaries/SETTINGS"

[deploy]
backend  = ["reflink", "hardlink", "copy"]   # preference chain, first supported wins
target   = "@paths.mods"
ordering = "lexical"
protect  = ["@paths.banks/*.pak"]            # vanilla; back up before any overwrite

[[steps]]
type = "extract"
  [steps.with]
  strip_root = "auto"
  allow      = ["**/*.pak"]                  # NMS mods are paks; nothing else is content
  deny       = ["**/*.exe", "**/*.dll"]      # never, under any circumstance
  hygiene    = "default"                     # OS cruft, VCS dirs, debug symbols
  quarantine = ["**/*.md", "**/*.txt",       # kept in CAS & browsable,
                "**/*.png", "Docs/**"]       #   never deployed, reported in preview

[[steps]]
type = "ensure-component"
with = { component = "nms.disable-pak-check", optional = true, prompt = true }

[[steps]]
type = "place"
with = { into = "@paths.mods", flatten = true }

[conflicts]
detect = ["same-path", "same-container-entry"]
policy = "priority-overwrite"

[[validate]]
cond = "mods.len() > 100"
level = "warn"
message = "NMS becomes unstable past roughly 100 .pak files."
```

### Declaring loaders

A plan declares the loading regimes its game supports; it does **not** need one plan per
loader. Each variant supplies only what differs — deploy targets, metadata location,
config format, bootstrap component:

```toml
[[loaders]]
id            = "fabric"
bootstrap     = "mc.fabric-installer"
mod_metadata  = "fabric.mod.json"
targets       = { mods = "mods", config = "config" }
config_format = "json5"
sides         = ["client", "server"]

[[loaders]]
id            = "quilt"
provides      = ["fabric"]               # a *loader* fork: Quilt targets accept Fabric mods
bootstrap     = "mc.quilt-installer"
mod_metadata  = "quilt.mod.json"
targets       = { mods = "mods", config = "config" }
sides         = ["client", "server"]

[[loaders]]
id            = "paper"                  # same game, entirely different ecosystem
bootstrap     = "mc.paper-jar"
mod_metadata  = "plugin.yml"
targets       = { mods = "plugins" }
config_format = "yaml"
sides         = ["server"]

[[loaders]]
id            = "jarmod"                 # pre-1.6: inject into minecraft.jar, in order
bootstrap     = "none"
ordering      = "explicit"
targets       = { container = "versions/{version}/{version}.jar" }
```

The Profile's **Target** names one of these ([01](01-domain-model.md)), the solver
filters candidates by it before version solving ([05 §5.2](05-solver.md)), and steps
address the chosen loader's directories through `@loader.targets.*` rather than
hardcoding paths. A game with one regime declares one loader and its users never see
the concept.

Plans compose via `extends = "bethesda.common"`, with steps addressable by `id` so a
child plan can `insert-before`, `replace` or `remove` rather than copy-pasting.

## 2.4 Step vocabulary (closed set)

Closed and versioned on purpose — an open set becomes "arbitrary code" by degrees.

| Step | Purpose |
|---|---|
| `fetch` | acquire an artifact via a provider or URL |
| `verify` | hash / size / signature assertions |
| `extract` | archive → CAS tree; strip-root, allow/deny globs, hygiene filter, quarantine report |
| `select` | interactive choice (FOMOD wizard, optional-file picker) |
| `transform` | glob-based move/rename/filter within a tree |
| `place` | tree → deployment target |
| `merge-config` | structured merge into TOML/JSON/INI/XML/YAML/NBT/SJSON |
| `write-file` | emit a generated file (load order lists, `modsettings.lsx`) |
| `patch-binary` | apply a bounded, hash-pinned binary diff |
| `ensure-component` | install/verify a loader or script extender |
| `set-launch-arg` / `set-env` | launch configuration; also the fallback route for Proton DLL overrides |
| `set-dll-override` | Wine/Proton DLL override written to the prefix registry, scoped to one executable ([08 §8.2](08-platforms-and-detection.md)) |
| `register-plugin` | add to a game-managed order file |
| `reorder` | apply the plan's ordering strategy |
| `validate` | assert a `[[validate]]` condition |
| `run-extension` | hand off to a sandboxed WASM module (below) |

New step kinds require an engine version bump, a spec change and a test fixture.
That friction is the point.

## 2.5 The WASM escape hatch

Roughly 10% of games need real computation: FOMOD's conditional trees, KSP's
ModuleManager patch semantics, deriving a load order from record-level conflicts.
Those load a **WebAssembly component** (wasmtime, WASI Preview 2 / component model).

The extension **has no filesystem, no network, no process, and no clock** unless
granted. It reads through host-provided handles and its *only* effect on the world is
emitting operations:

```wit
package msbe:plan@0.1.0;

interface host {
  record file-entry { path: string, size: u64, hash: string }

  list-archive: func(h: archive) -> list<file-entry>;
  read-entry:   func(h: archive, path: string) -> result<list<u8>, error>;
  read-game:    func(path: string) -> result<list<u8>, error>;   // cap-scoped
  ask:          func(q: question) -> answer;                     // UI wizard
  http-get:     func(url: string) -> result<response, error>;    // declared hosts only
  log:          func(lvl: level, msg: string);
  emit-op:      func(op: operation);                             // the ONLY effect
}
```

Declared in the manifest and shown to the user before first run:

```toml
[extension]
wasm = "fomod.wasm"
sha256 = "…"
caps = [
  "archive.read",
  "ui.prompt",
  "game.read:paths=['Data/*.esm']",
  "op.emit:kinds=['place','write-file']",
]
```

**Denial is structural, not a runtime check.** Each extension is instantiated against a
`Linker` that defines only the imports its granted capabilities allow. A component that
imports anything else fails to instantiate ("unknown import … has not been defined"), so
there is no permission check to bypass: an ungranted capability does not exist in the
extension's world ([15](15-m0-findings.md)).

Enforced limits: fuel-metered execution (`Config::consume_fuel`, per `Store`), a memory
and table ceiling (`ResourceLimiter`), wall-clock timeout,
deterministic (no ambient randomness or time), and **`process.spawn` does not exist**.
A plan that genuinely must run a vendor installer uses a distinct
`run-trusted-binary` step whose binary hash is pinned in the registry and which
prompts the user every single time.

## 2.6 Authoring & testing workflow

```
msbe plan new <id>              scaffold
msbe plan validate <path>       schema + lint + capability audit
msbe plan test <path>           run against fixtures/<id>/, golden OperationSet diff
msbe plan explain <path> <mod>  print the resolved OperationSet for one mod
```

Every registry plan ships fixtures: a synthetic mod archive and the expected
`OperationSet` as a golden file. CI runs `plan test` on every PR. A plan cannot merge
without fixtures — this is what keeps a community-contributed plan honest without
requiring a maintainer to understand every game.
