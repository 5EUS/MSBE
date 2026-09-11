# Security

## Reporting

Report suspected vulnerabilities privately via GitHub Security Advisories rather than
a public issue. A contact address is published before the first release.

## Scope

The parts of MSBE most worth attacking, and where reports are most valuable:

- **`msbe-archive`** — the hostile-input boundary. Zip-slip, symlink escape,
  decompression bombs, case collisions, reserved names. `#![forbid(unsafe_code)]` and
  continuously fuzzed.
- **`msbe-plan-host`** — the WASM sandbox. Extensions have no filesystem, network,
  process or clock; their only effect is `emit-op`, and the host re-validates every
  emitted operation against declared capabilities. A path out of that is critical.
- **`msbe-fsops`** — the only crate that writes to a game directory. A journal or
  rollback flaw is data loss.
- **`msbe-browser`** — runs untrusted web content. It has **no IPC binding into the
  app**; the capture channel is one-way and typed. A path from page content to the
  filesystem is critical.
- **Secrets** — provider tokens live only in the daemon, are zeroized, and pass through
  a redaction filter in front of the logger. A token appearing in a log or a support
  bundle is a valid report.

## Non-scope

MSBE deliberately never runs elevated. A report that requires the user to run it as
root or Administrator describes a configuration we refuse to support, not a
vulnerability. See `docs/11-security.md`.
