#!/usr/bin/env sh
# Builds the sandboxed extensions (pack codecs and plan step extensions) with the pinned toolchain
# and refreshes their committed modules: test fixtures for the examples, and the modules the crates
# that ship them embed. Pass --check to fail instead when a committed module differs from a fresh
# build.
# Needs the wasm32-unknown-unknown target: rustup target add wasm32-unknown-unknown
set -eu
root=$(git rev-parse --show-toplevel)
cd "$root"

check=false
if [ "${1:-}" = "--check" ]; then
  check=true
fi

target_dir="$root/target/wasm-extensions"
status=0

listed=""

# build EXTENSION MODULE: builds the crate at EXTENSION and writes its module to MODULE.
build() {
  listed="$listed $1/"
  package=$(sed -n 's/^name *= *"\(.*\)"$/\1/p' "$1/Cargo.toml" | head -n 1 | tr - _)
  # Remapped paths keep the module byte-identical wherever the repository is checked out.
  RUSTFLAGS="--remap-path-prefix=$root=/msbe --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo" \
    cargo build --quiet --manifest-path "$1/Cargo.toml" --target wasm32-unknown-unknown \
    --release --locked --target-dir "$target_dir"
  artifact="$target_dir/wasm32-unknown-unknown/release/$package.wasm"
  module="$root/$2"
  if $check; then
    if ! cmp -s "$artifact" "$module"; then
      printf 'error: %s is stale; run scripts/development/build-wasm-extensions.sh\n' "$module" >&2
      status=1
    fi
  else
    mkdir -p "$(dirname "$module")"
    cp "$artifact" "$module"
    printf 'built %s (%s bytes)\n' "$module" "$(wc -c < "$module")"
  fi
}

# Examples, used as test fixtures.
build extensions/codecs/pack-list crates/msbe-wasm-codec/tests/fixtures/pack-list.wasm
build extensions/steps/option-installer crates/msbe-plan-host/tests/fixtures/option-installer.wasm
# Shipped with MSBE, embedded by the crate that registers them.
build extensions/codecs/modrinth-mrpack crates/msbe-provider-modrinth/codecs/modrinth-mrpack.wasm

for extension in extensions/*/*/; do
  case "$listed " in
    *" $extension "*) ;;
    *)
      printf 'error: %s is not built by this script; add it above\n' "$extension" >&2
      status=1
      ;;
  esac
done
exit "$status"
