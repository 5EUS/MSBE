# 17 - Pack formats and native bundles

Pack import and export are extension capabilities. They are not game logic, CLI logic, or a
special case in `msbe-core`.

This distinction is load-bearing. A Modrinth `.mrpack`, a CurseForge manifest, a Thunderstore
package, and a native MSBE bundle describe overlapping ideas, but they have different metadata,
provider identities, loader names, side rules, acquisition policy, and redistribution rules.
Putting those details in the CLI makes every new format a cross-cutting change and eventually
puts game-specific branches in every client.

The architecture defined here replaces that coupling with three layers:

1. **`msbe-core` resolves truth.** Profiles, plans, provenance, lockfiles, deployment paths, and
   content digests remain provider-neutral.
2. **Pack codecs translate formats.** A reviewed extension owns each external format and converts
   between its wire representation and neutral pack records.
3. **`msbe-pack` orchestrates.** It selects a codec, validates options, plans blob inclusion, and
   runs import/export without knowing Modrinth, CurseForge, Minecraft, or any loader name.

Provider acquisition follows the same principle as game plans: a signed, declarative provider
program selects a closed runtime vocabulary by default. Native provider code is reserved for
protocols whose semantics cannot be expressed safely by that vocabulary. Pack codecs remain
reviewed code because archive parsing and wire-format validation are a separate security boundary.

The native `.msbepack` format is implemented as a built-in codec through the same registration
surface. It is privileged only in that MSBE defines its schema; it does not get a second path
through the CLI or daemon.

## 17.1 Boundary rule

The rule is exact:

> A crate may name a game, provider, loader ecosystem, or external pack format only when that
> crate implements that game plan or provider/format extension.

Consequences:

- `msbe-cli`, `msbe-daemon`, `MSBE.Client`, and `MSBE.Desktop` do not name Modrinth,
  CurseForge, `.mrpack`, Minecraft, Fabric, Quilt, or a format-specific dependency key.
- `msbe-core` records stable provider IDs and opaque provenance supplied through neutral types,
  but does not branch on those IDs.
- `msbe-pack` knows codecs, option schemas, neutral references, lockfiles, and blobs. It does not
  know external archive layouts.
- `msbe-provider-modrinth` owns `.mrpack` detection, `modrinth.index.json`, Modrinth dependency
  keys, environment rules, and conversion to and from Modrinth project/version references.
- A future `msbe-provider-curseforge` owns CurseForge `manifest.json`, project/file IDs, and
  `allowModDistribution` behavior.
- The local/native extension owns `.msbepack`, local-file references, embedded blobs, and native
  bundle policy.
- Plans own installation topology. A codec never decides that a JAR belongs in `mods/`; it uses
  paths and source classifications already resolved by the plan and lockfile.

An external format may support only certain games or plan targets. That limitation is a codec
capability reported before export, not a conditional in a client.

## 17.2 Crate ownership

The target repository layout is:

```text
crates/
  msbe-core/                  profiles, lockfiles, resolution, provenance, CAS references
  msbe-provider-api/          provider-program contracts plus neutral pack-codec contracts
  msbe-provider-modrinth/     native Modrinth exception and .mrpack codec
  msbe-provider-direct/       direct URL program and acquisition fixture; no external pack format
  msbe-provider-local/        local acquisition and native .msbepack codec registration
  msbe-providers/             runtime registry, trusted provider programs, codecs, policy gate
  msbe-pack/                  codec selection, neutral import/export planning, option validation
  msbe-cli/                   generic pack commands; no format-specific branches
```

A dedicated `msbe-pack-native` crate may be split from `msbe-provider-local` if the native codec
becomes large. It still registers through `PackCodecRegistration`; moving code does not create a
new architectural privilege.

The current `msbe-pack` implementation violates this boundary because it contains Modrinth and
CurseForge wire types, and the current CLI violates it by constructing Minecraft and loader
fields. Those are migration debt, not precedent.

## 17.3 Adapter and codec registration

Acquisition adapters and pack codecs are related but separate capabilities:

- An **adapter** searches, resolves, acquires, verifies, and updates provider artifacts.
- A **codec** detects, imports, and exports a pack format.

