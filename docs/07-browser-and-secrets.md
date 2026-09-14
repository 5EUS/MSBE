# 07 — Integrated Browser & Secrets

## 7.1 Why a bundled Chromium rather than the OS webview

`msbe-browser` is a **separate Rust process embedding CEF through
[`cef-rs`](https://github.com/tauri-apps/cef-rs)**, shipped as an *optional downloadable
component* (~100–200 MB) rather than bundled into the base install.

Rationale:

- **One automation story.** WebKitGTK, WebView2 and WKWebView have three different
  and unequal scripting/interception APIs. The assisted download queue needs
  navigation control, download interception and DOM readiness signals on all three
  platforms; CEF's client handlers give one implementation instead of three:
  `OnProtocolExecution` captures a link scheme, `OnBeforeDownload` sends every download to
  quarantine, and the display handler reports the page.
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
browser → daemon   CapturedProtocolUrl { url }
                   CapturedDownload    { suggested_name, quarantine_file, origin_url, size }
                   NavigationState     { url, title }
daemon → browser   Navigate { url }    # HTTPS on an origin the browser was started for
                   Close
```

The daemon validates every field, never trusts `suggested_name` as a path, and writes
captured bytes into a quarantine directory that the normal artifact pipeline (hash →
verify → CAS) then consumes. Everything else is refused.

Additional hardening:
- dedicated cache/profile directory per provider, never the user's real browser profile;
- `file://` access disabled; no plugins; no arbitrary JS injection except registry-signed
  automation scripts scoped to a declared origin;
- downloads only ever land in quarantine, never at a user-specified path;
- only HTTPS pages load, popups open in the one window, and the operating system never opens a link
  from the browser;
- the process runs at the same privilege as the user and never elevated.

**Status: implemented, but the CEF host has not yet been built or run.** `msbe-browser-channel`
defines the channel. The daemon starts `msbe-browser` from beside itself, with the channel on its
standard input and output: each frame is a four-byte length and at most 64 KiB of JSON, and the host
moves both pipes off the standard descriptors before Chromium starts a subprocess. The browser is
started for one provider, as `--msbe-*` arguments: its profile and quarantine directories under
`<home>/browser/<provider>/`, the origins of that provider's waiting pages, and its link scheme. No
`MSBE_<PROVIDER>_TOKEN` variable reaches it. The daemon ends the session on a frame the channel does
not carry, a download named outside the quarantine or unlike its file, or a link in a scheme the
browser was not started for. A captured download fills the waiting file whose page the browser was
sent to: it is named by the rule provider downloads follow, and its SHA-256 and SHA-512 are recorded.

`crates/msbe-browser` is built on its own, outside the Cargo workspace, because CEF's build downloads
Chromium and needs cmake and ninja. `cargo check --features dox` type-checks it against CEF's
bindings without either.

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

**Status: implemented on Linux and Windows.** `msbe handler status|register|unregister <scheme>`
and the daemon's `handler.*` methods take the scheme from an enabled provider program
([06 §6.4](06-providers-and-policy.md)), so none is built in. Registration never takes a scheme over
silently: while another application opens its links, `register` refuses and names it, and
`--replace` (`replace` over RPC) is the user's answer. What MSBE replaces is recorded inside its own
registration, so `unregister` gives the scheme back from any data directory.

- **Linux.** `msbe-handler.desktop` in `$XDG_DATA_HOME/applications` runs `msbe handoff %u`. The
  default is set in the user's `mimeapps.list`, or in the desktop-specific list that already sets
  one, and ownership follows the MIME Applications Associations lookup: defaults, added
  associations, then entries declaring the type. MSBE edits these files itself instead of running
  `xdg-mime`, and writes back every line it does not change.
- **Windows.** `HKCU\Software\Classes\<scheme>` runs `msbe.exe handoff "%1"`. A per-user key MSBE
  replaces is kept beneath `MSBE\Previous` and put back on unregister; a machine-wide handler is
  only reported. The handler reaches the daemon once the named-pipe transport lands.
- **macOS.** The application bundle declares `CFBundleURLTypes`, and Desktop hands the URLs macOS
  delivers as activations to `handoff.submit`. `msbe handler` explains that nothing is registered
  at run time.

Without a handler, a link can be pasted into Desktop's Downloads page.

Flatpak and Snap builds need explicit portal/interface declarations for this to work;
treated as a packaging test case, not an afterthought.

## 7.5 Secrets & keychain

Stored: provider API keys and OAuth tokens, the optional remote-daemon bearer token,
and nothing else. Never: mod content, never game paths, never anything large.

| Platform | Backend |
|---|---|
| Linux | Secret Service over D-Bus (pure-Rust `zbus`, no `libsecret`) → GNOME Keyring / KWallet |
| Windows | DPAPI via Credential Manager |
| macOS | Keychain Services |
| Headless / CI / container | encrypted file fallback, below |

The `msbe-secrets` crate, running in the daemon — **the UI never stores or reads back a
secret**. A key the user pastes passes through the client once, on its way to the
daemon. After that the C# client asks "am I signed in to this provider?" and gets a
boolean and an account name, never a token. This keeps the entire secret surface inside
one Rust process and out of a managed heap that is hard to zeroize.

A provider's credential is looked up in this order:

1. `MSBE_<PROVIDER>_TOKEN`, the provider id uppercased with each `-` as `_` (the CI and
   container path), which overrides a stored credential;
2. the platform keyring, under the service `msbe` with the provider id as the user;
3. the encrypted file, `<home>/auth/secrets.toml`, once it is unlocked.

A new credential goes to the keyring when one can be reached, and otherwise to the
unlocked encrypted file.

**Headless fallback** is required, not optional — a Minecraft server admin has no
D-Bus session and no keyring daemon. The encrypted file is XChaCha20-Poly1305 under a
key derived with Argon2id (64 MiB and three passes by default) from a passphrase, and
is unlocked once per daemon lifetime. Its Argon2id costs, salt and nonce are
authenticated with the ciphertext, so an altered header fails exactly like a wrong
passphrase, and a file asking for more than 4 GiB of memory is refused before any key
is derived. `msbe auth login <provider>` reads a key only with `--token-from-stdin` or
`--token-file`, never from an argument, and sends it to the daemon alone. The daemon refuses it
until the provider's terms are acknowledged (`msbe auth acknowledge`), checks it against the
program's `[auth] validate` route, and keeps it with the account name the provider reported.
`msbe auth logout` forgets it; a token in the environment is unaffected.

`<home>/auth/` is readable only by its owner. Beside the encrypted file it holds
`credentials.toml`, which records each provider's store, account name, and when its
credential was stored and last used, and `acknowledgements.toml`, the terms
acknowledged for each provider. Neither holds a secret. The provider policy gate reads
only these two files and the environment, never the keyring: a provider declaring
`ack_required` is refused until its current terms URL, under its current program
digest, is acknowledged, and one declaring `requires_auth` is refused until it has a
credential.

Discipline around them:
- a secret is a zeroizing value that `Debug` never shows and that has no `Display`,
  `Serialize` or `Clone`. A value shorter than 8 characters, with surrounding
  whitespace, or with a control character is refused, because it could not be redacted
  or sent in a header safely;
- **redaction works on values, not call sites**: every secret the process holds is
  registered, every daemon response passes through the filter before it is written, and
  so does CLI output on platforms that run without the daemon. It is property-tested —
  "we remembered to redact everywhere" is not a security control. There is no logger
  yet; when one lands, it goes behind the same filter;
- HTTP errors never repeat a URL's query or fragment, where signed download links carry
  their credentials;
- the provider registry, not the adapter, attaches a credential. An adapter names the
  header its credential goes in; the registry adds the value only to requests for the
  origin (scheme, host and port) of the provider's `api_base`, never to an artifact
  download, and refuses header names HTTP already gives a meaning to, such as
  `authorization`, `cookie`, `content-*` and `proxy-*`. ureq forwards every header except
  `Authorization` and `Cookie` across redirects, so `msbe-http` follows the redirects of a
  request carrying a credential itself and fails one that leaves its origin;
- `msbe bundle` (support bundle, planned) runs the same redactor and prints a summary of
  what it removed so the user can see it worked before mailing the file to a stranger;
- token scopes are minimal, expiry is honoured, and `msbe auth status` shows exactly
  which providers have a credential, where it is kept, the account it belongs to, when it
  was last used (in JSON), whether the terms are accepted, and the quota last reported.
