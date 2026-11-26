#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
target=${CARGO_TARGET_DIR:-${CARGO_BUILD_TARGET_DIR:-target}}
case "$target" in
    /*) ;;
    *) target="$root/$target" ;;
esac
binary="$target/release/cpw"
link="$HOME/.local/bin/cpw"

if [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
    printf 'No executable release binary at %s.\n' "$binary" >&2
    exit 1
fi
if [ -L "$link" ] && [ "$(readlink -- "$link")" = "$binary" ]; then
    printf '%s already points to this build.\n' "$link"
    exit 0
fi

mkdir -p -- "${link%/*}"
ln -sT -- "$binary" "$link"
printf 'Linked %s to %s.\n' "$link" "$binary"
