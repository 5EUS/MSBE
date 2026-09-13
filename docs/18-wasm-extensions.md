# 18 - WebAssembly extensions

Two of MSBE's four behavioral extension kinds ([17 §17.18](17-pack-formats-and-native-bundles.md))
run as sandboxed WebAssembly: **plan step extensions**, which install what a declarative plan cannot
express, and **pack codecs**, which translate third-party pack formats. Both follow the same rules,
and both are implemented:

| Kind                | ABI                 | Host crate        | Guest SDK          | Example                             |
| ------------------- | ------------------- | ----------------- | ------------------ | ----------------------------------- |
| Plan step extension | `msbe-plan-step-1`  | `msbe-plan-host`  | `msbe-step-guest`  | `extensions/steps/option-installer` |
| Pack codec          | `msbe-pack-codec-1` | `msbe-wasm-codec` | `msbe-codec-guest` | `extensions/codecs/pack-list`       |

## 18.1 Shared sandbox rules

**Core modules, JSON records.** An extension is a core WebAssembly module built for
`wasm32-unknown-unknown`. Records cross the boundary as JSON in linear memory, in the shapes the
host's typed contracts define. There is no WASI and no component model: one toolchain, no
`wit-bindgen`, and a record vocabulary that already exists as serde types on both sides. Doc 02
originally proposed WASI components; the codec host showed that JSON over memory is enough, and a
second ABI style would buy nothing.

**Denial is structural.** The host links only the imports an extension's grant allows. A module that
imports anything else, including any WASI function, fails to load with an error naming the import,
before any of its code runs. There is no permission check to bypass: an ungranted capability does not
exist in the module's world.

**Nothing ambient.** No filesystem, network, process, clock or randomness is reachable. The engine
runs with fuel metering, no shared-memory threads, deterministic relaxed SIMD and canonical NaNs, so
identical inputs produce identical output on every machine. Fuel is the timeout: it is deterministic,
where a wall-clock timeout would let the same run pass on a fast machine and fail on a slow one.

**Calling convention.** Every call runs in a fresh instance. The host writes a request into memory
the module's `msbe_alloc(size) -> address` reserved, calls the entry point with `(address, length)`,
and reads back an `i64` that packs the response address in its high 32 bits and its length in the low
32. The response is `{"ok": value}` or `{"error": {"kind": ..., ...}}`. Memory handed to the host is
never freed; the instance is discarded.

**Host data.** A host import that returns bytes stages them and returns their length. The guest then
calls `take(address, capacity)` to copy them into its own buffer. A negative return is a code:

| Code | Meaning                                                        |
| ---- | -------------------------------------------------------------- |
| -1   | Not found                                                      |
| -2   | Over the read limit the guest passed                           |
| -3   | Unsafe path                                                    |
| -4   | Over the host's read budget                                    |
| -5   | Question pending (steps only)                                  |
| -6   | Denied: outside the grant (steps only)                         |
| -7   | Invalid request, or a recorded answer was refused (steps only) |

The guest SDKs wrap all of this; an extension author implements one trait and exports it with one
macro.

## 18.2 Plan step extensions

Roughly a tenth of games need real computation to install a mod: FOMOD's conditional trees, KSP's
ModuleManager patches, a load order derived from record conflicts. A `run-extension` step hands each
mod to a sandboxed module, and the module's only effect is the operations it returns.

### Declaring one

A plan declares each module it may run and exactly what that module is granted, then names it from a
step:

```toml
[[extensions]]
id           = "installer"
path         = "extensions/installer.wasm"   # relative to plan.toml
sha256       = "9f2c…"                        # pins the module bytes
capabilities = ["archive-read", "game-read", "ui-prompt"]
game_read    = ["Data/*.esm"]                 # required with game-read, refused without it
emit         = ["place", "write-file"]
roots        = ["@loader.targets.mods"]       # empty: every target of the selected loader

[[steps]]
type = "run-extension"
  [steps.with]
  id         = "install"                      # keys recorded answers and the lockfile transform
  extension  = "installer"
  loaders    = ["modern"]
  parameters = { into = "mods" }              # string values; may use {game_version}
```

Validation refuses an undeclared extension, a duplicate extension or step identifier, a malformed
SHA-256, `game_read` globs without the `game-read` capability or the capability without globs, an
empty or repeated `emit` list, and an unsafe path or target reference.

### Capabilities

| Capability     | Imports linked                              | What it allows                                                                     |
| -------------- | ------------------------------------------- | ---------------------------------------------------------------------------------- |
| (always)       | `msbe_host.take`, `msbe_host.log`           | Receiving staged bytes; up to 256 diagnostic lines                                 |
| `archive-read` | `msbe_archive.entries`, `msbe_archive.read` | Listing and reading the files of the mod being installed                           |
| `game-read`    | `msbe_game.read`                            | Reading game files matching `game_read`, as they were before MSBE changed anything |
| `ui-prompt`    | `msbe_ui.ask`                               | Asking installer questions, answered from recorded answers                         |

