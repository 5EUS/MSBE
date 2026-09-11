# 06 — Providers & Policy

Providers are technically uniform and legally _not_. The policy layer is a
first-class part of the design, not a README disclaimer — getting this wrong is the
most likely way this project dies, and it would die by C&D rather than by bug.

## 6.1 Uniform interface

```rust
trait Provider {
    fn search(&self, q: &Query)                 -> Result<Vec<Mod>>;
    fn versions(&self, id: &ModId)              -> Result<Vec<ModVersion>>;
    fn artifacts(&self, v: &ModVersionId)       -> Result<Vec<Artifact>>;
    fn acquire(&self, a: &ArtifactId)           -> Result<Acquisition>;
    fn policy(&self)                            -> &ProviderPolicy;
}

enum Acquisition {
    DirectUrl { url, headers, expires },
    BrowserAssisted { page_url, capture: CaptureSpec },   // user must act
    Unavailable { reason, user_action },                  // e.g. "download on site"
}
```

`Acquisition` is deliberately a three-way enum. "We cannot fetch this for you" is a
normal, well-typed outcome — not an error and not an invitation to work around it.

## 6.2 Policy matrix

| Provider               | Auth                               | Programmatic download      | Notable constraints                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| ---------------------- | ---------------------------------- | -------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Modrinth**           | none for public; token for private | ✓ open API                 | **Implemented (M1).** Real dependency graph. Requires an identifying User-Agent; 300 requests a minute, surfaced as `RateLimited` with the reset time. Downloads are https-only and verified against the published size and SHA-512 before ingest. Updates are found with the bulk hash endpoints (`POST /version_files` and `/version_files/update`), at most four requests however many mods are installed. A mod stays on its release channel or moves to a more stable one, and never goes to an older version unless the installed one no longer supports the instance. |
| **Thunderstore**       | none                               | ✓                          | Clean SemVer, clean package format.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| **CurseForge**         | API key required                   | ✓ _conditionally_          | **Must honour `allowModDistribution: false`.** When false, third-party download is forbidden — return `Unavailable` and send the user to the mod page. Non-negotiable; this flag is why several managers got access revoked.                                                                                                                                                                                                                                                                                                                                                 |
| **Nexus Mods**         | personal API key / OAuth           | premium: ✓ direct. free: ✗ | Free accounts have no programmatic file download. The _supported_ path is the `nxm://` handler (see below). Rate limits published per-key; honour them and the `X-RL-*` response headers.                                                                                                                                                                                                                                                                                                                                                                                    |
| **GitHub Releases**    | optional token                     | ✓                          | Watch unauthenticated rate limits.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| **CKAN repos**         | none                               | ✓                          | Consume the existing index; do not fork it.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| **Steam Workshop**     | Steam account                      | SteamCMD or import         | **Planned.** MSBE may invoke a user-installed SteamCMD to acquire content the user's account is entitled to receive, or ingest a local file, archive, or directory obtained elsewhere. It will not implement Steam-client, depot, manifest, or authentication protocols itself.                                                                                                                                                                                                                                                                                              |
| **Local / direct URL** | n/a                                | ✓                          | Always available; the manual escape hatch. **Implemented (M1).** URLs must be https, redirects included. A `#sha256=` or `#sha512=` fragment pins a checksum that is verified before ingest, and every download's SHA-512 is recorded as provenance.                                                                                                                                                                                                                                                                                                                         |

TLS for every provider goes through `msbe-http`, which trusts the operating system's
certificate store rather than a bundled list of roots. That respects system and corporate
certificate authorities, and keeps `webpki-roots` (CDLA-Permissive-2.0, which `deny.toml`
does not allow) out of the dependency graph.

Encoded as data in `ProviderPolicy`, not as scattered `if` statements:

```rust
struct ProviderPolicy {
    requires_auth: bool,
    rate_limit: RateLimit,            // token bucket, honours Retry-After / 429
    respects_distribution_flag: bool,
    user_agent: &'static str,         // honest: "MSBE/0.4 (+https://…)"
    tos_url: &'static str,
    ack_required: bool,               // user must acknowledge once, per provider
}
```

## 6.3 Provider manifests

