# 14 — Risks & Open Questions

## 14.1 Risks, ordered by how badly they end the project

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| 1 | **Provider relations sour** — Nexus or CurseForge revokes access | medium | severe | The policy layer in [06](06-providers-and-policy.md) is the mitigation. Apply for official keys early, identify honestly, never work around a control, open a channel *before* launch. Degrade gracefully to manual download rather than breaking |
| 2 | **Scope collapse** — game-agnostic quietly becomes "several hardcoded games" | **high** | severe | The compile-test from [00](00-overview.md): `msbe-core` must build and pass tests with zero plans. Any game name in core is a review blocker |
| 3 | **Avalonia + NativeAOT doesn't hold up** at this UI complexity | **low** | moderate | **M0: publish proven**, with 0 trim/AOT warnings and 33.4 MB shippable ([15](15-m0-findings.md)). Remaining: the binary launching, then the 1000-row list. Fallback decided: a self-contained non-AOT publish of the same app. The RPC boundary still means the UI is replaceable without touching core |
| 4 | **Data destruction bug** — one report of deleted saves is unrecoverable reputationally | low | severe | Journal + backups + crash-injection tests + never-elevated + destructive ops always preview. Consider a public bug bounty at v1 |
| 5 | **Plan abstraction is wrong** — a major game won't fit the eight axes | medium | severe | M3 stresses it with Minecraft's legacy jarmod topology and M4 with an unrelated game, both long before Bethesda in M8. **M4 is a measurement, not a feature**: if the NMS plan needs core changes, the axes are wrong and the schedule stops there |
| 6 | **Solo-maintainer burnout** across Rust + C# + WASM + CEF + six-platform CI | **high** | severe | Ruthless milestone scoping, CLI-first (no UI until M6), boring dependencies, and M7 registry specifically to distribute the per-game work to contributors |
| 7 | **CEF weight and CVE cadence** | medium | moderate | Optional download, pinned + verified, system-browser path always works without it. Hosted from Rust via `cef-rs` (Tauri-maintained, tracks current Chromium), so it never touches the AOT-published C# process |
| 8 | **Legal — DMCA/C&D over overlay metadata or plans** | low | moderate | Host no binaries. Evidence-linked overlay entries. Clear takedown process and contact address before launch |
| 9 | **Nobody uses it** — MO2/Vortex/Prism are entrenched and good | medium | moderate | Lead with what they cannot do: true cross-platform, real Proton support, reproducible lockfiles, `bisect`, and a scriptable CLI |
| 10 | **Cross-platform lockfiles turn out not to be reproducible** (case, normalization, path length) | medium | moderate | Validate against the *target* platform's rules at solve time; fail loudly rather than drift ([08 §8.3](08-platforms-and-detection.md)) |
| 11 | **Deployment degrades on real machines**: libraries on several volumes, no reflink on ext4 or NTFS, games rewriting linked files | **high** | moderate | Found in M0 on the first machine checked ([15](15-m0-findings.md)). One store shard per volume; read-only blobs and verification before linking; plans declare mutable paths, which are always copied ([04](04-deployment-engine.md)) |

Risks 2 and 6 are the ones that actually kill projects like this, and both are
discipline problems rather than technical ones.

## 14.2 Open questions

**Needs a decision before M1**

- **Name.** "MSBE" is a working codename. Needs a real name with a free crates.io name,
  an available domain, and no trademark collision in the games space.
- **Licence.** Recommendation: **MPL-2.0** for core (file-level copyleft — keeps
  improvements to the engine open without preventing embedding), **CC0** for registry
  metadata (it should be freely reusable by other tools, including competitors —
  that is how it becomes canonical). Alternative if stronger copyleft is wanted: GPL-3.0
  for the apps, MPL-2.0 for the libraries.
- **Hash function.** BLAKE3 (much faster, tree hashing suits the CAS) versus SHA-256
  (ubiquitous, matches what providers already publish). Leaning: **store both** —
  SHA-256 for provider-supplied verification, BLAKE3 for internal CAS addressing.
- **SQLite vs plain files** for state. SQLite for the index and journal; lockfiles and
  profiles stay as text files so they are diffable and git-friendly.

**Needs a decision before M2**

- **CurseForge at all?** Its API key terms and distribution flag make it the most
  constrained provider. Options: full support with strict flag honouring, metadata-only
  (search and resolve, but downloads go to the website), or omit. Leaning: full support
  with strict honouring, since packs depend on it.
- **Config-merge conflict semantics.** When mod A and mod B set the same key to
  different values and the user has no preference — fail, pick by priority, or prompt?
  Leaning: prompt once and record the decision in the profile.

**Needs a decision before M7**

- **Registry governance.** Who holds the `root` key? What happens to the project if
  that person disappears? Multi-party root signing from day one is cheap insurance.
- **Overlay moderation.** Evidence links are required, but who adjudicates a disputed
  conflict claim?

**Longer-horizon**

- Should MSBE expose a **library API** so launchers (Prism, Heroic, Lutris) can embed
  the engine rather than reimplement it? That would be a much larger win than MSBE's own
  UI ever will be — worth designing the daemon RPC so it stays possible.
- Is there a sustainable funding model that does not compromise the provider-policy
  stance? (Donations and sponsorship are compatible; anything ad- or affiliate-shaped
  is not.)