A provider may implement either or both. Direct HTTPS is an adapter without a public pack format.
The native codec can package local content without a network API. Modrinth implements both.

A provider program normally selects a reviewed runtime by name. Native extensions are explicit
exceptions and export a registration containing zero or more pack codecs:

```rust
pub struct Registration {
    pub id: &'static str,
    pub manifest: &'static str,
    pub overlay: &'static [&'static str],
    pub build: BuildAdapter,
    pub pack_codecs: &'static [PackCodecRegistration],
}

pub struct PackCodecRegistration {
    /// Stable global ID, such as "modrinth-mrpack" or "msbe-native".
    pub id: &'static str,
    /// Builds the reviewed codec implementation.
    pub build: BuildPackCodec,
}
```

Codec metadata is returned by the implementation rather than interpreted as executable behavior
from provider TOML. A provider manifest may declare policy and display metadata, but an unknown
manifest never activates a generic archive parser.

```rust
pub struct PackCodecDescriptor {
    pub id: String,
    pub provider: Option<String>,
    pub name: String,
    pub extensions: Vec<String>,
    pub media_types: Vec<String>,
    pub directions: PackDirections,
    pub supported_games: SupportSet,
    pub option_schema: PackOptionSchema,
}
```

`provider` is absent for a provider-neutral codec such as `msbe-native`. `supported_games` is an
explicit set or a universal marker. It is advisory for UI filtering and is checked again by the
codec during planning.

`msbe-providers` loads trusted provider programs and builds native adapters and codecs from the
same fail-closed registry. It rejects:

- duplicate codec IDs;
- a codec claiming a provider other than its registration;
- an extension or media-type collision with equal detection priority;
- a codec whose option schema is invalid;
- a codec enabled while its provider policy is unavailable;
- a provider program whose signature, schema, runtime, or vocabulary is unsupported;
- a native manifest without a matching reviewed registration;
- an uncompiled manifest that claims codec behavior.

Clients retrieve descriptors from the daemon. They do not hardcode a list of formats.

## 17.4 Neutral codec contract

The codec boundary carries neutral records only. External wire records stay private to the
extension crate.

```rust
pub trait PackCodec: fmt::Debug + Send + Sync {
    fn descriptor(&self) -> &PackCodecDescriptor;

    fn probe(&self, input: &mut dyn ReadSeek) -> Result<Probe, PackCodecError>;

    fn plan_import(
        &self,
        input: &mut dyn ReadSeek,
        context: &PackImportContext,
        options: &PackOptions,
    ) -> Result<PackImportPlan, PackCodecError>;

    fn plan_export(
        &self,
        context: &PackExportContext,
        options: &PackOptions,
    ) -> Result<PackExportPlan, PackCodecError>;

    fn export(
        &self,
        plan: &PackExportPlan,
        blobs: &dyn BlobReader,
        output: &mut dyn WriteSeek,
    ) -> Result<PackExportResult, PackCodecError>;
}
```

`probe` reads bounded leading bytes and archive entry names. File extension is a hint, never the
only detector. Probing cannot access the network, mutate state, or extract arbitrary archive
content.

`plan_import` and `plan_export` are pure planning phases. They expose every acquisition,
embedded blob, omission, policy decision, incompatibility, and warning before bytes are written.
This keeps preview and execution on the same path.

### Import records

```rust
pub struct PackImportPlan {
    pub codec: String,
    pub title: Option<String>,
    pub target: ImportedTarget,
    pub requirements: Vec<PackRequirement>,
    pub embedded: Vec<EmbeddedBlob>,
    pub warnings: Vec<PackWarning>,
}

pub enum PackRequirement {
    Provider {
        package: PackageId,
        version: Option<String>,
        hashes: BTreeMap<String, String>,
        destination: Option<RelPath>,
        side: Availability,
    },
    Direct {
        urls: Vec<String>,
        hashes: BTreeMap<String, String>,
        destination: Option<RelPath>,
        side: Availability,
    },
    UserAction {
        provider: String,
        reference: String,
        reason: String,
        destination: Option<RelPath>,
    },
}
```

