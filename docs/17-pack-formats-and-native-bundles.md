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

A codec can export only what the lockfile records. §17.5 therefore defines what a lockfile must
capture: every input that affects a deployed digest, and nothing that changes over time. §17.6
defines the one identity that plans, provider programs, codecs, and WASM extensions share.
§17.18 lists what this architecture deliberately does not grow into.

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

`msbe-provider-local` registers the native codec; `msbe-pack` holds no codec of its own.

Phase B moved external wire types out of `msbe-pack`, and Phase D replaced the CLI's
format-specific pack paths with codec orchestration. Any literal that remains in a generic crate is
migration debt tracked in §17.16, not precedent.

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

A pack codec is reviewed native code today. The contract in §17.4 passes only host-owned views
and serializable records, so the same contract can later be served by a sandboxed WASM codec,
loaded through the extension envelope (§17.6) with no network, filesystem, clock, or randomness.
For pure bytes-to-records translation a sandbox is a stronger boundary than review, so
third-party formats should arrive as WASM codecs rather than as new native crates (Phase F).

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
- a codec whose option schema is invalid;
- a codec enabled while its provider policy is unavailable;
- a provider program whose signature, schema, runtime, or vocabulary is unsupported;
- a native manifest without a matching reviewed registration;
- an uncompiled manifest that claims codec behavior;
- a WASM codec whose extension envelope, signature, or capability request is invalid.

Extensions and media types may overlap. CurseForge modpacks and Thunderstore packages are both
`.zip` archives with a root `manifest.json`, so a registry that refused shared extensions would
refuse real formats. Detection is decided by probe confidence (§17.12); an extension only orders
which codecs probe first.

Clients retrieve descriptors from the daemon. They do not hardcode a list of formats.

## 17.4 Neutral codec contract

The codec boundary carries neutral records only. External wire records stay private to the
extension crate.

```rust
pub trait PackCodec: fmt::Debug + Send + Sync {
    fn descriptor(&self) -> &PackCodecDescriptor;

    fn probe(&self, input: &dyn PackInput) -> Result<PackProbe, PackCodecError>;

    fn plan_import(
        &self,
        input: &dyn PackInput,
        context: &PackImportContext,
        options: &PackOptions,
    ) -> Result<PackImportPlan, PackCodecError>;

    fn plan_export(
        &self,
        context: &PackExportContext,
        options: &PackOptions,
    ) -> Result<PackExportPlan, PackCodecError>;

    fn layout(&self, plan: &PackExportPlan) -> Result<PackLayout, PackCodecError>;
}
```

`plan_import` and `plan_export` are pure planning phases. They expose every acquisition,
embedded blob, omission, policy decision, incompatibility, and warning before bytes are written.
This keeps preview and execution on the same path. `layout` is equally pure: it maps a validated
plan to entries.

### Host-owned containers

A codec never opens, decompresses, or writes a container. The host does, through `msbe-archive`,
so the limits in §17.15 and the determinism rules in §17.9 are enforced once instead of being
trusted to every codec.

```rust
/// A bounded view of a pack input whose paths and sizes the host has already validated.
pub trait PackInput {
    fn container(&self) -> ContainerKind;
    /// Every entry, with a normalized safe path and declared size, in lexical order.
    fn entries(&self) -> &[PackEntry];
    /// Reads one entry, failing past `limit` or the host's manifest ceiling.
    fn read(&self, path: &RelPath, limit: u64) -> Result<Vec<u8>, PackCodecError>;
}

/// The entries an export contains. The host writes, hashes, and verifies them.
pub struct PackLayout {
    pub container: ContainerKind,
    pub entries: Vec<LayoutEntry>,
}

pub struct LayoutEntry {
    pub path: RelPath,
    pub content: EntryContent,
}

pub enum EntryContent {
    /// Streamed from the store; blob bytes never pass through the codec.
    Blob(Digest),
    /// Codec-generated manifest bytes, bounded by the manifest ceiling.
    Inline(Vec<u8>),
}
```

`ContainerKind` is a closed set: deterministic ZIP, directory, and single file. Directory input
admits instance folders from other launchers and managers without a round trip through an
archive. A format whose container is none of these is a documented native exception, reviewed
like a native provider, and its output must still pass the host's hash and manifest verification.

`probe` reads bounded leading bytes and entry names through `PackInput`. File extension is a
hint, never the only detector. Probing cannot access the network, mutate state, or read past its
bound.

`PackInput` maps directly onto host imports and every record is serializable, so a sandboxed WASM
codec can implement this contract unchanged (§17.3).

### Import records

```rust
pub struct PackImportPlan {
    pub codec: String,
    pub title: Option<String>,
    pub origin: PackOrigin,
    pub target: ImportedTarget,
    pub environment: Vec<EnvironmentRequirement>,
    pub requirements: Vec<PackRequirement>,
    pub embedded: Vec<EmbeddedBlob>,
    pub warnings: Vec<PackWarning>,
}

/// The pack an import came from, recorded as the profile's pack layer (§17.5).
pub struct PackOrigin {
    pub codec: String,
    /// The pack's identity across versions, when the format declares one.
    pub pack: Option<String>,
    pub version: Option<String>,
    /// Digest of the input as read.
    pub digest: Digest,
}

pub struct PackRequirement {
    /// Exact bytes, when the format pins them.
    pub digest: Option<Digest>,
    pub hashes: BTreeMap<String, String>,
    pub destination: Option<RelPath>,
    pub side: Availability,
    /// Installer answers carried by the format (§17.5).
    pub answers: InstallAnswers,
    /// Where the bytes may be obtained, in preference order.
    pub sources: Vec<RequirementSource>,
}

pub enum RequirementSource {
    Provider { package: PackageId, version: Option<String> },
    Direct { urls: Vec<String> },
    UserAction { provider: String, reference: String, reason: String },
}
```