A game read outside the granted globs returns -6 and is not recorded. Network access is deliberately
not offered: a step whose output depends on a download cannot be reproduced from a lockfile.

### Request and response

`msbe_run` receives:

```json
{
  "step": "install",
  "module": "better-textures",
  "loader": "modern",
  "game_version": "1.21.1",
  "parameters": { "into": "mods" },
  "roots": ["mods"]
}
```

and returns operations:

```json
{
  "ok": {
    "operations": [
      { "kind": "place", "source": "options/high/texture.dds", "path": "mods/texture.dds" },
      { "kind": "write-file", "path": "mods/better-textures.choices.txt", "text": "textures=high\n" }
    ]
  }
}
```

The host refuses the whole result, and nothing is deployed, when any operation:

- is a kind the declaration's `emit` list does not include;
- has a path that is not a safe relative path, or does not lie beneath one of the roots;
- places a `source` the mod does not contain;
- writes a path another operation in the same run already wrote.

Accepted operations become ordinary deployment claims. Conflicts with other mods, mutable paths,
verification, rollback and purge apply to them exactly as to files a `place` step routed.

### Questions and answers

`msbe_ui.ask` takes a question:

```json
{
  "id": "textures",
  "prompt": "Texture resolution",
  "kind": "choice",
  "choices": [
    { "id": "standard", "label": "Standard" },
    { "id": "high", "label": "High" }
  ],
  "default": "standard"
}
```

Kinds are `choice` (one choice identifier), `multi` (distinct comma-separated identifiers), `boolean`
(`true` or `false`) and `text`. The host answers from the answers recorded on the mod for that step;
it never prompts during a run. A question with neither a recorded answer nor a default returns -5, and
the run fails listing every such question, however the extension handled the code. A recorded answer
the question refuses fails the run too.

Answers are recorded per mod, per step, and per question; an empty answer removes one:

```sh
msbe profile answer my-instance better-textures install/textures=high
```

They are stored in the profile, copied into the lockfile's `answers`, carried by native bundles, and
restored on native import, so replaying an install asks nothing
([17 §17.5](17-pack-formats-and-native-bundles.md)).

### Limits

| Limit                        | Default       |
| ---------------------------- | ------------- |
| Fuel per run                 | 2,000,000,000 |
| Linear memory                | 256 MiB       |
| Bytes read from mod and game | 1 GiB         |
| Operations per run           | 100,000       |
| Generated text per run       | 16 MiB        |
| Unanswered questions per run | 256           |
| Module size                  | 64 MiB        |
| Path, question or log line   | 64 KiB        |

### Reproducibility

Creating an instance reads every declared module from beside the plan, verifies its SHA-256, and pins
a copy at `extensions/<sha256>.wasm` in the instance directory. Deployment always runs the pinned
copy, verified again. Instance snapshots carry it, and restore it only when its name matches its
digest.

A placed file keeps the mod's own classification. A written file is `Derived`. Its inputs are every
file of the mod plus the digest of every game file the run read, and its transform identity is:

| `TransformId` field | Value                                                                                             |
| ------------------- | ------------------------------------------------------------------------------------------------- |
| `plan`              | Digest of the pinned plan                                                                         |
| `step`              | The step's `id`                                                                                   |
| `extensions`        | Digest of the module                                                                              |
| `parameters`        | Digest of the mod name, filled parameters, consulted answers, and game files seen, absent included |
| `deterministic`     | `true`, which the sandbox guarantees                                                              |

### Writing one

Implement `msbe_step_guest::Step` and export it:

```rust
use msbe_step_guest::{Choice, Context, Error, Operation, Question, Request, Step};

struct Installer;

impl Step for Installer {
    fn run(context: &Context, request: Request) -> Result<Vec<Operation>, Error> {
        let root = request.roots.first().cloned().ok_or_else(|| Error::extension("no roots"))?;
        let question = Question::choice(
            "edition",
            "Edition",
            vec![Choice::new("lite", "Lite"), Choice::new("full", "Full")],
        )
        .with_default("lite");
        let prefix = format!("{}/", context.ask(&question)?);
        Ok(context
            .entries()?
            .into_iter()
            .filter_map(|entry| {
                let relative = entry.path.strip_prefix(&prefix)?.to_owned();
                Some(Operation::Place { path: format!("{root}/{relative}"), source: entry.path })
            })
            .collect())
    }
}

msbe_step_guest::export_step!(Installer);
```

Off `wasm32`, `Context::native(archive, game, answers)` serves everything from memory, so the logic is
unit tested natively. `extensions/steps/option-installer` is the reference: a FOMOD-style installer
with conditional options, recorded choices and a generated file. Build every example module and
refresh the committed test fixtures with `scripts/development/build-wasm-extensions.sh`; `--check`
fails on a stale fixture instead.

## 18.3 Pack codecs

