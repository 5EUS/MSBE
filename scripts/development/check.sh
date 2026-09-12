#!/usr/bin/env sh
# Everything CI checks, locally. Run before opening a PR.
set -eu
root=$(git rev-parse --show-toplevel)
cd "$root"

printf '== rustfmt ==\n';  cargo fmt --all --check
printf '== clippy ==\n';   cargo clippy --workspace --all-targets --all-features -- -D warnings
printf '== tests ==\n';    cargo test --workspace --all-features
printf '== architecture guards ==\n'; sh scripts/development/check-architecture.sh
printf '== xaml guards ==\n'; sh scripts/development/check-xaml.sh

if command -v cargo-deny >/dev/null 2>&1; then
  printf '== cargo-deny ==\n'; cargo deny check
else
  printf 'note: cargo-deny not installed (cargo install cargo-deny); skipped.\n' >&2
fi

if command -v dotnet >/dev/null 2>&1; then
  printf '== dotnet ==\n'
  dotnet restore dotnet/MSBE.slnx --locked-mode
  dotnet format whitespace dotnet/MSBE.slnx --no-restore --verify-no-changes
  dotnet build dotnet/MSBE.slnx -c Release --no-restore
else
  printf 'note: dotnet not installed; skipped the .NET half.\n' >&2
fi