A requirement is one set of bytes with ordered alternatives, not one source. The same file is
often published on several hosts. Import tries each source in order and accepts the first whose
bytes match every declared hash, so a file removed from one host still resolves from another.

The orchestration layer sends `Provider` and `Direct` sources back through the provider registry.
A codec never downloads an artifact during parsing. `UserAction` is a normal result for
browser-assisted or distribution-restricted content.

`environment` lists bytes the pack expects from the user's own installation (§17.5). Import
verifies them before acquiring anything.

Embedded files are declared by entry path and ingested by the host through the normal archive
limits and CAS; their bytes never pass through the codec. Their destination paths are validated
`RelPath` values and still pass through plan resolution; a codec cannot write into a game
directory.

### Export context

```rust
pub struct PackExportContext<'a> {
    pub game: &'a LockedPlan,
    pub target: &'a LockedTarget,
    pub lockfile: &'a Lockfile,
    pub files: &'a [PackFile],
    pub observations: &'a Observations,
    pub inclusion: &'a PackInclusion,
}

pub struct PackFile {
    pub path: RelPath,
    pub digest: Digest,
    pub role: PackFileRole,
    pub layer: String,
    /// False for artifact files a plan consumes without placing, such as injection inputs.
    pub deployed: bool,
    pub source: BlobSource,
    pub distribution: DistributionDecision,
}

/// The host's §17.10 decision, made before the codec runs.
pub struct PackInclusion {
    pub embed: BTreeSet<Digest>,
    pub permitted: BTreeSet<Digest>,
    pub requirements: Vec<PackRequirement>,
    pub environment: Vec<EnvironmentRequirement>,
}
```

`inclusion` is decided by `msbe-pack`, not by the codec. A codec that cannot represent a reference
may embed that digest instead, which is how a format restricts `blob-mode`, but only when
`permitted` contains it. The host rejects a plan or layout that embeds any other digest before a
byte is written.

`PackFileRole` distinguishes provider artifact, pack-owned config, local artifact, generated
output, loader component, and other resolved content. This classification is produced while the
lockfile is built; exporters must not infer ownership from paths such as `config/` or `mods/`.

`distribution` is not read from the lockfile. The planner decides it from the license facts the
lockfile recorded and the current observations (§17.5).

`BlobSource` states how the exact digest can be reproduced:

```rust
pub enum BlobSource {
    Provider { provenance: Provenance, exact: bool },
    Direct { urls: Vec<String>, hashes: BTreeMap<String, String> },
    Local,
    PackOwned,
    Environment { root: String, path: RelPath },
    Derived { inputs: Vec<Digest>, transform: TransformId },
    Component { id: String, version: String },
    Unknown,
}
```

Whether a source can still be acquired is an observation, not part of the source. A provider
reference counts as reproducible only when it identifies an exact release/file and has a verified
digest. A project slug or mutable URL is not enough.

## 17.5 Reproducibility model

A codec can export only what the lockfile records, so pack correctness rests on one invariant:

> Every input that affects a deployed digest has a content identity in the lockfile. Nothing that
> can change without the user changing the profile is stored in it.

The first half makes strict export and native import possible. The second keeps lockfiles
diffable and deterministic: resolving the same intent against the same inputs produces the same
bytes on any day. Each subsection below closes a place where a lockfile would otherwise break one
half. They are schema facts, cheap to record when content enters a profile and unrecoverable
afterwards: a lockfile written without them can only be repaired by guessing.

### Profile lineage

An imported pack is an upstream, not a one-time copy. A profile records ordered layers:

```toml
[[layer]]
id = "pack"
kind = "pack"
codec = "modrinth-mrpack"
pack = "example-pack"
version = "1.8"
digest = "sha256:..."

[[layer]]
id = "user"
kind = "changes"
```

Every locked module, config, and order entry names the layer that introduced it. A `pack` layer is
replaced as a whole. The `changes` layer records operations against the layers beneath it (add,
remove, pin, disable, reorder, and config patch) rather than a flattened result. A profile that
never imported a pack has only a `changes` layer.

Updating a pack re-imports its layer at the new version and reapplies the `changes` layer. A change
that no longer applies, such as a patch to a config the pack rewrote or a pin on a mod the pack
removed, is reported as a `LayerConflict` for the user to resolve. It is never silently dropped or
silently kept. Exports may flatten layers; the native format preserves them.

Layers are recorded at import. An import that flattens a pack into an unlayered profile discards
the one fact an update needs.

### Environment inputs

Some deployed bytes derive from the user's own installation: a jarmod output built from the
vanilla client jar, a patch applied to a base game master file, a delta against a shipped
executable. Those inputs are `BlobSource::Environment { root, path }` with their digest. They are
never embedded, never acquired from a provider, and never classified as `Unknown`.

