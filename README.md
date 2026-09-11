# MSBE (Modding Should be Easy)

A cross-platform, **game-agnostic** mod manager with a GUI and a first-class CLI.

Most mod managers are built for one game family and encode that game's assumptions
into the tool itself. MSBE inverts that: the tool knows nothing about any game, and
every game's install behaviour is described by a **Plan** — a declarative, signed,
community-contributable document that composes a small, closed vocabulary of steps.

Stack: **Rust** core + daemon + CLI, **C# / Avalonia 11 (NativeAOT)** desktop UI.

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
| [13 — Roadmap](docs/13-roadmap.md) | Milestones M0–M6 |
| [14 — Risks & open questions](docs/14-risks.md) | What could sink this, and what still needs deciding |

## Status

Planning. No code yet. `MSBE` is a working codename.
