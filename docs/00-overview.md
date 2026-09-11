# 00 — Overview & Non-Goals

## The thesis

Every existing mod manager is a *game-family* tool wearing a general-purpose coat:

- **Mod Organizer 2** — brilliant VFS, Bethesda-shaped, Windows-only (USVFS).
- **Vortex** — multi-game, but Nexus-coupled and its "game extensions" are arbitrary JS.
- **CKAN** — the best dependency model in modding, curated manifests, **one game**.
- **r2modman / Thunderstore** — excellent for BepInEx games, one ecosystem.
- **Prism / MultiMC / packwiz** — excellent for Minecraft, one game.

The repeated pattern: a good install model gets built, then welded to one game.

MSBE's claim is that **the install model is the product** and the game is data. The
core knows how to fetch, verify, unpack, resolve a file layout, materialize it into a
directory transactionally, and undo that. Everything game-specific lives in a Plan.

## What "game-agnostic" has to mean concretely

Not "we have a switch statement with many cases." It means the core never names a
game, a store, a loader, or a file format. If a game concept leaks into
`msbe-core`, that's a design bug, and the test for it is: *could this crate compile and
pass its tests with zero plans installed?* (Answer must stay yes.)

## Design principles

1. **Resolve, then apply.** Every operation is computed as an inert `OperationSet`
   before a single byte is written. Dry-run, `diff`, preview and rollback then come for
   free, and they are not a separate code path that can drift.
2. **The game directory is not state.** It is a *render target*. Real state lives in
   the content-addressed store plus a lockfile. Anything in the game dir can be
   reconstructed, and anything MSBE put there can be removed exactly.
3. **Nothing is trusted.** Not mod archives, not plans, not provider responses, not
   the registry. Each gets an explicit trust boundary (see [11 — Security](11-security.md)).
4. **The CLI is not a port of the GUI.** Both are clients of the same daemon. If a
   thing can be done in the UI and not the CLI, that is a bug.
5. **Reproducibility is a feature, not a side effect.** A profile exports to a
   lockfile; applying that lockfile on another machine, another OS, or in CI must
   produce a byte-identical deployment or fail loudly saying why it cannot.
6. **Never run elevated.** MSBE writes into game directories as the user. If a path
   needs admin, that is a diagnosis to report, not a privilege to acquire.

## Non-goals

- **We do not host or mirror mod binaries.** Ever. Bandwidth, legality and
  moderation are all somebody else's core competency.
- **We do not circumvent access controls.** No paywall bypass, no captcha solving,
  no wait-timer skipping, no premium spoofing. See [06](06-providers-and-policy.md).
- **We do not ship a mod editor**, asset tooling, or a launcher replacement.
- **We are not a general package manager.** Refusing to grow into one is an ongoing
  discipline, not a one-time decision.
- **No anticheat-adjacent work.** Games with kernel anticheat get a loud warning and
  no special support. Getting users banned is an unrecoverable reputational event.
- **v1 is not a multiplayer sync service.** Profile export is a file, not a cloud.

## What success looks like at v1

A user on Arch Linux running a 250-mod Minecraft modpack, No Man's Sky through Proton,
and a modded Skyrim can, from either the GUI or a shell script:

- add a mod by name, get its dependencies resolved and its incompatibilities flagged;
- switch between two profiles in under a second without re-downloading anything;
- hand a single lockfile to a friend on Windows and have them end up with the same build;
- break the game, run `msbe bisect`, and identify the culprit mod in ~8 launches;
- run `msbe purge` and have the game directory be bit-identical to vanilla.