`LockedTarget` also pins the installation fingerprint the lockfile was solved against: the
detected edition or store variant and the digest of each file the plan declares identifying,
typically the main executable ([08](08-platforms-and-detection.md)). A version string alone is not
an identity. Two store editions of one game can share a version and differ in exactly the bytes
that script extenders and patches depend on.

A plan may limit its fingerprint and environment inputs to the loaders whose steps read them, so a
profile that never derives from the installation neither pins nor requires those files. The
Minecraft plan limits both to `jarmod`.

Import verifies the fingerprint and every environment input before acquiring anything. A mismatch
is `EnvironmentMismatch`, naming the expected and found digests, rather than an integrity failure
after a long download.

### Derivation identity

`BlobSource::Derived` records what produced an output, not only what went in:

```rust
pub struct TransformId {
    /// Digest of the pinned plan, not only its version string.
    pub plan: Digest,
    /// The step within the plan.
    pub step: String,
    /// Digest of each extension the step ran (§17.6).
    pub extensions: Vec<Digest>,
    /// Digest of the normalized step parameters and installer answers.
    pub parameters: Digest,
    /// Digest of external data the step read, such as a load-order masterlist revision.
    pub data: Vec<Digest>,
    /// Whether the step declares byte-identical output for an identical identity.
    pub deterministic: bool,
}
```

`LockedPlan` gains the plan digest for the same reason. A derived blob may be omitted from an
export only when its transform is deterministic and every input, extension, and data digest is
reproducible; otherwise it is unsourceable output (§17.10). When re-derivation on import produces
different bytes, `DerivationMismatch` names the part of the identity that changed.

### Facts and observations

The lockfile holds facts that stay true: digests, exact references, the license and distribution
terms declared when the bytes were acquired, and the date they were recorded. Whether a source can
still be acquired, whether a provider currently permits redistribution, and whether a known-bad
entry now applies are observations.

Observations live in a dated cache beside the store. Export planning refreshes the ones it needs,
and nothing ever writes them into the lockfile. `currently_acquirable` and the live distribution
decision therefore leave `BlobSource` and the locked file classification. An export plan records
which observations it relied on and when each was taken, so a preview that predates a provider
change is visibly stale.

### Installer answers

A module's resolved install shape includes the choices made while installing it: FOMOD
selections, optional-file picks, and extension questions. The locked module records them as
`InstallAnswers`, keyed by plan step and question ID and opaque to core. Codecs carry them through
`PackRequirement::answers`, so a format that records choices, such as a curated collection, never
has to drop them, and replaying an install with recorded answers asks no questions.

### Capture

Games write into mutable paths after deployment, and pack authors routinely tune configs in-game
before exporting. Capture turns those changes into profile content:

1. Compare every file under the plan's mutable globs and target roots with the deployment record.
2. Present each changed or new file with its diff, proposed role (`PackOwnedConfig` by default),
   and the `changes` layer as its destination.
3. On confirmation, ingest the accepted files into the store and record them as layer operations.

Capture is always explicit. Export never captures implicitly, and capture never adopts a file
outside the plan's declared roots.

## 17.6 Extension identity

Plans, provider programs, pack codecs, WASM step extensions, component bundles, and external data
such as masterlists all change deployed bytes. They share one envelope instead of one trust and
versioning story each:

```toml
[extension]
id = "..."
version = "1.4.0"
digest = "sha256:..."          # normalized package contents
provides = ["plan"]            # closed set: plan, provider-program, codec, wasm-step, component, data
host-api = ">=3, <4"           # supported host contract range
capabilities = []              # closed vocabulary; empty for pure extensions
signer = "..."
```

The loader validates the envelope the same way for every kind: signature and signer trust,
revocation, host API range, requested capabilities against the kind's permitted set, and digest.
Kind-specific validation, such as plan schema, provider-program vocabulary, or codec conformance,
runs afterwards. Key rotation and revocation are defined once, and the registry
([10](10-registry.md)) is the distribution channel for every kind.

Lockfiles pin extensions by digest wherever they affected a result: the plan, every extension a
derivation ran, and the data it read (§17.5). Export records the codec's digest alongside its ID
and schema version. Native registrations compiled into MSBE carry the same envelope with the MSBE
build as signer, so a native exception is visible as one.

## 17.7 Game and loader identities

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

## 17.8 Option schemas

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

- codec ID, digest, and schema version;
- normalized option values, including defaults;
- selected preset, if any;
- MSBE version and lockfile schema;
- the observations the plan relied on and when each was taken;
- warnings acknowledged by the user.

This makes an export invocation reproducible and lets the CLI print the exact equivalent of a
Desktop selection.

### Common options

`msbe-pack` prepends common policy fields to every codec schema:

| Key               | Values                                       | Meaning                                                              |
| ----------------- | -------------------------------------------- | -------------------------------------------------------------------- |
| `purpose`         | `distribute`, `private-transfer`             | Redistribution policy and warning posture; backups are snapshots.   |
| `reproducibility` | `strict`, `allow-user-action`, `best-effort` | Controls whether unresolved exact content fails export.              |
| `blob-mode`       | `thin`, `portable`, `complete`               | Default embedded-blob selection; codecs may restrict it.             |
| `on-forbidden`    | `error`, `external-requirement`              | Never permits embedding; chooses failure or an explicit requirement. |
| `compression`     | codec-declared choices                       | Compression affects bytes and size, never logical content.           |
| `deterministic`   | `true` by default                            | Requires normalized ordering, timestamps, permissions, and metadata. |

