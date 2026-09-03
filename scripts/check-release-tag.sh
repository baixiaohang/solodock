#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' 'usage: check-release-tag.sh TAG [REPOSITORY_ROOT]'
}

tag=${1-}
repo_root=${2-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)}
if (($# < 1 || $# > 2)); then
  usage >&2
  exit 2
fi
[[ $tag =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || {
  printf '%s\n' 'release tag must use canonical vMAJOR.MINOR.PATCH syntax' >&2
  exit 1
}
[[ -f $repo_root/Cargo.toml && ! -L $repo_root/Cargo.toml ]] || {
  printf '%s\n' 'repository root does not contain a safe Cargo.toml' >&2
  exit 1
}
command -v cargo >/dev/null || { printf '%s\n' 'cargo is required to verify the release version' >&2; exit 1; }
command -v jq >/dev/null || { printf '%s\n' 'jq is required to verify the release version' >&2; exit 1; }

metadata=$(cd -- "$repo_root" && cargo metadata --locked --no-deps --format-version 1)
version=$(jq -er '
  [.packages[] | select(.name == "solodock") | .version]
  | if length == 1 then .[0] else error("expected exactly one solodock package") end
' <<<"$metadata")
[[ $tag == "v$version" ]] || {
  printf 'release tag %s does not match Cargo package version %s\n' "$tag" "$version" >&2
  exit 1
}
printf 'release tag %s matches Cargo package version %s\n' "$tag" "$version"
