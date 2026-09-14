# 09 — Interfaces: CLI, UI, Headless

Both frontends are clients of the same daemon over the same RPC. The parity rule from
[00](00-overview.md) is enforced by a contract test: every RPC method must be reachable
from the CLI, and the test fails the build if one is not.

## 9.1 CLI

Rust + `clap`. Nouns and verbs match the domain model exactly.

> **Implemented so far (M1):** `instance add|set|list|remove`, `profile new|list|show`, `add`
> (local files, `.zip` archives, `https://` URLs optionally pinned with `#sha256=` or
> `#sha512=`, and `modrinth:<project>[@<version>]` with `--with-deps`),
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
msbe journal   <instance>       # deployments still in effect, newest first
msbe rollback  <instance> [--to <txn>]  # undo the last deployment, or every one after <txn>

msbe order     list | set | sort | pin | unpin
msbe conflicts <instance> [--profile p]  # mods placing different contents at one path

msbe pack      formats | options <codec>
msbe pack      import <instance> <input> [--codec <codec>] [--options <file>] [--dry-run]
msbe pack      update <instance> <input> [--resolve <conflict>=keep|drop]... [--dry-run]
msbe pack      export <instance> <output> --codec <codec> [--preset <preset>] [--options <file>] [--dry-run]
msbe pack      capture <instance> [--path <path>]... [--dry-run]
msbe pack      validate <instance>
msbe snapshot  create <instance> <output> | restore <input> [--dry-run]

msbe plan      list | show | new | validate | test | explain
msbe registry  update | sources | trust

msbe auth      status | acknowledge <provider> | logout <provider>
msbe auth      login <provider> --token-from-stdin | --token-file <file>
msbe download  add | list | pause | resume | cancel | retry | move | confirm | clear
msbe handoff   <uri>            # a provider link from the browser, into the download queue
msbe handler   status [<scheme>] | register <scheme> [--replace] | unregister <scheme>
msbe browser   status | open [<id>] [--auto-advance|--no-auto-advance] | close
msbe tool      list | register <provider> <program> --accept-terms | forget <provider>

msbe doctor                     # environment diagnosis
msbe bisect                     # find the mod that broke it
msbe bundle                     # redacted support bundle
msbe store     gc | verify | path
msbe launch
msbe daemon    start | stop | status
```

Cross-cutting flags, uniform everywhere:

| Flag                       | Behaviour                                                                              |
| -------------------------- | -------------------------------------------------------------------------------------- |
| `--format json`            | machine-readable on stdout; human text always on stderr so piping is clean             |
| `--dry-run`                | resolve and print the `OperationSet`, apply nothing                                    |
| `--non-interactive`        | never prompt; an unanswered `Question` is a non-zero exit with the question serialized |
| `--answers <file>`         | pre-supply wizard answers (FOMOD choices) for CI and scripted installs                 |
| `--instance` / `--profile` | override the active selection                                                          |
| `--yes`                    | accept _safe_ confirmations only; destructive ones still prompt unless `--force`       |

Stable, documented exit codes (`0` ok, `1` generic, `2` usage, `3` unresolvable
dependencies, `4` conflict requiring a decision, `5` unanswered question, `6` provider
policy refusal, `7` integrity/verification failure). Scripts depend on these, so they
are part of the compatibility contract.

## 9.2 `msbe doctor`

Runs the checks that account for most support traffic, and says what to _do_:

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

- **Instances** — detected games, add manually by selecting a daemon-supported game,
  per-instance health from `doctor`. Plan files and identifiers are not user-facing.
- **Games** — the daemon's currently loaded game support and available mod ecosystems.
- **Profiles** — switch, clone, diff two profiles side by side, export/import.
- **Pack editor** — pack-owned configs, codec and preset selection, schema-driven options,
  validation, and an export preview that separates references, embedded content, user actions,
  estimated size, and policy blockers.
- **Mod list** — virtualized (must stay smooth at 1000+ rows), drag-reorder where the
  plan declares ordering, inline enable/disable, filter by provider/tag/state.
- **Browse** — unified search across providers, with provider policy shown honestly
  ("this mod must be downloaded from the site"). A result whose provider needs a key or accepted
  terms, hands files over on its own page, or runs a registered tool is marked "Needs you" before
  it is queued, and the marked list says how many will wait. A provider that cannot be searched
  takes a project reference instead. Both come from `provider.list`, never from provider names.
- **Install preview** — the `OperationSet` rendered before anything happens:
  files to place, conflicts, **excluded/quarantined files with the rule that matched**,
  config merges, components to install. This is the trust-building screen.
- **Conflicts** — a tree of file and semantic conflicts with resolution actions. File conflicts
  (`conflicts.list`) are in the History workspace: each path, the mods that place it, and removing
  one of them.
- **Wizard host** — renders `Question` events (FOMOD etc.) as native dialogs.
- **Download queue** — including the browser-assisted queue with clear progress.

Pack formats and their options are daemon-discovered capabilities. Neither frontend contains a
format-specific command model or ViewModel. The typed preview returned by the daemon is also the
execution plan: the daemon holds it under a plan ID and digest and runs exactly that plan as a job,
so Desktop and CLI show identical inclusion and policy decisions. The pack codec flag is `--codec`,
because `--format` is the global output format. See [17](17-pack-formats-and-native-bundles.md).

- **MSBE browser** — the CEF component, a separate window visually distinct so it is obvious the
  user is on a third-party site. Downloads drives it. Settings → Browser component says whether it
  is installed (`browser.status` `installed`); without it, a waiting row's page opens in the user's
  own browser and the link comes back through a link handler or the paste box.
- **History** — every deployment still in effect, newest first, with rollback to any of them after
  a confirmation that says how many later deployments it undoes (`journal.list`,
  `journal.rollback`); provider updates, previewed and then applied as a job (`update.preview`,
  `update.apply`); and snapshot restore, also a job.
- **Settings** — Accounts from `auth.status`: sign-in state, a link to the program's key page,
  paste → check → keep, sign out, terms, and quota headroom. A pasted key leaves the view as it is
  sent and never comes back. Link handlers, which ask before taking a scheme from another
  application; Browser component; and External tools, which register a program by SHA-256 once
  its provider's terms are accepted.

Accessibility and i18n are in from the start: keyboard navigation everywhere, screen
reader labels on custom controls (an AOT-compatibility test case in its own right),
and no string concatenation for translatable text. Every user-visible string lives in
`Resources/Strings.resx`. `Microsoft.CodeAnalysis.ResxSourceGenerator` emits `Strings.Name`
accessors and `Strings.FormatName(...)` methods for placeholders at compile time, so resources need
no reflection under NativeAOT; views use `{x:Static res:Strings.Name}`. A translation is a
`Strings.<culture>.resx` beside it.

## 9.5 Headless / server mode

`msbe daemon --listen 127.0.0.1:7777 --token-file /etc/msbe/token` for server admins
managing a Minecraft modpack over SSH or from a container. Loopback by default, bearer
token required, TLS required if bound to a non-loopback address, and it refuses to
start as root. Ships with a systemd unit and a container image for the CI use case.