External codecs append format-specific choices. A Modrinth codec may expose pack version,
client/server environment defaults, and override inclusion. It may not expose a switch that
weakens provider policy. A codec field that redeclares a common key is a schema error.

`purpose` defaults to `private-transfer`: embedding content whose redistribution rights are unknown
is allowed there with a warning, and public distribution is an explicit choice. `compression` offers
`deflate` and `store`. Schema-1 hosts write only deterministic archives, so `deterministic = false`
is rejected as `InvalidOptions` rather than silently ignored.

### Presets

`msbe-pack` also prepends four common presets, available with every codec:

- **`thin`**: embed nothing and reference everything; strict, so content without an exact source
  blocks the export.
- **`portable`**: embed pack-owned configs and content without an exact source; reference exact
  provider files.
- **`complete`**: embed every blob policy permits for a private transfer. A personal backup of
  everything is an instance snapshot (§17.10), not a preset.
- **`public-distribution`**: `portable` selection with `purpose = distribute`, so unknown or
  prohibited redistribution fails the export.

Presets are convenience only. The normalized field map is authoritative.

## 17.9 Native `.msbepack` format

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
    observations.toml           observations the export relied on, with dates
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
plan_digest = "sha256:..."

[compatibility.fingerprint]
edition = "..."
identifying = { "..." = "sha256:..." }
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
- explicit file role/source metadata added by the lockfile schema revision;
- profile layers and the layer that introduced each entry;
- the installation fingerprint and every environment input;
- derivation identities;
- installer answers.

The lockfile remains independently usable outside the archive.

### `requirements.toml`

Requirements explain every digest not embedded in the bundle:

```toml
schema = 1

[[requirement]]
digest = "sha256:..."
destination = "mods/example.jar"
side = "required"
hashes = { sha512 = "..." }

  [[requirement.sources]]
  kind = "provider"
  package = { provider = "modrinth", project = "..." }
  version = "..."

  [[requirement.sources]]
  kind = "direct"
  urls = ["https://..."]

[[requirement]]
digest = "sha256:..."
side = "required"

  [[requirement.sources]]
  kind = "user_action"
  provider = "nexus"
  reference = "..."
  reason = "Free-account download requires browser confirmation"

[[environment]]
root = "game"
path = "versions/1.5.2/1.5.2.jar"
digest = "sha256:..."
```

Sources are alternatives for the same bytes, in preference order (§17.4). Environment entries are
never satisfied by a download. The root `game` names the instance's game directory. A provider
source is acquired through that provider's release metadata when it publishes one, or by routing its
project reference when the reference is itself a source, as a pinned direct URL is.

A thin bundle may contain no blobs. A portable bundle normally embeds configs, local files, and
other exact content with no stable source. A complete bundle attempts to embed all required
blobs that policy allows.

Native import first verifies the installation fingerprint and environment inputs, then embedded
blobs, then resolves omitted requirements through each requirement's sources in order, then
derives deterministic outputs, and finally checks that every deployment digest in the lockfile is
available. A mismatch is an integrity failure, never an opportunity to rewrite
the lockfile.

## 17.10 Blob inclusion and redistribution

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
unknown distribution rights by default. They may be included in a private transfer after an
explicit acknowledgement and are always included in a snapshot, but public distribution requires affirmative license or provider
policy metadata. A provider's distribution prohibition is not user-overridable.

Derived blobs may be omitted only when every input is available and the derivation is declared
deterministic under the pinned plan version. Otherwise they are treated as unsourceable output.

`reproducibility = strict` means import can reach every lockfile digest without substituting a
new release. It does not mean MSBE ignores distribution rules. If strict reproduction cannot be
packaged legally, export fails with the exact blocking files and available alternatives.

Environment inputs (§17.5) are never embedded and never acquired. Every export emits them as
environment requirements. They do not block strict export, because import verifies them against
the recipient's installation before doing anything else.

### Instance snapshots

A backup is not an export. Hosts remove files regularly, and a backup that must omit whatever a
provider forbids redistributing fails exactly when it is needed. An instance snapshot is therefore
a separate operation, not a codec:

- it contains the profile, its layers, the lockfile, the observation cache, and every store blob
  the lockfile references, regardless of distribution terms;
- it is written only to a user-chosen local path and marked non-distributable in its manifest;
- `pack export` never produces one and `pack import` never accepts one;
- restore verifies every blob digest and writes only MSBE state; deployment remains a separate,
  previewed step.

MSBE cannot stop a user from copying a snapshot. That is why a snapshot has no pack-format
identity: nothing in MSBE presents one as shareable.

## 17.11 Export planning and execution

Export is always two-phase:

1. Load or derive the canonical lockfile.
2. Refresh the observations the plan needs, recording when each was taken.
3. Classify every required digest by role, layer, source, derivability, and distribution decision.
4. Select a codec from the requested ID or output extension.
5. Fetch its descriptor and option schema.
6. Normalize and validate options.
7. Ask the codec for a `PackExportPlan`.
8. Merge codec requirements with the neutral blob-inclusion plan.
9. Store the merged plan in the daemon under a plan ID and plan digest, then present files,
   references, environment inputs, embedded bytes, estimated size, warnings, blockers, and
   observation ages.
