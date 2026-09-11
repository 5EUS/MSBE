#!/usr/bin/env sh
set -eu
root=$(git rev-parse --show-toplevel)
cd "$root"
cargo fmt --all
if command -v dotnet >/dev/null 2>&1; then
  dotnet format whitespace dotnet/MSBE.slnx --no-restore
else
  printf 'note: dotnet not installed; skipped .NET formatting.\n' >&2
fi
