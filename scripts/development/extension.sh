#!/usr/bin/env sh
# Builds sandboxed extensions to WebAssembly modules, and installs pack codecs and provider bundles.
#
#   sh extension.sh build   DIR               build the crate at DIR (e.g. extensions/codecs/pack-list) into DIR/dist/
#   sh extension.sh install DIR --key signer.pem --version 1.0.0 [--id ID]
#       DIR is a codec crate: build it, sign it, and install its module and envelope into the codecs folder.
#       DIR is a provider bundle (has program.toml, e.g. extensions/providers/modrinth): sign program.toml and
#       install its envelope into the providers folder, then build, sign and install every codec under codecs/*.
#       --id names a single codec and is refused for a bundle.
#
# Provider overlays are not installed: MSBE compiles them in (crates/msbe-providers/src/builtin.rs) and reads none from
# the data directory yet, so the script reports them and copies nothing. Plan step extensions are never installed; a
# plan declares them (docs/18).
# Needs the wasm32-unknown-unknown target (`rustup target add wasm32-unknown-unknown`). The `msbe` tool is the MSBE
# environment variable if set, else `msbe` on the PATH, else built from this repository with cargo, so nothing needs
# installing. The data directory is $MSBE_HOME, else $XDG_DATA_HOME/msbe, else
# ~/.local/share/msbe. CODECS_DIR and PROVIDERS_DIR override the two folders. Extra cargo flags go in CARGO_FLAGS.
set -eu

usage() {
  sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

[ $# -ge 2 ] || usage
command=$1
dir=$2
shift 2
case "$command" in build | install) ;; *) usage ;; esac

key=""
version=""
id=""
while [ $# -gt 0 ]; do
  case "$1" in
    --key) key=${2:?--key needs a file}; shift 2 ;;
    --version) version=${2:?--version needs a value}; shift 2 ;;
    --id) id=${2:?--id needs a value}; shift 2 ;;
    *) usage ;;
  esac
done
if [ "$command" = install ] && { [ -z "$key" ] || [ -z "$version" ]; }; then
  echo "install needs --key and --version" >&2
  usage
fi

dir=$(cd "$dir" && pwd)
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
# msbe ARGS...: the MSBE environment variable if set, else `msbe` on the PATH, else built from this repository.
msbe() {
  if [ -n "${MSBE:-}" ]; then
    "$MSBE" "$@"
  elif command -v msbe > /dev/null 2>&1; then
    command msbe "$@"
  else
    cargo run --quiet --manifest-path "$root/Cargo.toml" -p msbe-cli --bin msbe -- "$@"
  fi
}
data=${MSBE_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/msbe}
codecs=${CODECS_DIR:-$data/extensions/codecs}
providers=${PROVIDERS_DIR:-$data/extensions/providers}

# build_module CRATE_DIR: builds the crate and prints the path of the module copied to CRATE_DIR/dist.
build_module() {
  crate=$(sed -n 's/^name *= *"\(.*\)"$/\1/p' "$1/Cargo.toml" | head -n 1)
  if [ -z "$crate" ]; then
    echo "cannot find the crate name in $1/Cargo.toml" >&2
    exit 1
  fi
  file="$(echo "$crate" | tr - _).wasm"
  # shellcheck disable=SC2086 # CARGO_FLAGS is a word list on purpose
  cargo build --release --target wasm32-unknown-unknown --manifest-path "$1/Cargo.toml" ${CARGO_FLAGS:-} >&2
  mkdir -p "$1/dist"
  cp "$1/target/wasm32-unknown-unknown/release/$file" "$1/dist/$file"
  echo "built $1/dist/$file ($(wc -c < "$1/dist/$file" | tr -d ' ') bytes)" >&2
  echo "$1/dist/$file"
}

# install_codec CRATE_DIR [ID]: builds, signs, and copies the module and envelope into the codecs folder.
install_codec() {
  module=$(build_module "$1")
  if [ -n "${2:-}" ]; then
    msbe extension sign "$module" --key "$key" --version "$version" --id "$2"
  else
    msbe extension sign "$module" --key "$key" --version "$version"
  fi
  mkdir -p "$codecs"
  base=${module%.wasm}
  cp "$module" "$codecs/"
  # The envelope is written beside the module; copy both side by side.
  for envelope in "$base".toml "$base".json "$module".toml "$module".json; do
    [ -f "$envelope" ] && cp "$envelope" "$codecs/"
  done
  echo "installed codec into $codecs"
}

if [ "$command" = build ]; then
  build_module "$dir" > /dev/null
  exit 0
fi

if [ -f "$dir/program.toml" ]; then
  [ -z "$id" ] || { echo "--id names one codec; a bundle takes none" >&2; exit 2; }
  # The program's envelope is written as <provider id>.toml; keep it out of the source tree.
  stage=$(mktemp -d)
  trap 'rm -rf "$stage"' EXIT
  msbe extension sign "$dir/program.toml" --key "$key" --version "$version" --output "$stage"
  mkdir -p "$providers"
  cp "$stage"/*.toml "$providers/"
  echo "installed provider program into $providers"
  for codec in "$dir"/codecs/*/; do
    [ -f "$codec/Cargo.toml" ] && install_codec "${codec%/}"
  done
  if [ -d "$dir/overlays" ]; then
    echo "skipped $(find "$dir/overlays" -name '*.toml' | wc -l | tr -d ' ') overlays: MSBE reads none from the data directory yet" >&2
  fi
else
  case "$dir" in
    */codecs/*) install_codec "$dir" "$id" ;;
    *)
      echo "$dir is neither a provider bundle (no program.toml) nor under a codecs/ folder" >&2
      exit 1
      ;;
  esac
fi
echo "check with: msbe extension list"