10. On confirmation of that plan ID and digest, run export as a job: ask the codec for its layout,
    stream entries to a temporary output while hashing, verify the declared manifest, and
    atomically rename the output.

`PackExportPlan` contains no open files or callbacks and is serializable, so Desktop preview and
CLI `--dry-run` show the exact operation that execution uses.

Clients receive plans to display, never to submit. Execute takes the plan ID and digest, and the
daemon runs its own stored copy. A plan is invalidated when its lockfile, options, codec, or
relied-on observations change; executing an invalidated plan fails with `StalePlan`. A client
therefore cannot add a forbidden file to `embedded` or alter `codec_state` between preview and
execution.

The daemon owns execution because it owns lockfiles, CAS access, provider policy, and serialized
mutation. Clients choose options and output destinations only.

## 17.12 Import planning and execution

Import follows the inverse flow:

1. Open the input through the host, enforcing §17.15, and probe every permitted codec.
2. Refuse ambiguous matches, where more than one codec reports the highest confidence, and report
   the candidate codec IDs.
3. Parse with the selected codec into `PackImportPlan` and store it under a plan ID and digest.
4. Resolve target compatibility against a selected instance/profile, a create-profile request,
   or the existing pack layer being updated.
5. Verify the installation fingerprint and environment inputs.
6. Send each requirement's sources through the normal resolver and policy gate, in preference
   order.
7. Queue browser/user-action requirements without pretending they are failures.
8. Verify and ingest embedded blobs into CAS.
9. Require every locked digest to be present or reproducible.
10. Only after the complete plan validates, create the profile, or replace the pack layer and
    reapply the `changes` layer, reporting every `LayerConflict`.
11. Preview deployment separately; import never writes the game directory.

Import execution is a job ([03](03-architecture.md)). Acquisition reports progress, installer
questions without recorded answers arrive as job questions, and cancellation leaves no partial
profile.

External pack manifests that do not pin exact versions or hashes produce an ordinary resolution
request, not a false lockfile. Native `.msbepack` import preserves its lockfile exactly.

## 17.13 CLI, RPC, and Desktop surfaces

The generic CLI surface is:

```text
msbe pack formats [--direction import|export] [--game GAME]
msbe pack options CODEC [--preset PRESET] [--direction import|export]
msbe pack import INSTANCE INPUT [--codec CODEC] [--options FILE] [-p PROFILE] [--dry-run]
msbe pack update INSTANCE INPUT [--codec CODEC] [-p PROFILE] [--resolve CONFLICT=keep|drop]... [--dry-run]
msbe pack export INSTANCE OUTPUT --codec CODEC [--preset PRESET] [--options FILE] [-p PROFILE] [--dry-run]
msbe pack capture INSTANCE [-p PROFILE] [--path PATH]... [--dry-run]
msbe pack validate INSTANCE [-p PROFILE]
msbe snapshot create INSTANCE OUTPUT
msbe snapshot restore INPUT [--dry-run]
```

`pack update` replaces a profile's pack layer with another version of the same pack and reapplies
its `changes` layer (§17.5). A conflict ID is `mod:NAME`, `config:PATH` or `order`; an update with an
unresolved conflict exits with the conflict code and changes nothing.

`CODEC` is a codec ID such as `modrinth-mrpack` or `msbe-native`, and import detects it from the
input when omitted. The flag is `--codec` because `--format` is already the global `human|json`
output flag. The CLI does not gain `--minecraft-version`, `--fabric-loader`, or other wire-specific
flags. Format-specific values live in the TOML options document, typed by the codec's schema.
`--dry-run` prints the preview and exits with the code of its first blocker: 6 for a distribution
refusal, 7 for an integrity or environment mismatch, 4 for a layer conflict.

RPC adds descriptor, schema, preview, and execute methods rather than tunneling opaque CLI text:

```text
pack.codec.list
pack.codec.options
pack.import.preview     -> { plan_id, plan_digest, plan }
pack.update.preview     -> { plan_id, plan_digest, plan }
pack.export.preview     -> { plan_id, plan_digest, plan }
pack.capture.preview    -> { plan_id, plan_digest, plan }
pack.import.execute     { plan_id, plan_digest }            # job
pack.update.execute     { plan_id, plan_digest }            # job
pack.export.execute     { plan_id, plan_digest }            # job
pack.capture.execute    { plan_id, plan_digest }            # job
snapshot.create         { instance, output }                # job
snapshot.restore        { input }                           # job
```

Execute and snapshot methods run only through `job.start` ([03](03-architecture.md)); calling one
directly is refused. Jobs run one at a time on a worker thread, report progress through
`job.events { job_id, after }`, a cursor poll whose consecutive progress events coalesce, and cancel
cooperatively without leaving a partial profile or output. While a job runs it holds the instance
state, so previews and `command.run` answer busy (`-32020`) instead of observing a half-applied
operation. A pack failure answers `-32030` with `data.code`, the stable §17.14 code, and
`data.issues`. Installer questions have no producer in pack workflows yet, so `job.answer` is not
part of contract 4.

