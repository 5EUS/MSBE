# 07 — Integrated Browser & Secrets

## 7.1 Why a bundled Chromium rather than the OS webview

`msbe-browser` is a **separate Rust process embedding CEF through
[`cef-rs`](https://github.com/tauri-apps/cef-rs)**, shipped as an *optional downloadable
component* (~100–200 MB) rather than bundled into the base install.

Rationale:

- **One automation story.** WebKitGTK, WebView2 and WKWebView have three different
  and unequal scripting/interception APIs. The assisted download queue needs
  navigation control, download interception and DOM readiness signals on all three
  platforms; CEF's DevTools Protocol gives one implementation instead of three.
- **Consistent behaviour.** A login flow that works on Windows works on Linux.
- **Sandboxing we don't have to write.** Chromium's own multi-process sandbox.
- **Avalonia's WebView story is the weak link** in this stack; this routes around it.
- **Chromium stays out of the C# process.** Hosting CEF from Rust keeps it entirely out of
  the NativeAOT-published desktop app, so there is no CefGlue or AOT-compatibility question
  to answer. `cef-rs` is maintained by the Tauri team, tracks current Chromium, and covers
  Linux, macOS and Windows on x86-64 and ARM64 ([15](15-m0-findings.md)).

Cost — stated plainly: it is large, it is a second update stream with its own CVE
cadence, and it must be kept current. Mitigations: it is optional (everything works
via the system browser + `nxm://` handler without it), it is downloaded on demand,
and its version is pinned and updated as a tracked dependency.

## 7.2 Security boundary — the rule that must never be broken

**The browser process has no IPC binding into MSBE.** Not a narrowed one. None.

A remote page — Nexus, or anything it embeds, or anything an XSS on it reaches — must
never be one `window.msbe.*` call away from the filesystem. The browser talks to the
daemon over a **narrow, one-way, typed capture channel** carrying exactly:

```
CapturedProtocolUrl { scheme: "nxm", url }
CapturedDownload    { suggested_name, bytes (to a quarantine dir), origin_url }
NavigationState     { url, title, queue_position }
```

The daemon validates every field, never trusts `suggested_name` as a path, and writes
captured bytes into a quarantine directory that the normal artifact pipeline (hash →
verify → CAS) then consumes. Everything else is refused.

Additional hardening:
- dedicated cache/profile directory per provider, never the user's real browser profile;
- `file://` access disabled; no plugins; no arbitrary JS injection except registry-signed
  automation scripts scoped to a declared origin;
- downloads only ever land in quarantine, never at a user-specified path;
- the process runs at the same privilege as the user and never elevated.

## 7.3 Automation scope

Automation drives **navigation and capture**. It does not click through access
controls. Concretely, an automation script may: navigate to a URL, wait for a
selector, read the page's mod/file identifiers, detect that a download has started,
and report queue state. It may not: click a download button, submit a captcha, skip a
timer, or synthesise a session. See [06 §6.5](06-providers-and-policy.md).

Automation scripts live in the registry, are signed, are scoped to a single origin,
and are reviewable as plain text. A provider who dislikes one can point at it.

## 7.4 Protocol handler registration

`nxm://` (and future equivalents) registered per platform, and **always
unregisterable** — a mod manager that fights other mod managers over a protocol
handler is a bad citizen. On install MSBE detects an existing handler and asks rather
than silently taking over.

| Platform | Mechanism |
|---|---|
| Linux | `.desktop` with `MimeType=x-scheme-handler/nxm`, `xdg-mime default` |
| Windows | `HKCU\Software\Classes\nxm` (per-user; never `HKLM`, never requires admin) |
| macOS | `CFBundleURLTypes` in `Info.plist` + Launch Services registration |

Flatpak and Snap builds need explicit portal/interface declarations for this to work;
treated as a packaging test case, not an afterthought.

## 7.5 Secrets & keychain

Stored: provider API keys and OAuth tokens, the optional remote-daemon bearer token,
and nothing else. Never: mod content, never game paths, never anything large.

| Platform | Backend |
|---|---|
| Linux | Secret Service (`libsecret`) → GNOME Keyring / KWallet |
| Windows | DPAPI via Credential Manager |
| macOS | Keychain Services |
| Headless / CI / container | encrypted file fallback, below |

Via the Rust `keyring` crate in `msbe-daemon` — **the UI never holds a secret**. The
C# client asks the daemon "am I authenticated with Nexus?" and gets a boolean and a
username, never a token. This keeps the entire secret surface inside one Rust process
and out of a managed heap that is hard to zeroize.

**Headless fallback** is required, not optional — a Minecraft server admin has no
D-Bus session and no keyring daemon. In order of preference:

1. `MSBE_<PROVIDER>_TOKEN` environment variable (the CI path);
2. `--token-from-stdin` / `--token-file` for one-shot commands;
3. an age-encrypted secrets file, key derived with Argon2id from a passphrase,
   unlocked once per daemon lifetime.

Discipline around them:
- tokens are `Zeroizing<String>` and never `Debug`-printed;
- a **redaction filter sits in front of the logger**, not at each call site, and is
  property-tested — "we remembered to redact everywhere" is not a security control;
- `msbe bundle` (support bundle) runs the same redactor and prints a summary of what
  it removed so the user can see it worked before mailing the file to a stranger;
- token scopes are minimal, expiry is honoured, and `msbe auth status` shows exactly
  which credentials exist and when they were last used.