A WASM codec serves the neutral `PackCodec` contract ([17 §17.4](17-pack-formats-and-native-bundles.md))
from the sandbox. Its exports are `msbe_abi_version`, `msbe_alloc`, `msbe_descriptor`, `msbe_probe`,
`msbe_plan_import`, `msbe_plan_export` and `msbe_layout`. Its only imports are `msbe_input.container`,
`msbe_input.entries`, `msbe_input.read` and `msbe_input.take`, which serve the host-owned
`PackInput`. A codec never sees the network, the store, or a container it did not receive.

Each call runs in a fresh instance on its own thread, while the thread that owns the pack answers
its reads, so a codec reads only the entries it asks for: probing an archive never reads its blobs.

| Limit per call   | Default        |
| ---------------- | -------------- |
| Fuel             | 10,000,000,000 |
| Linear memory    | 512 MiB        |
| Pack bytes read  | 64 MiB         |

A codec's failure keeps its kind. An `unreproducible` or `unsupported_target` error from the sandbox
reaches the user, and sets the exit code, exactly as the same native error would.

A codec is a signed extension envelope that provides exactly `pack-codec-v1`, requests no
capabilities, and supports host API 1. The CLI and daemon load every codec installed in MSBE's data
directory:

```text
<home>/extensions/
  trust.toml
  codecs/pack-list.toml
  codecs/pack-list.wasm
```

`trust.toml` is local policy: the signers allowed to publish extensions, each with its hexadecimal
Ed25519 public key and the providers it may bind codecs to.

```toml
[[signer]]
id        = "example-publisher"
key       = "3b6a27bcceb6a42d62a3a8d02a6f0d73653215771de243a63ac048a18b59da29"
providers = ["modrinth"]
```

An envelope document carries the envelope's fields and names its module, a `.wasm` file beside it,
whose bytes are the signed payload:

```toml
schema         = 1
id             = "pack-list"
version        = "1.0.0"
module         = "pack-list.wasm"
package_digest = "…"
provides       = ["pack-codec-v1"]
host_api       = { minimum = 1, maximum = 1 }
signer         = "example-publisher"
signature      = "…"
```

`msbe extension` writes both documents ([16](16-cli-guide.md)). `keygen` creates a private signing
key and prints the trust entry for its public key. `sign` loads a module in the sandbox, then writes
its envelope document beside it. `verify` checks an envelope against the local trust root exactly as
installing it would, without installing anything. A key file is created readable only by its owner,
is never replaced, and cannot sign while other users can read it.

A codec whose descriptor names no provider needs only a trusted signer. A codec that names a
provider is served under that provider's identity and policy gate, like a native codec, so it also
needs the provider to be registered and its signer to be granted that provider. A codec that fails
any check refuses them all, with an error naming its envelope: nothing runs with trust the user did
not intend. Installed codecs are not native build pins, so installing one never changes which native
bundles a build accepts.

**Shipped codecs.** MSBE ships Modrinth's `.mrpack` codec as a sandboxed codec. Its source is
`extensions/providers/modrinth/codecs/mrpack`. `scripts/development/build-wasm-extensions.sh`
builds `extensions/providers/modrinth/modrinth-mrpack.wasm`, and the provider registry embeds the
module through `wasm_pack_codecs`. A shipped codec is trusted as part of the build rather than through `trust.toml`,
may name only its registration's provider, and is pinned like a native extension, by the SHA-256 of
its module. Each module is compiled once per process.

**Conformance.** Every codec, native or sandboxed, passes `msbe_provider_api::conformance`'s
invariants: a valid descriptor, zero confidence rather than an error for a pack in no format, and
refusing to lay out another codec's export plan. A format also keeps cases and a golden transcript
beside its source, in `conformance/cases.json` and `conformance/transcript.json`, and every
implementation of the format must reproduce the transcript byte for byte. The `.mrpack` transcript was
recorded from the native codec before that codec was removed, so the sandboxed codec is known to
import, export and fail exactly as it did. After a deliberate change, record the new transcript with
`MSBE_BLESS=1 cargo test -p msbe-providers conformance` and review its diff.

Implement `msbe_codec_guest::Codec` and export it with `export_codec!`. `extensions/codecs/pack-list`
is the reference: a ZIP manifest format with pinned downloads and bundled files, imported and
exported entirely through neutral records.

## 18.4 Deliberately absent

- **Network access.** Not a step capability. Data a step needs, such as a load-order masterlist,
  belongs in a pinned data extension ([17 §17.6](17-pack-formats-and-native-bundles.md)), never a
  download during a run.
- **Clock, randomness, processes and the filesystem.** Not linkable. A vendor installer that must run
  natively belongs to a separate, hash-pinned, always-prompting `run-trusted-binary` step, which
  WebAssembly can never reach.
- **Prompts during a run.** Questions are answered from recorded answers, so a deploy never blocks on
  a person and always replays the same way.
- **A wall-clock timeout.** Fuel bounds every run deterministically instead.
