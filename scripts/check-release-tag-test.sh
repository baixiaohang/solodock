#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
checker="$script_dir/check-release-tag.sh"
fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT

mkdir -p "$fixture/src"
cp "$script_dir/../rust-toolchain.toml" "$fixture/rust-toolchain.toml"
printf '%s\n' \
  '[package]' \
  'name = "solodock"' \
  'version = "1.2.3"' \
  'edition = "2024"' >"$fixture/Cargo.toml"
printf '%s\n' 'fn main() {}' >"$fixture/src/main.rs"
(cd -- "$fixture" && cargo generate-lockfile >/dev/null)

"$checker" v1.2.3 "$fixture" >/dev/null
for invalid in 1.2.3 v1.2 v01.2.3 v1.2.3-rc.1 v1.2.4; do
  if "$checker" "$invalid" "$fixture" >/dev/null 2>&1; then
    printf 'release tag checker accepted %s\n' "$invalid" >&2
    exit 1
  fi
done

printf '%s\n' 'release tag checker tests passed'
