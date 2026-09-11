# 09 — Interfaces: CLI, UI, Headless

Both frontends are clients of the same daemon over the same RPC. The parity rule from
[00](00-overview.md) is enforced by a contract test: every RPC method must be reachable
from the CLI, and the test fails the build if one is not.

## 9.1 CLI

Rust + `clap`. Nouns and verbs match the domain model exactly.

> **Implemented so far (M1):** `instance add|set|list`, `profile new|list|show`, `add`
> (local files, `.zip` archives, and `modrinth:<project>[@<version>]` with `--with-deps`),
> `search`, `update [<mod>...] [--dry-run]` (Modrinth mods, staying on each mod's release
> channel), `remove`, `deploy [--dry-run]`, `rollback`, `purge`, `verify` and `status`, with
> `--format json` and `--home`. Exit codes 0, 1, 2, 4 and 7 behave as specified below; the
> rest of this surface is still planned. Deploying a profile is also how profiles switch: it
> applies only the difference from what is deployed, and it repairs drift.

```
msbe game      list | show
msbe instance  detect | add <path> | list | show | remove | use <name>
msbe profile   new | list | switch | copy | diff <a> <b> | export | import | remove

msbe search    <query> [--provider p] [--game g]
msbe add       <mod>[@version] [--optional] [--pin]
msbe remove    <mod>
msbe update    [<mod>] [--latest] [--dry-run]

msbe lock                       # solve selections -> lockfile
msbe sync                       # make disk match lockfile (the workhorse)
msbe deploy | undeploy | purge  # materialization control
msbe verify                     # re-hash deployment, report drift
msbe rollback [<txn>]           # undo the last transaction

msbe order     list | set | sort | pin | unpin
msbe conflicts list | resolve

msbe plan      list | show | new | validate | test | explain
msbe registry  update | sources | trust

msbe auth      login <provider> | logout | status
msbe download  queue | resume | cancel

msbe doctor                     # environment diagnosis
msbe bisect                     # find the mod that broke it
msbe bundle                     # redacted support bundle
msbe store     gc | verify | path
msbe launch
msbe daemon    start | stop | status
```

Cross-cutting flags, uniform everywhere:

| Flag | Behaviour |
|---|---|
| `--format json` | machine-readable on stdout; human text always on stderr so piping is clean |
| `--dry-run` | resolve and print the `OperationSet`, apply nothing |
| `--non-interactive` | never prompt; an unanswered `Question` is a non-zero exit with the question serialized |
| `--answers <file>` | pre-supply wizard answers (FOMOD choices) for CI and scripted installs |
| `--instance` / `--profile` | override the active selection |
| `--yes` | accept *safe* confirmations only; destructive ones still prompt unless `--force` |

Stable, documented exit codes (`0` ok, `1` generic, `2` usage, `3` unresolvable
dependencies, `4` conflict requiring a decision, `5` unanswered question, `6` provider
policy refusal, `7` integrity/verification failure). Scripts depend on these, so they
are part of the compatibility contract.

## 9.2 `msbe doctor`

Runs the checks that account for most support traffic, and says what to *do*:

- store & path detection, and whether the game version matches what the profile solved against;
- materialization results per volume, naming the backend each instance actually got and
  why it fell back. For example: "`/mnt/nvme` is NTFS via ntfs-3g, which has no reflink, so
  deploys hardlink from the shard at `/mnt/nvme/SteamLibrary/.msbe/store`", or "no store
  shard could be created on `/mnt/SSD` because it is not writable, so this instance copies
  every file; grant write access to `/mnt/SSD/SteamLibrary` to reclaim ~40 GB";
- **Steam running while launch options need writing** (see [08 §8.2](08-platforms-and-detection.md));
- Proton version drift since the profile was built;
- loader/component present and at the expected version;
- deployment drift (a game update overwrote modded files);
- orphaned files inside managed directories that MSBE did not place;
- plugin-count / engine limits;
- permissions, path length, case collisions, reserved names;
- provider auth status and rate-limit headroom.

## 9.3 `msbe bisect` — the feature that justifies the architecture

The single most common modding question is "which of my 250 mods broke this?" The
standard answer is hours of manual halving. Because profile switching is a tree diff
([04 §4.5](04-deployment-engine.md)), MSBE can do it in `log₂(n)` launches — about 8
for 250 mods:

```
msbe bisect start --profile hardcore
msbe bisect run            # deploys a subset, launches, asks: did it work? [y/n]
…
msbe bisect result         # -> "BetterCombat 2.1 is the culprit"
                           #    "…and it only fails when ArmorTweaks is also present"
```

Dependency closures are kept intact at every step (never test a subset that cannot
load), the search handles **interaction bugs between pairs** via a follow-up delta
pass, and the whole session is resumable across reboots because it is just profiles
and a journal. Result can be submitted as a registry `conflicts` entry with one
command — turning one user's painful afternoon into knowledge every later user gets
for free.

## 9.4 Desktop UI

Avalonia 12, NativeAOT, MVVM via `CommunityToolkit.Mvvm` (source-generated, no
reflection — see [03 §3.3](03-architecture.md)).

Primary surfaces:

- **Instances** — detected games, add manually, per-instance health from `doctor`.
- **Profiles** — switch, clone, diff two profiles side by side, export/import.
- **Mod list** — virtualized (must stay smooth at 1000+ rows), drag-reorder where the
  plan declares ordering, inline enable/disable, filter by provider/tag/state.
- **Browse** — unified search across providers, with provider policy shown honestly
  ("this mod must be downloaded from the site").
- **Install preview** — the `OperationSet` rendered before anything happens:
  files to place, conflicts, **excluded/quarantined files with the rule that matched**,
  config merges, components to install. This is the trust-building screen.
- **Conflicts** — a tree of file and semantic conflicts with resolution actions.
- **Wizard host** — renders `Question` events (FOMOD etc.) as native dialogs.
- **Download queue** — including the browser-assisted queue with clear progress.
- **Browser tab** — the CEF component, visually distinct so it is obvious the user is
  on a third-party site.
- **Journal / history** — every transaction with a one-click rollback.

Accessibility and i18n are in from the start: keyboard navigation everywhere, screen
reader labels on custom controls (an AOT-compatibility test case in its own right),
and no string concatenation for translatable text.

## 9.5 Headless / server mode

`msbe daemon --listen 127.0.0.1:7777 --token-file /etc/msbe/token` for server admins
managing a Minecraft modpack over SSH or from a container. Loopback by default, bearer
token required, TLS required if bound to a non-loopback address, and it refuses to
start as root. Ships with a systemd unit and a container image for the CI use case.