The orchestration layer sends `Provider` and `Direct` requirements back through the provider
registry. A codec never downloads an artifact during parsing. `UserAction` is a normal result for
browser-assisted or distribution-restricted content.

Embedded files are ingested through the normal archive limits and CAS. Their destination paths
are validated `RelPath` values and still pass through plan resolution; a codec cannot write into
a game directory.

### Export context

```rust
pub struct PackExportContext<'a> {
    pub game: &'a LockedPlan,
    pub target: &'a LockedTarget,
    pub lockfile: &'a Lockfile,
    pub files: &'a [PackFile],
}

pub struct PackFile {
    pub path: RelPath,
    pub digest: Digest,
    pub role: PackFileRole,
    pub source: BlobSource,
    pub distribution: DistributionDecision,
}
```

`PackFileRole` distinguishes provider artifact, pack-owned config, local artifact, generated
output, loader component, and other resolved content. This classification is produced while the
lockfile is built; exporters must not infer ownership from paths such as `config/` or `mods/`.

`BlobSource` states whether the exact digest can be reacquired:

```rust
pub enum BlobSource {
    Provider {
        provenance: Provenance,
        exact: bool,
        currently_acquirable: bool,
    },
    Direct {
        urls: Vec<String>,
        hashes: BTreeMap<String, String>,
        currently_acquirable: bool,
    },
    Local,
    PackOwned,
    Derived { inputs: Vec<Digest>, deterministic: bool },
    Component { id: String, version: String },
}
```

A provider reference counts as reproducible only when it identifies an exact release/file and
has a verified digest. A project slug or mutable URL is not enough.

## 17.5 Game and loader identities

The CLI must not translate `fabric` to `fabric-loader`, and core must not know that a Modrinth
pack uses `minecraft` as a dependency key.

Format-specific identity translation belongs to the codec. The codec receives stable plan and
loader IDs plus neutral capabilities. It may ship reviewed mappings in its own crate or
provider-owned data files:

```toml
# Private to msbe-provider-modrinth.
[[pack-target]]
plan = "minecraft"
game = "minecraft"

  [pack-target.loaders]
  fabric = "fabric-loader"
  quilt = "quilt-loader"
  neoforge = "neoforge"
  forge = "forge"
```

This mapping is not part of the generic plan schema because `fabric-loader` is a Modrinth wire
identity, not an installation fact. A codec that lacks a mapping reports
`UnsupportedTarget { codec, plan, loader }` during preview. Adding support changes only the
extension crate.

Provider-neutral capabilities such as loader `provides`, side, and game version remain in the
lockfile and may be used by any codec. Wire names remain private.

## 17.6 Option schemas

Pack options are data supplied by the codec, rendered by CLI/Desktop, and validated by
`msbe-pack` before the codec runs. Clients do not gain one property or command-line flag per
format.

The schema is a closed, versioned subset designed for NativeAOT clients and deterministic CLI
serialization:

```rust
pub struct PackOptionSchema {
    pub schema: u32,
    pub presets: Vec<PackPreset>,
    pub fields: Vec<PackOptionField>,
    pub constraints: Vec<PackOptionConstraint>,
}

pub struct PackOptionField {
    pub key: String,
    pub label: String,
    pub description: String,
    pub required: bool,
    pub default: PackOptionValue,
    pub kind: PackOptionKind,
}

pub enum PackOptionKind {
    Boolean,
    Integer { min: i64, max: i64, step: i64 },
    Text { min_len: usize, max_len: usize, pattern: Option<String> },
    Choice { values: Vec<PackChoice> },
    MultiChoice { values: Vec<PackChoice>, min: usize, max: usize },
    Path { mode: PathMode, extensions: Vec<String> },
}
```

Schemas have no scripts, expressions, remote references, arbitrary JSON Schema keywords, or
format-provided XAML. Constraints use a small reviewed vocabulary such as `requires`,
`conflicts_with`, and `visible_when_equals`.

Every export persists:

- codec ID and codec schema version;
- normalized option values, including defaults;
- selected preset, if any;
- MSBE version and lockfile schema;
- warnings acknowledged by the user.

This makes an export invocation reproducible and lets the CLI print the exact equivalent of a
Desktop selection.

### Common options

`msbe-pack` prepends common policy fields to every codec schema:

