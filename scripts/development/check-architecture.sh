#!/usr/bin/env sh
# Prevent provider, game, and pack-format behavior from leaking back into generic runtime crates.
set -eu

root=$(git rev-parse --show-toplevel)
status=0

for crate in msbe-cli msbe-core msbe-daemon msbe-pack msbe-plan-host; do
  directory="$root/crates/$crate/src"
  if hits=$(rg --line-number --glob '*.rs' --glob '!end_to_end_tests.rs' \
    'loader_dependency|Pack::(?:Modrinth|CurseForge)|export_modrinth|\b(?:minecraft|mrpack|curseforge)\b' \
    "$directory" 2>/dev/null); then
    printf 'error: provider, game, or format literal leaked into %s; move it to an extension crate.\n%s\n' \
      "$crate" "$hits" >&2
    status=1
  fi
done

exit "$status"