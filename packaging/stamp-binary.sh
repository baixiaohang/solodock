#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' 'usage: stamp-binary.sh --binary PATH --generation IDENTIFIER'
}

binary=''
generation=''
while (($#)); do
  case "$1" in
    --binary) binary=${2-}; shift 2 ;;
    --generation) generation=${2-}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

[[ -f $binary && ! -L $binary ]] || { printf '%s\n' 'binary must be a safe regular file' >&2; exit 2; }
[[ $generation =~ ^[0-9A-Za-z][0-9A-Za-z._-]{0,63}$ ]] || { printf '%s\n' 'generation is invalid' >&2; exit 2; }
command -v objcopy >/dev/null || { printf '%s\n' 'objcopy is required to stamp the package binary' >&2; exit 1; }
command -v readelf >/dev/null || { printf '%s\n' 'readelf is required to verify the package binary stamp' >&2; exit 1; }
if readelf -SW "$binary" | grep -Fq '.solodock.package'; then
  printf '%s\n' 'binary already contains a SoloDock package generation' >&2
  exit 1
fi

stamp=$(mktemp)
trap 'rm -f -- "$stamp"' EXIT
printf '%s\n' "$generation" >"$stamp"
objcopy \
  --add-section ".solodock.package=$stamp" \
  --set-section-flags .solodock.package=readonly,data \
  "$binary"
readelf -p .solodock.package "$binary" | grep -Fq "$generation"