| Key               | Values                                       | Meaning                                                              |
| ----------------- | -------------------------------------------- | -------------------------------------------------------------------- |
| `purpose`         | `distribute`, `private-transfer`, `backup`   | Determines redistribution policy and warning posture.                |
| `reproducibility` | `strict`, `allow-user-action`, `best-effort` | Controls whether unresolved exact content fails export.              |
| `blob-mode`       | `thin`, `portable`, `complete`               | Default embedded-blob selection; codecs may restrict it.             |
| `on-forbidden`    | `error`, `external-requirement`              | Never permits embedding; chooses failure or an explicit requirement. |
| `compression`     | codec-declared choices                       | Compression affects bytes and size, never logical content.           |
| `deterministic`   | `true` by default                            | Requires normalized ordering, timestamps, permissions, and metadata. |

External codecs append format-specific choices. A Modrinth codec may expose pack version,
client/server environment defaults, and override inclusion. It may not expose a switch that
weakens provider policy.

### Presets

Schemas may define presets as named complete option maps:

- **Small download**: prefer provider references; include only content that cannot be reproduced
  otherwise.
- **Portable**: include pack-owned configs and legally embeddable unsourceable content; reference
  exact provider files.
- **Offline backup**: include every legally embeddable required blob.
- **Public distribution**: fail on unknown or prohibited redistribution and minimize embedded
  third-party content.

Presets are convenience only. The normalized field map is authoritative.

## 17.7 Native `.msbepack` format

`.msbepack` is MSBE's provider-neutral, content-addressed bundle. It is a deterministic ZIP64
container in schema 1. ZIP is framing, not semantics; readers validate every path and digest and
apply the same archive limits as any imported mod.

```text
example.msbepack
  msbe-pack.toml
  lock.toml
  requirements.toml
  blobs/
    sha256/
      ab/
        cdef...                 raw blob bytes
  metadata/
    notices/                    optional licenses/notices selected for distribution
    export-options.toml         normalized codec options
  signatures/                   reserved; absent in schema 1
```

### `msbe-pack.toml`

```toml
schema = 1
format = "msbe-native"
created_by = "0.5.0"
lockfile = "lock.toml"
requirements = "requirements.toml"

[content]
mode = "portable"
embedded = 14
referenced = 237

[compatibility]
lock_schema = 1
plan_id = "minecraft"
plan_version = "0.2.0"
```

No creation timestamp is written in deterministic mode. Entry ordering is lexical, path
separators are `/`, permissions are normalized, extra ZIP fields are stripped, and compression
settings are fixed by schema plus normalized options. Exporting the same lockfile, selected
blobs, options, and notices produces byte-identical output.

### `lock.toml`

This is the canonical lockfile, not a second manifest model. Native import can reconstruct the
same resolved profile because it preserves:

- pinned plan identity;
- target and side;
- module order;
- exact provider provenance;
- components;
- every deployment path and digest;
- explicit file role/source metadata added by the lockfile schema revision.

The lockfile remains independently usable outside the archive.

### `requirements.toml`

Requirements explain every digest not embedded in the bundle:

```toml
schema = 1

[[requirement]]
digest = "sha256:..."
kind = "provider"
provider = "modrinth"
project = "..."
version = "..."
hashes = { sha512 = "..." }

[[requirement]]
digest = "sha256:..."
kind = "user-action"
provider = "nexus"
reference = "..."
reason = "Free-account download requires browser confirmation"
```

A thin bundle may contain no blobs. A portable bundle normally embeds configs, local files, and
other exact content with no stable source. A complete bundle attempts to embed all required
blobs that policy allows.

Native import first verifies embedded blobs, then resolves omitted requirements through provider
adapters, then derives deterministic outputs, and finally checks that every deployment digest in
the lockfile is available. A mismatch is an integrity failure, never an opportunity to rewrite
the lockfile.

## 17.8 Blob inclusion and redistribution

“Cannot be sourced from a provider” and “may be redistributed” are different facts.

The export planner evaluates every required digest along two independent axes:

1. **Reacquisition**: can this exact digest be obtained again through a stable provider/direct
   reference or deterministic derivation?