Provider definitions are versioned TOML documents. The M1 runtime loads the built-in direct
URL and Modrinth definitions through the same validated catalog that will consume signed
registry definitions. A manifest declares identity, source recognition, metadata origin,
policy, and an acquisition primitive; it cannot execute code, alter HTTP transport rules, or
weaken policy enforcement.

```toml
schema = 1
id = "modrinth"
name = "Modrinth"

[source]
type = "prefixed"
prefix = "modrinth:"

[metadata]
api_base = "https://api.modrinth.com/v2"

[acquisition]
type = "direct_https"

[policy]
requires_auth = false
respects_distribution_flag = true
tos_url = "https://modrinth.com/legal/terms"
ack_required = false
```

The schema rejects unknown fields, duplicate provider ids, empty or non-ASCII source prefixes,
and metadata endpoints that are not HTTPS. The acquisition vocabulary is closed. In M1 it
contains only `direct_https`: a reviewed adapter must still supply an HTTPS artifact URL and
the applicable hash/size validation. Future primitives such as `browser_assisted`,
`local_import`, and `steamcmd` require a runtime implementation and policy review before a
manifest can select them.

Manifest data routes sources and configures reviewed adapters. It does not replace provider
semantics that cannot be described safely as mappings: Modrinth's dependency walk, release
channel policy, and bulk update protocol remain a built-in adapter behind its manifest.

The M1 runtime resolves every recognized source through a fail-closed reviewed-adapter registry.
Only the built-in `modrinth` and `url` adapters are available; a registry manifest without a
compiled adapter is rejected rather than interpreted generically. The registry checks manifest
policy before an adapter can make a request. M1 has no credential or persisted-acknowledgement
workflow, so providers declaring `requires_auth = true` or `ack_required = true` are refused
with an explicit unsupported-workflow error. This permits future adapters to declare stronger
requirements without accidentally weakening their policy on older clients.

### Adapter implementation contract

A new provider is a reviewed Rust adapter, not a manifest template. Its implementation must
obey these boundaries:

1. **Registry-only entry.** The adapter is added to `ProviderRegistry` by stable manifest ID.
   It may only use manifest-validated metadata origins and only after the registry policy gate
   has accepted the provider. A source prefix alone never enables code or network access.
2. **One compatibility input.** Selection and search accept the shared
   `msbe_providers::Target` `{ game_version, loader, provides, loader_version, side }`. Native
   provider metadata is translated into it before candidates leave the adapter. Missing side
   metadata is incompatible; loader capabilities are explicit rather than inferred.
3. **Bounded metadata.** API calls use `JsonEndpoint` with endpoint-relative paths and a fixed
   response limit. Adapters do not construct URLs from untrusted identifiers or issue ad-hoc
   HTTP requests.
4. **Verified acquisition.** Direct artifacts use the shared acquisition service, which owns
   create-new streaming transfer, HTTPS-only URLs, safe output names, published size checks, and
   SHA-256/SHA-512 verification. The adapter supplies only an inert descriptor and must provide
   every integrity value it has.
5. **Narrow extensions.** Dependency resolution, release-channel policy, and bulk update lookup
   are provider-specific extensions. They must be named methods with tests, not hidden behavior
   in the common search or acquisition path. An adapter without a sanctioned capability returns
   an explicit unavailable outcome.
6. **Normalized discovery.** Registry-dispatched search returns the stable `SearchResult`
   `{ provider, project, reference, title, description, downloads }`, not an adapter result.
   Adapter JSON request/response records and endpoint details stay private as each provider is
   migrated to the normalized metadata model.

Modrinth remains the reference adapter for the optional dependency-walk and bulk-update
extensions. It uses the shared target model, endpoint, acquisition service, and registry policy;
new providers begin with search/select/acquire and add extensions only when their public API and
tests justify them. The remaining Modrinth project, release, and dependency records are the
next solver-aligned migration; they are intentionally not forced into a premature common trait.

## 6.4 What we will not do

Stated here so it is never re-litigated in a PR:

