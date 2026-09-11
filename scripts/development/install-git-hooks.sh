#!/usr/bin/env sh
set -eu
root=$(git rev-parse --show-toplevel)
git -C "$root" config core.hooksPath .githooks
printf 'git hooks installed (core.hooksPath = .githooks)\n'