2. **Distribution**: may this export purpose embed the bytes?

The decision table is:

| Exact source | Embedding allowed | Typical action                                                                                                 |
| ------------ | ----------------- | -------------------------------------------------------------------------------------------------------------- |
| yes          | yes               | Omit in `thin`/`portable`; include in `complete`.                                                              |
| yes          | no                | Emit exact requirement; never embed.                                                                           |
| user action  | yes               | Embed in `portable`/`complete`, or emit user action when minimizing size.                                      |
| user action  | no                | Emit user action; strict offline export fails.                                                                 |
| no           | yes               | Embed in `portable`/`complete`; thin strict export fails.                                                      |
| no           | no/unknown        | Public export fails; private export requires explicit policy and still cannot override a provider prohibition. |

Pack-owned configs are authored by the pack creator and default to embeddable. Local mods have
unknown distribution rights by default. They may be included in a private transfer/backup after
an explicit acknowledgement, but public distribution requires affirmative license or provider
policy metadata. A provider's distribution prohibition is not user-overridable.

Derived blobs may be omitted only when every input is available and the derivation is declared
deterministic under the pinned plan version. Otherwise they are treated as unsourceable output.

`reproducibility = strict` means import can reach every lockfile digest without substituting a
new release. It does not mean MSBE ignores distribution rules. If strict reproduction cannot be
packaged legally, export fails with the exact blocking files and available alternatives.

## 17.9 Export planning and execution

Export is always two-phase:

1. Load or derive the canonical lockfile.
2. Classify every required digest by role, source, derivability, and distribution decision.
3. Select a codec from the requested ID or output extension.
4. Fetch its descriptor and option schema.
5. Normalize and validate options.
6. Ask the codec for a `PackExportPlan`.
7. Merge codec requirements with the neutral blob-inclusion plan.
8. Present files, references, embedded bytes, estimated size, warnings, and blockers.
9. On confirmation, stream to a temporary output while hashing.
10. Finalize the codec, verify its declared manifest, and atomically rename the output.

`PackExportPlan` contains no open files or callbacks and is serializable over RPC. Desktop preview
and CLI `--dry-run` therefore show the exact operation that execution uses.

The daemon owns execution because it owns lockfiles, CAS access, provider policy, and serialized
mutation. Clients choose options and output destinations only.

## 17.10 Import planning and execution

Import follows the inverse flow:

1. Probe every permitted codec using bounded reads.
2. Refuse ambiguous matches and report the candidate codec IDs.
3. Parse with the selected codec into `PackImportPlan`.
4. Resolve target compatibility against a selected instance/profile or create-profile request.
5. Send provider requirements through the normal resolver and policy gate.
6. Queue browser/user-action requirements without pretending they are failures.
7. Verify and ingest embedded blobs into CAS.
8. Require every locked digest to be present or reproducible.
9. Create a profile only after the complete plan validates.
10. Preview deployment separately; import never writes the game directory.

External pack manifests that do not pin exact versions or hashes produce an ordinary resolution
request, not a false lockfile. Native `.msbepack` import preserves its lockfile exactly.

## 17.11 CLI, RPC, and Desktop surfaces

The generic CLI surface is:

```text
msbe pack formats [--direction import|export] [--game GAME]
msbe pack options FORMAT [--preset PRESET]
msbe pack import INSTANCE INPUT [--format FORMAT] [--options FILE] [-p PROFILE] [--dry-run]
msbe pack export INSTANCE OUTPUT --format FORMAT [--options FILE] [-p PROFILE] [--dry-run]
msbe pack validate INSTANCE [-p PROFILE]
```

`FORMAT` is a codec ID such as `modrinth-mrpack` or `msbe-native`; aliases and extensions are
resolved by descriptors. The CLI does not gain `--minecraft-version`, `--fabric-loader`, or
other wire-specific flags. Format-specific values live in the options document.

RPC adds descriptor, schema, preview, and execute methods rather than tunneling opaque CLI text:

```text
pack.codec.list
pack.codec.options
pack.import.preview
pack.import.execute
pack.export.preview
pack.export.execute
```

