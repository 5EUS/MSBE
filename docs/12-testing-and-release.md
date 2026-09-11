# 12 — Testing, Packaging & Release

## 12.1 Test strategy

The resolve/apply split ([02 §2.2](02-plan-system.md)) is what makes this testable: the
overwhelming majority of logic produces an `OperationSet` and never touches a disk.

| Layer | Approach |
|---|---|
| **Step / plan logic** | pure unit tests against in-memory fixture archives; **golden `OperationSet` files** — a plan change that alters behaviour shows up as a readable diff in review |
| **Solver** | property tests (a solution always satisfies every constraint; solving is deterministic; adding a constraint never *adds* solutions) + a corpus of real-world hard cases, including known-unsolvable ones where the *error message* is the assertion |
| **Archive** | continuous fuzzing plus a curated corpus of every hostile archive in [11 §11.2](11-security.md) |
| **Applier / journal** | **crash-injection tests** — kill the process at operation *n* for every *n*, then assert resume-or-rollback returns a consistent state. This is the highest-value test suite in the project |
| **Filesystem backends** | run against real ext4, btrfs, XFS, NTFS, APFS, exFAT and a network share; assert the capability probe matches reality |
| **Providers** | recorded HTTP fixtures for CI; a nightly live-API job that alerts on upstream drift rather than breaking PRs |
| **RPC contract** | schema-driven; a test asserts every method is reachable from the CLI (the parity rule) |
| **UI** | Avalonia headless renderer for view-model and smoke tests; **every test also runs against the AOT-published binary**, because AOT breakage is invisible in `dotnet run` |
| **End-to-end** | synthetic "games" in `fixtures/` — a fake game directory plus fake mods — exercising install → conflict → switch → update → rollback → purge, asserting the directory is byte-identical to vanilla at the end |

The synthetic-game fixtures matter: real games cannot be shipped in CI, and a fake one
exercises the same code paths while being fast, legal and deterministic.

## 12.2 CI matrix

NativeAOT cannot cross-compile, so this is six real runners, not one:

| OS | Arch | Builds |
|---|---|---|
| Linux | x64, arm64 | core, cli, daemon, desktop |
| Windows | x64, arm64 | core, cli, daemon, desktop |
| macOS | x64, arm64 | core, cli, daemon, desktop |

Plus: `cargo clippy -D warnings`, `cargo deny`, `cargo audit`, `cargo fuzz` (nightly,
time-boxed), .NET analyzers with AOT/trim warnings as errors, and the registry's own
`plan validate` + `plan test` on every plan.

## 12.3 Packaging

| Target | Format |
|---|---|
| Linux | AppImage (primary), Flatpak, AUR, `.deb`/`.rpm`, plus a standalone CLI tarball |
| Windows | MSIX and a plain portable zip; signed. **No admin install** |
| macOS | signed + notarized `.app` in a DMG; hardened runtime |
| CLI-only | static binaries per platform; Homebrew, Scoop, `cargo install` |
| Server | container image + systemd unit |

The CLI must be installable **without** the desktop app — that is the server and
scripting audience, and making them install a GUI would lose them.

Flatpak and Snap need explicit attention: sandboxed filesystem access to Steam
libraries and `nxm://` handler registration both require declared permissions, and both
are tested rather than assumed ([08 §8.1](08-platforms-and-detection.md)).

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
