#!/usr/bin/env sh
# Builds the example sandboxed codecs with the pinned toolchain and refreshes their committed test
# fixtures. Pass --check to fail instead when a committed fixture differs from a fresh build.
# Needs the wasm32-unknown-unknown target: rustup target add wasm32-unknown-unknown
set -eu
root=$(git rev-parse --show-toplevel)
cd "$root"

check=false
if [ "${1:-}" = "--check" ]; then
  check=true
fi

target_dir="$root/target/wasm-codecs"
status=0
for codec in extensions/codecs/*/; do
  name=$(basename "$codec")
  package=$(printf 'msbe_codec_%s' "$name" | tr - _)
  # Remapped paths keep the module byte-identical wherever the repository is checked out.
  RUSTFLAGS="--remap-path-prefix=$root=/msbe --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo" \
    cargo build --quiet --manifest-path "$codec/Cargo.toml" --target wasm32-unknown-unknown \
    --release --locked --target-dir "$target_dir"
  artifact="$target_dir/wasm32-unknown-unknown/release/$package.wasm"
  fixture="$root/crates/msbe-wasm-codec/tests/fixtures/$name.wasm"
  if $check; then
    if ! cmp -s "$artifact" "$fixture"; then
      printf 'error: %s is stale; run scripts/development/build-wasm-codecs.sh\n' "$fixture" >&2
      status=1
    fi
  else
    mkdir -p "$(dirname "$fixture")"
    cp "$artifact" "$fixture"
    printf 'built %s (%s bytes)\n' "$fixture" "$(wc -c < "$fixture")"
  fi
done
exit "$status"
