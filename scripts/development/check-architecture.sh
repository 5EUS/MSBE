#!/usr/bin/env sh
# Prevent provider, game, storefront, loader, and pack-format behavior from leaking back into the
# generic crates and the desktop client (docs/17 §17.1). Every crate is checked: providers, games
# and formats belong in extensions/ and plans/. Matching is case-insensitive so help text, doc
# comments and UI copy are covered too.
#
# Exempt: tests (`*_tests.rs`, and fixtures that serve a real provider's wire format), and the one
# file that lists what the build ships.
set -eu

root=$(git rev-parse --show-toplevel)
status=0
pattern='loader_dependency|Pack::(?:Modrinth|CurseForge)|export_modrinth|\b(?:minecraft|modrinth|mrpack|curseforge|thunderstore|nexus|nexusmods|nxm|fabric|quilt|neoforge|forge|bepinex|skse|valheim|skyrim|fallout|steam)\b'

if ! command -v rg >/dev/null 2>&1; then
  printf 'error: ripgrep (rg) is required for the architecture guards.\n' >&2
  exit 1
fi

report() {
  printf 'error: provider, game, loader, or format literal leaked into %s; move it to an extension.\n%s\n' \
    "$1" "$2" >&2
  status=1
}

for directory in "$root"/crates/*/; do
  crate=$(basename "$directory")
  case "$crate" in
    msbe-cli) exempt='!**/src/fake_modrinth.rs' ;;
    msbe-providers) exempt='!**/src/builtin.rs' ;;
    *) exempt='!**/.none' ;;
  esac
  if hits=$(rg --ignore-case --line-number --glob '*.rs' --glob '!*_tests.rs' --glob "$exempt" \
    "$pattern" "$directory"); then
    report "$crate" "$hits"
  fi
done

if hits=$(rg --ignore-case --line-number --glob '*.cs' --glob '*.axaml' \
  "$pattern" "$root/dotnet/src"); then
  report "MSBE desktop sources" "$hits"
fi

exit "$status"