Desktop renders `PackOptionSchema` with native controls, provides presets, and shows an export
preview grouped into provider references, user actions, embedded configs, embedded local content,
derived content, and policy blockers. It never contains a Modrinth-specific ViewModel.

## 17.12 Error model

Pack failures are typed and stable:

- `UnknownCodec`
- `AmbiguousFormat`
- `UnsupportedDirection`
- `UnsupportedTarget`
- `InvalidOptions`
- `ManifestTooLarge`
- `UnsafeArchivePath`
- `MissingExactVersion`
- `MissingDigest`
- `MissingBlob`
- `UnreproducibleContent`
- `DistributionForbidden`
- `DistributionUnknown`
- `UserActionRequired`
- `IntegrityMismatch`
- `CodecFailure`

Errors name the codec and affected package/path/digest. Raw parser or provider error text is
retained as diagnostic detail but is not the public contract.

## 17.13 Security limits

Every codec obeys shared limits before format-specific parsing:

- bounded manifest bytes, archive entries, total expanded bytes, nesting depth, and compression
  ratio;
- no absolute paths, `..`, backslashes, drive letters, alternate data streams, symlinks, devices,
  or duplicate normalized paths;
- create-new temporary outputs and atomic final rename;
- digest verification before CAS admission;
- no network during probe or manifest parse;
- no direct game-directory writes;
- no codec-provided executable UI or validation script;
- no policy downgrade through options;
- deterministic archive metadata by default;
- unknown schema fields fail closed.

Native bundles do not trust their lockfile merely because MSBE wrote the format. Every embedded
blob is hashed and every referenced digest is resolved independently.

## 17.14 Migration from the current implementation

Migration is incremental, but the end state is non-negotiable.

### Verified boundary audit (2026-09-12)

The Rust workspace was audited for explicit Modrinth names and implicit assumptions such as
`.mrpack` paths, provider wire fields, loader dependency keys, API endpoints, hash choices, and
release-channel behavior. The remaining production violations are:

| Location                                    | Violation                                                                                                                                                                      | Required owner                                                                                                                     |
| ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------- |
| `msbe-pack`                                 | Owns `modrinth.index.json`, `.mrpack` detection and ZIP layout, `ModrinthPack`/`ModrinthFile`, Modrinth export, CurseForge wire records, and a hardcoded `game = "minecraft"`. | Move Modrinth behavior to `msbe-provider-modrinth`; remove CurseForge records until a reviewed CurseForge adapter and codec exist. |
| `msbe-cli::pack_import`                     | Branches on `Pack::Modrinth` and `Pack::CurseForge`, interprets Modrinth environment flags, selects download URLs and hashes, and emits format-specific errors.                | Replace with neutral `PackImportPlan` execution through codec lookup and the provider policy gate.                                 |
| `msbe-cli::pack_export`                     | Builds Modrinth dependencies and calls `export_modrinth` directly.                                                                                                             | Replace with codec selection, normalized options, and `PackExportPlan` execution.                                                  |
| `msbe-cli::loader_dependency`               | Maps `fabric` and `quilt` to Modrinth dependency keys.                                                                                                                         | Move the mapping into reviewed data private to the Modrinth codec.                                                                 |
| `msbe-cli::UpdateReport::not_from_modrinth` | Exposes a provider-specific JSON field even though the implementation means that no registered provider has update capability.                                                 | Rename to a provider-neutral field in a versioned CLI/RPC contract change and retain an explicit compatibility path if required.   |
| CLI help and pack errors                    | Name Modrinth, CurseForge and `.mrpack` as built-in command behavior.                                                                                                          | Generate format descriptions from codec descriptors and use neutral orchestration errors.                                          |

The following matches were reviewed and are **not** boundary violations:

- `msbe-core` stores opaque provider/project/version provenance and a hash map. Its Modrinth
  mentions are examples and migration fixtures; it has no provider-name branches.
- SHA-256 and SHA-512 support in `msbe-provider-api` and `msbe-core` is generic integrity and
  legacy-data handling, not a Modrinth protocol assumption.
- `msbe-providers` names `msbe-provider-modrinth::REGISTRATION` in `BUILTIN` and verifies routing
  in integration tests. The reviewed registry is the one generic crate allowed to list shipped
  extensions.
