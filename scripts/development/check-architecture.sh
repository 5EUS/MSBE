#!/usr/bin/env sh
# Prevent provider, game, loader, and pack-format behavior from leaking back into generic runtime
# crates and the desktop client (docs/17 §17.1). Matching is case-insensitive so help text and UI
# copy are covered too.
set -eu

root=$(git rev-parse --show-toplevel)
status=0
pattern='loader_dependency|Pack::(?:Modrinth|CurseForge)|export_modrinth|\b(?:minecraft|modrinth|mrpack|curseforge|fabric|quilt|neoforge)\b'

if ! command -v rg >/dev/null 2>&1; then
  printf 'error: ripgrep (rg) is required for the architecture guards.\n' >&2
  exit 1
fi

report() {
  printf 'error: provider, game, loader, or format literal leaked into %s; move it to an extension.\n%s\n' \
    "$1" "$2" >&2
  status=1
}

for crate in msbe-archive msbe-browser msbe-cli msbe-core msbe-daemon msbe-fsops msbe-pack msbe-plan-host msbe-rpc-schema msbe-wasm-codec; do
  directory="$root/crates/$crate/src"
  [ -d "$directory" ] || continue
  if hits=$(rg --ignore-case --line-number --glob '*.rs' --glob '!*_tests.rs' --glob '!fake_modrinth.rs' \
    "$pattern" "$directory"); then
    report "$crate" "$hits"
  fi
done

if hits=$(rg --ignore-case --line-number --glob '*.cs' --glob '*.axaml' \
  "$pattern" "$root/dotnet/src"); then
  report "MSBE desktop sources" "$hits"
fi

exit "$status"