Desktop renders `PackOptionSchema` with native controls, provides presets, and shows an export
preview grouped into provider references, user actions, environment inputs, embedded configs,
embedded local content, derived content, and policy blockers, with the age of every observation
the plan relied on. Pack update lists each `changes` operation that no longer applies. Desktop
never contains a Modrinth-specific ViewModel.

## 17.14 Error model

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
- `LimitExceeded`
- `EnvironmentMismatch`
- `DerivationMismatch`
- `LayerConflict`
- `StalePlan`
- `UntrustedExtension`
- `CodecFailure`

Errors name the codec and affected package/path/digest. Raw parser or provider error text is
retained as diagnostic detail but is not the public contract. Two host codes accompany the list:
`Cancelled` for a job the caller stopped, and `HostFailure` for instance-state or filesystem
failures that are not pack failures.

## 17.15 Security limits

The host enforces these limits while opening an input, before any codec code runs (§17.4). A codec
never sees an entry the host rejected and cannot raise a limit itself.

| Limit                                          | Default | Status                                                         |
| ---------------------------------------------- | ------- | -------------------------------------------------------------- |
| Entries per input                              | 100,000 | Implemented: `msbe_archive::Limits::max_entries`               |
| Expanded bytes per entry                       | 4 GiB   | Implemented: `Limits::max_file_bytes`                          |
| Expanded bytes per input                       | 16 GiB  | Implemented: `Limits::max_total_bytes`                         |
| Compression ratio, entries over 1 MiB          | 1,000:1 | Implemented: `Limits::max_ratio`                               |
| Manifest ceiling per entry read by a codec     | 16 MiB  | Implemented by the host-owned `PackInput` read bound            |
| Download without a declared size               | 2 GiB   | Implemented: `msbe_provider_api::DOWNLOAD_LIMIT`               |
| Archive nesting opened by the host             | 0       | Proposed                                                       |
| Path length                                    | 1 KiB   | Proposed                                                       |
| Path components                                | 64      | Proposed                                                       |

A codec descriptor may declare higher entry and expanded-byte ceilings for its format, up to hard
maximums defined as host constants; complete bundles of large Bethesda profiles legitimately exceed
16 GiB. The registry validates declared ceilings at load time. Pack content, options, and clients
never raise a limit, and the ratio, nesting, and path rules have no override.

The host never opens an archive nested inside an input. A nested archive is an ordinary blob, and a
plan step that later extracts it applies these limits again.

Every input and output also obeys these rules:

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

## 17.16 Migration from the current implementation

Migration is incremental, but the end state is non-negotiable.

### Verified boundary audit (2026-09-12)

The Rust workspace was audited for explicit Modrinth names and implicit assumptions such as
`.mrpack` paths, provider wire fields, loader dependency keys, API endpoints, hash choices, and
release-channel behavior. The production violations it found, and how each was resolved:

| Location                                    | Violation                                                                                                                                                                      | Required owner                                                                                                                     |
| ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------- |
| `msbe-pack`                                 | Resolved in Phase B: the crate owns only provider-neutral host container utilities and the unregistered native codec. Modrinth wire records, `.mrpack` detection, and export layout live in `msbe-provider-modrinth`; CurseForge records were removed. | Phase B complete. |
| `msbe-cli::pack_import`                     | Resolved in Phase D: import previews and executes `msbe_pack` plans; codecs are detected through the registry, and requirements are acquired through the provider policy gate. | Phase D complete. |
| `msbe-cli::pack_export`                     | Resolved in Phase D: export selects a codec by ID, normalizes its schema, and writes only the policy-gated `PackExportPlan`.                                                   | Phase D complete. |
| `msbe-cli::loader_dependency`               | Resolved in Phase B: the mapping is private to the Modrinth codec.                                                                                                             | Phase B complete. |
| `msbe-cli::UpdateReport` | Resolved in Phase E: the provider-specific update field is now the provider-neutral `not_updatable`.                                                 | Phase E complete.   |
| CLI help and pack errors                    | Resolved: pack help comes from descriptors, failures carry §17.14 codes, and `add` help and Desktop copy describe sources without naming a provider.   | Phase E complete. |

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
explicit architecture review. Phase E added the automated guard,
`scripts/development/check-architecture.sh`, which CI runs over the generic crates and Desktop
sources.

### Contract audit (2026-09-12)

The Phase A contracts were reviewed against §17.4 through §17.6 before the A2 revision. A2
resolved the contract entries below, and Phase D resolved the plan-binding, daemon and capture
entries.

