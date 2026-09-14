# 12 — Testing, Packaging & Release

## 12.1 Test strategy

The resolve/apply split ([02 §2.2](02-plan-system.md)) is what makes this testable: the
overwhelming majority of logic produces an `OperationSet` and never touches a disk.

| Layer                   | Approach                                                                                                                                                                                                                                            |
| ----------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Step / plan logic**   | pure unit tests against in-memory fixture archives; **golden `OperationSet` files** — a plan change that alters behaviour shows up as a readable diff in review                                                                                     |
| **Solver**              | property tests (a solution always satisfies every constraint; solving is deterministic; adding a constraint never _adds_ solutions) + a corpus of real-world hard cases, including known-unsolvable ones where the _error message_ is the assertion |
| **Archive**             | continuous fuzzing plus a curated corpus of every hostile archive in [11 §11.2](11-security.md)                                                                                                                                                     |
| **Applier / journal**   | **crash-injection tests** — kill the process at operation _n_ for every _n_, then assert resume-or-rollback returns a consistent state. This is the highest-value test suite in the project                                                         |
| **Filesystem backends** | run against real ext4, btrfs, XFS, NTFS, APFS, exFAT and a network share; assert the capability probe matches reality                                                                                                                               |
| **Providers**           | recorded HTTP fixtures for CI; a nightly live-API job that alerts on upstream drift rather than breaking PRs                                                                                                                                        |
| **Pack codecs**         | shared conformance suite for bounded detection, hostile archives, option-schema validation, neutral import records, policy enforcement, and deterministic byte-identical exports; golden fixtures stay in the owning extension crate                |
| **RPC contract**        | schema-driven; a test asserts every method is reachable from the CLI (the parity rule)                                                                                                                                                              |
| **UI**                  | Avalonia headless renderer for view-model and smoke tests; **every test also runs against the AOT-published binary**, because AOT breakage is invisible in `dotnet run`                                                                             |
| **End-to-end**          | synthetic "games" in `fixtures/` — a fake game directory plus fake mods — exercising install → conflict → switch → update → rollback → purge, asserting the directory is byte-identical to vanilla at the end                                       |

The synthetic-game fixtures matter: real games cannot be shipped in CI, and a fake one
exercises the same code paths while being fast, legal and deterministic.

## 12.2 CI matrix

NativeAOT cannot cross-compile, so this is six real runners, not one:

| OS      | Arch       | Builds                     |
| ------- | ---------- | -------------------------- |
| Linux   | x64, arm64 | core, cli, daemon, desktop |
| Windows | x64, arm64 | core, cli, daemon, desktop |
| macOS   | x64, arm64 | core, cli, daemon, desktop |

Plus: `cargo clippy -D warnings`, `cargo deny`, `cargo audit`, `cargo fuzz` (nightly,
time-boxed), .NET analyzers with AOT/trim warnings as errors, and the registry's own
`plan validate` + `plan test` on every plan.

## 12.3 Packaging

| Target   | Format                                                                         |
| -------- | ------------------------------------------------------------------------------ |
| Linux    | AppImage (primary), Flatpak, AUR, `.deb`/`.rpm`, plus a standalone CLI tarball |
| Windows  | MSIX and a plain portable zip; signed. **No admin install**                    |
| macOS    | signed + notarized `.app` in a DMG; hardened runtime                           |
| CLI-only | static binaries per platform; Homebrew, Scoop, `cargo install`                 |
| Server   | container image + systemd unit                                                 |

The CLI must be installable **without** the desktop app — that is the server and
scripting audience, and making them install a GUI would lose them.

Flatpak and Snap need explicit attention: sandboxed filesystem access to Steam
libraries and `nxm://` handler registration both require declared permissions, and both
are tested rather than assumed ([08 §8.1](08-platforms-and-detection.md)).

The MSBE browser, `crates/msbe-browser`, is its own artifact and is not a Cargo workspace member:
CEF's build script downloads a Chromium distribution and needs cmake and ninja, and its build
dependencies are outside `deny.toml`'s policy. `scripts/development/check.sh` type-checks and lints it
with `--features dox`, which needs neither. Its artifact must carry CEF's libraries and resources
beside the executable, and it is installed beside the daemon, which starts it from there.

## 12.4 Updates

- Signed, delta where practical; the CEF component updates independently of the app.
- **The daemon never auto-updates under a running operation.** It finishes or refuses.
- Registry updates are separate from app updates — a new plan must not require a new
  app release, which is the entire point of the plan system.
- Schema migrations (SQLite, lockfile, plan schema) are versioned, forward-tested and
  **backward-tested**: a lockfile written by v0.4 must still apply under v0.9 or fail
  with a clear message, never silently produce a different result.

## 12.5 Versioning contracts

Three independently versioned surfaces, each with its own compatibility promise:

1. **`engine`** — core capability version that plans declare compatibility against.
2. **`schema`** — plan manifest schema version.
3. **RPC** — negotiated at connect; the daemon serves N and N−1 so a UI and CLI of
   slightly different vintages still work.

Lockfile format changes are the strictest: a lockfile is the reproducibility contract
and must be readable essentially forever.

Pack codec descriptors, option schemas, native bundle manifests, and normalized export options
are versioned independently. CI imports the oldest retained native fixture with the newest reader,
rejects future schemas without guessing, and verifies that each embedded blob and final deployment
digest still matches. External format fixtures test the owning provider extension; generic crates
contain no game/provider-specific golden data. See [17](17-pack-formats-and-native-bundles.md).