- `msbe-cli` has a dev-only dependency on `msbe-provider-modrinth` for end-to-end fixtures. Test
  coupling to a provider-owned fake is intentional and does not put provider behavior in the CLI.
- `msbe-http` mentions the Modrinth CDN and API in comments and an ignored live transport test.
  Its production user agent, TLS, limits, and request behavior are provider-neutral.
- `msbe-provider-api` overlay and manifest tests use `modrinth` as a concrete sample provider ID
  without branching on it.

This audit is a baseline, not an allowlist. New provider, game, loader, endpoint, or external
format literals in generic production code require either relocation to an extension or an
explicit architecture review. Phase D adds an automated guard after the current violations have
been removed.

### Phase A - neutral contracts

Phase A is implemented. The native codec remains intentionally unregistered until Phase C, and
the existing external formats remain on their temporary paths until Phase B.

1. [x] Add `PackCodec`, descriptors, plans, option schemas, file roles, source classifications, and
       distribution decisions to `msbe-provider-api` or a smaller neutral API crate if dependency
       direction requires it.
2. [x] Extend provider `Registration` and `Providers` with codec registration and lookup.
3. [x] Extend the lockfile schema with explicit file role/source data needed for blob planning.
4. [x] Add native codec conformance fixtures and deterministic archive tests.

### Phase B - move existing formats

1. Move Modrinth wire structs and `.mrpack` ZIP code from `msbe-pack` into
   `msbe-provider-modrinth`.
2. Move Minecraft/loader dependency mapping from `msbe-cli` into Modrinth codec-owned mapping.
3. Remove CurseForge wire structs from generic code; reintroduce them only with the reviewed
   CurseForge adapter and codec.
4. Reduce `msbe-pack` to orchestration, probing, option validation, and blob planning.
5. Replace CLI format branches with codec lookup.

### Phase C - declarative provider runtimes

1. Define the signed provider-program schema and its closed vocabularies for source recognition,
   routes, typed record mappings, compatibility, dependency relations, acquisition, and policy.
2. Implement bounded `catalog-v1` and direct-URL runtimes plus fixture-based conformance tests.
3. Load trusted provider programs through `msbe-providers`; report signer, schema, and runtime
   through the daemon without exposing provider wire details to clients.
4. Convert the declarative subset of existing providers to programs. Retain native adapters only
   for documented protocol, update, authentication, or policy semantics the runtime cannot model.
5. Require every native provider registration to state its exception reason and run the same
   neutral-record, transport, acquisition, and policy conformance suite.

### Phase D - native bundles and clients

1. Register `msbe-native` through the local/native extension.
2. Add thin, portable, complete, and public-distribution presets.
3. Add typed daemon pack RPC.
4. Render codec schemas and export previews in Desktop.
5. Keep the current command-run bridge only as a compatibility path until typed RPC ships.

### Phase E - compatibility and removal

1. Read legacy lockfile schema 1 and classify missing source roles conservatively.
2. Continue importing existing `.mrpack` archives through the relocated codec.
3. Remove `loader_dependency`, hardcoded `minecraft`, `Pack::Modrinth`,
   `Pack::CurseForge`, and `export_modrinth` from generic crates.
4. Add CI guards forbidding provider/game/format literals in anything but provider-specific crates
   outside fixtures and user-facing neutral examples.

Until Phase B is complete, the existing implementation is explicitly temporary and must not be
copied for another format.

## 17.15 Acceptance criteria

The architecture is complete when:

- adding a pack format changes one extension crate and one registration line, not CLI/core/UI;
- CLI and Desktop discover formats and options from daemon descriptors;
- `.mrpack` behavior is byte-for-byte covered after moving out of generic crates;
- the native codec reproduces a profile with provider mods, pack-owned configs, local mods, and
  deterministic derived files;
- thin, portable, and complete exports produce the documented inclusion sets;
- repeated deterministic exports are byte-identical;
- public export cannot embed content with forbidden or unknown distribution rights;
- strict export fails with actionable blockers when exact reproduction is impossible;
- native import verifies every blob and reaches the original deployment digest map;
- no generic client or core crate branches on a provider, game, loader, or external format ID.