| Location                                           | Gap                                                                                                                                                                                                  | Resolved by                                             |
| -------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------- |
| `msbe-provider-api::codec::PackCodec`              | Resolved: codecs now receive bounded `PackInput` views and produce `PackLayout`; `msbe-pack` owns ZIP validation and deterministic writing.                                                        | `PackInput` and `PackLayout` (§17.4).                   |
| `msbe-provider-api::codec::PackRequirement`        | Resolved: requirements carry an optional exact digest, ordered source alternatives, and installer answers.                                                                                           | Per-digest requirements with ordered sources (§17.4).   |
| `msbe-provider-api::codec::PackExportPlan`         | Resolved: clients never submit a plan. The daemon holds each preview under a plan ID and digest, runs it once, and re-plans before executing so any drift fails as `StalePlan`.                     | Daemon-held plan IDs and digests (§17.11).              |
| `msbe-core::instance::LockedFileClassification`    | Resolved: the lockfile retains only role and source facts; dated live state is represented by export observations.                                                                                   | Facts and observations (§17.5).                         |
| `msbe-core::instance::BlobSource`                  | Resolved: installation-owned inputs use `Environment`; derived outputs name `TransformId` when relocked.                                                                                            | `Environment` and `TransformId` (§17.5).                |
| `msbe-core::instance::LockedPlan`, `LockedTarget`  | Resolved: plan digest and optional installation fingerprint are lockfile facts.                                                                                                                      | Plan digest and fingerprint (§17.5).                    |
| `msbe-core::instance::LockedModule`, `Profile`     | Resolved: modules record their introducing layer and installer answers; profiles retain layer records.                                                                                               | Profile lineage and installer answers (§17.5).          |
| `msbe-daemon::handle`                              | Resolved: long operations are jobs on a worker thread; the listener keeps answering progress and cancellation.                                                                                        | Jobs ([03](03-architecture.md)), Phase D.               |
| `[deploy] mutable` paths                           | Resolved: `pack capture` adopts changed and new files beneath the plan's mutable roots into the changes layer.                                                                                        | Capture (§17.5).                                        |

### Phase A - neutral contracts

Phase A is implemented. The native codec remains intentionally unregistered until Phase D, and
the existing external formats remain on their temporary paths until Phase B.

1. [x] Add `PackCodec`, descriptors, plans, option schemas, file roles, source classifications, and
       distribution decisions to `msbe-provider-api` or a smaller neutral API crate if dependency
       direction requires it.
2. [x] Extend provider `Registration` and `Providers` with codec registration and lookup.
3. [x] Extend the lockfile schema with explicit file role/source data needed for blob planning.
4. [x] Add native codec conformance fixtures and deterministic archive tests.

### Phase A2 - reproducibility contract revision

Phase A2 is implemented. It revised the Phase A contracts before any external format moves onto
them, so Phase B migrates each format once.

1. [x] Replace `ReadSeek`/`WriteSeek` codec I/O with host-owned `PackInput` and `PackLayout`, and move
   container reading, writing, limits, and determinism into the host.
2. [x] Make `PackRequirement` per-digest with ordered sources and installer answers; add `PackOrigin`
   and environment requirements to `PackImportPlan`.
3. [x] Revise the lockfile schema: plan digest, installation fingerprint, `BlobSource::Environment`,
   `TransformId`, installer answers, and layer attribution. Remove `currently_acquirable` and the
   live distribution decision.
4. [x] Add the dated observation cache and record relied-on observations in export plans.
5. [x] Read the previous lockfile schema conservatively: a profile without layers becomes one
   `changes` layer, a missing fingerprint is detected and recorded at the next lock with a
   warning, and a derived blob without a transform identity is unsourceable until relocked.
6. [x] Extend native codec conformance fixtures to layered profiles, environment inputs, and derived
   outputs.

### Phase B - move existing formats

Phase B is implemented. Existing external pack behavior now enters generic clients through the
reviewed codec registry.

1. [x] Move Modrinth wire structs and `.mrpack` manifest handling from `msbe-pack` into
   `msbe-provider-modrinth` on the Phase A2 contract. ZIP reading and writing stay in the host.
2. [x] Move Minecraft/loader dependency mapping from `msbe-cli` into Modrinth codec-owned mapping.
3. [x] Remove CurseForge wire structs from generic code; reintroduce them only with the reviewed
   CurseForge adapter and codec.
4. [x] Reduce `msbe-pack` to host-owned container utilities and provider-neutral native codec support.
5. [x] Replace CLI format branches with codec lookup.

### Phase C - declarative provider runtimes

Phase C provider-program execution is implemented. `direct-url-v1` runs as a signed declarative
program; Modrinth remains a reviewed native exception because its bulk update and release
protocol semantics are not yet representable by `catalog-v1`. The shared envelope is active for
provider programs. Compiled native registrations and codecs carry reviewed identity metadata and
are pinned in native export lockfiles; loading every extension kind through the signed envelope
remains the next extension-identity migration.

1. [x] Define the signed provider-program schema and its closed vocabularies for source recognition,
   routes, typed record mappings, compatibility, dependency relations, acquisition, and policy.
2. [x] Implement bounded `catalog-v1` and direct-URL runtimes plus fixture-based conformance tests.
3. [x] Load trusted provider programs through `msbe-providers`, enforcing signer-key trust,
  revocation, payload digests, Ed25519 signatures, supported host API ranges, and closed runtime
  capabilities. Daemon descriptor reporting follows the typed provider RPC work in Phase D.
4. [x] Convert the declarative subset of existing providers to programs. Retain native adapters only
   for documented protocol, update, authentication, or policy semantics the runtime cannot model.
5. [x] Require every native provider registration to state its exception reason and run the same
   neutral-record, transport, acquisition, and policy conformance suite.
6. [~] Load plans, provider programs, and native registrations through the shared extension envelope
  (§17.6), and pin extension digests wherever they affect a lockfile. Native registrations and
  codecs now declare host-compatible identities whose canonical descriptors are pinned in native
  export lockfiles and verified on import. Plan-declared installation fingerprints/environment
  inputs and stable derivation transform identities are also recorded. Signed envelopes for plans
  and compiled native extensions remain outstanding.