- No captcha solving, no wait-timer skipping, no ad-gate bypass.
- No spoofing premium status, session tokens, or another client's User-Agent.
- No scraping around a published rate limit, and no distributing shared API keys.
- No mirroring or re-hosting mod binaries.
- No ignoring `allowModDistribution: false` or any equivalent opt-out.
- No Steam credential handling, client-protocol emulation, depot or manifest access, or
  integration that bypasses Steam's normal entitlement and delivery controls.

MSBE identifies itself honestly in every request. If a provider asks us to change
something, we change it. The alternative is losing access for all users — which is
what has happened to every tool that treated these as obstacles.

## 6.5 Steam Workshop: SteamCMD or user-supplied content

Steam Workshop support is planned as an opt-in integration with the user's installed SteamCMD,
plus a local import boundary. Workshop delivery has game-specific layouts, subscription
semantics, and platform rules; MSBE will not infer permission to automate around any of them.
The supported workflows are deliberately narrow:

1. the user explicitly enables the SteamCMD adapter and points MSBE to their installed binary;
2. SteamCMD performs any authentication and entitlement checks through its own supported flow;
3. MSBE requests only the selected content, imports the resulting local artifact, and validates,
   hashes, stores, and deploys it through the normal plan;
4. alternatively, the user selects an exported file, archive, or directory obtained through the
   Steam client or another tool they choose;
5. MSBE records the source, content digest, and optional Workshop item URL or ID for display
   and update reminders.

The planned adapter must use a SteamCMD binary supplied by the user; it must not bundle or
modify SteamCMD, retain Steam credentials, emulate Steam protocols, inspect depot or manifest
data, or bypass entitlement, subscription, rate-limit, or content-owner controls. A game plan
may describe how an acquired artifact installs, but it does not grant MSBE permission to acquire
it. If SteamCMD cannot obtain an item through its supported flow, MSBE returns `Unavailable` and
offers the local-import path without suggesting a workaround.

Possible future conveniences are limited to local, user-initiated operations: importing a path,
checking whether its recorded content digest changed, and opening the item's public page in the
user's browser. Background downloads, subscription synchronization, and update polling remain
out of scope unless Valve publishes and permits a suitable integration path.

## 6.6 The `nxm://` path (why free Nexus users are fine)

Nexus provides "Mod Manager Download" buttons that emit `nxm://` links **for free
accounts too**. That is the sanctioned mechanism, and it is what MO2 and Vortex use.

```mermaid
sequenceDiagram
  actor User
  participant Br as Browser<br/>(integrated or system)
  participant Nx as Nexus Mods
  participant M as MSBE daemon
  participant S as CAS

  User->>Br: click "Mod Manager Download"
  Note over Br,Nx: any wait timer runs normally, untouched
  Br->>Nx: request
  Nx-->>Br: nxm://game/mods/1234/files/5678?key=…&expires=…
  Br->>M: OS protocol-handler dispatch
  M->>Nx: redeem one-time key via API
  Nx-->>M: time-limited direct URL
  M->>S: download → verify hash → blob
  M-->>User: queue advances (N of 200)
```

MSBE registers the protocol handler on all three platforms and catches links from
**the integrated browser or the user's system browser alike** — a user who prefers
Firefox loses nothing.

## 6.7 Assisted download queue (the large-modpack case)

The honest version of "browser automation." For a 200-mod Nexus list on a free
account, MSBE:

1. resolves the list to concrete mod/file pages;
2. shows a queue with progress — _N of 200_;
3. navigates the integrated browser to the next page in the queue;
4. **waits for the user to click the download button**, and any wait timer runs
   normally, un-touched;
5. captures the resulting `nxm://` link, ingests the file, advances the queue.

The automation is _navigation and capture_, not _clicking through gates_. It removes
tab management and copy-pasting, which is the actual tedium, and it leaves every
access control exactly where the site put it. Rate-limited, resumable, and cancellable.

Optionally an "auto-advance" toggle moves to the next page after a successful capture.
There is no toggle that clicks the download button.

## 6.8 Operational asks

Tasks, not afterthoughts — start them early because approval takes weeks:

- Apply for an official **CurseForge API key** for a named application.
- Apply for a **Nexus Mods** application/API registration and open a channel with
  their team before launch, not after.
- Publish the User-Agent, contact address and a clear statement of what MSBE does and
  does not do, so providers can evaluate us without reverse-engineering traffic.