### Phase D - native bundles and clients

Phase D is implemented. The native bundle joins through a reviewed registration, every pack
operation is previewed before it runs, the daemon holds the previewed plan and runs it only as a
job, and Desktop renders codec schemas instead of naming formats.

1. [x] Create `msbe-provider-local` and register `msbe-native` through it. The codec left
   `msbe-pack`. It honours the host's inclusion decision, records environment inputs and the
   observations an export relied on, and declares embedded blobs by entry path so the host ingests
   and verifies their bytes.
2. [x] Add thin, portable, complete, and public-distribution presets. They are common presets over
   the §17.8 policy fields, applied by the §17.10 inclusion planner in `msbe-pack`.
3. [x] Add the daemon job model and typed pack RPC with daemon-held plan IDs and digests
   (contract 4, §17.13).
4. [x] Add pack update over profile layers, with `LayerConflict` reporting. An imported pack layer
   records its base, the changes layer is the difference between that base and the profile, and a
   change the new version invalidates must be resolved as `keep` or `drop` before the update runs.
   Unchanged modules are reused rather than downloaded again.
5. [x] Add capture and instance snapshots. Capture adopts only files beneath the plan's mutable
   roots, shows a line diff for small text files, and re-verifies each digest before recording.
   Snapshot restore verifies every blob and stages instance state beside its destination before one
   rename commits it.
6. [x] Render codec schemas, export previews, observation ages, and layer conflicts in Desktop.
7. [x] Keep the current command-run bridge only as a compatibility path until typed RPC ships. Pack
   workflows use typed RPC; `command.run` remains for surfaces without typed methods.

Carried forward:

- Export planning reads the dated observation cache beside the store but does not refresh it. No
  adapter exposes an availability or redistribution probe yet, so provider content without an
  observation has unknown distribution rights.
- The lockfile writer pins the normalized plan digest and records plan-declared installation
  fingerprints and environment inputs. Stable IDs on inject/edit-json derivations produce
  deterministic `TransformId`s; legacy or unnamed derivations retain a deterministic fallback ID.
  Native exports also pin their reviewed provider and codec identities; import checks a non-legacy
  native bundle's pin set against the running build before acquiring anything. Import verifies
  whichever of these a lockfile carries, before acquiring anything.
- Installer questions have no producer in pack workflows, so `job.answer` is not in contract 4.
- The daemon serves Unix domain sockets only.

### Phase E - removal

This is a private project: Phase E deliberately does not add compatibility handling for legacy
lockfiles or removed APIs.

1. [x] Continue importing existing `.mrpack` archives through the relocated codec.
2. [x] Remove `loader_dependency`, hardcoded `minecraft`, `Pack::Modrinth`,
   `Pack::CurseForge`, and `export_modrinth` from generic crates.
3. [x] Add CI guards forbidding provider, game, loader, and format literals in generic runtime
   crates and Desktop sources, outside provider-specific crates and end-to-end fixtures.

### Phase F - sandboxed codecs

1. Host `PackCodec` in the WASM runtime with `PackInput` as its only import and no network,
   filesystem, clock, or randomness.
2. Load WASM codecs through the extension envelope and run the conformance suite native codecs
   run.
3. Deliver new third-party formats as WASM codecs. A new native codec requires a documented
   exception, as a native provider does.

## 17.17 Acceptance criteria

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
- no generic client or core crate branches on a provider, game, loader, or external format ID;
- codecs never open or write containers, and host limits reject oversized, over-ratio, or unsafe
  inputs before any codec code runs;
- resolving the same intent against the same inputs on different days produces a byte-identical
  lockfile;
- updating an imported pack preserves `changes` operations and reports every one that no longer
  applies;
- native import on a different installation fails with `EnvironmentMismatch` before acquisition,
  and succeeds on a matching one;
- every derived blob names its transform, and an export omits one only when that transform is
  deterministic and all of its inputs are reproducible;
- installer answers survive export and import, so replaying an install asks no questions;
- an execute request cannot run any plan other than the one previewed;
- a snapshot restores every blob the lockfile references, including content that no export may
  embed.

## 17.18 Deliberate limits

The architecture has four behavioral extension kinds: plans, provider programs, pack codecs, and
WASM step extensions, each with a documented native exception path. Component bundles and data
files share the envelope (§17.6) but carry no behavior. New requirements fit one of these. The
following are out of scope on purpose:

- **No fifth extension kind.** A proposal that seems to need one is first checked against the
  existing four; most turn out to be a plan step, a provider-program vocabulary term, or a codec.
- **No merger of codecs and provider programs.** Acquisition and format translation remain
  separate capabilities with separate registrations (§17.3), even when one provider supplies both.
- **No general schema language for options.** The option-schema vocabulary stays closed (§17.8).
  A format that needs richer validation performs it in `plan_export` and reports `InvalidOptions`.
- **No codec-decided topology.** Plans route files; codecs translate records (§17.1).
- **No codec-owned containers** outside the documented native exception (§17.4).
- **No executable behavior from TOML.** An unknown or uncompiled manifest never activates a parser,
  runtime, or codec (§17.3).
- **No policy weakening** through options, pack content, or snapshots presented as packs (§17.10).
- **No time-varying data in lockfiles** (§17.5).
