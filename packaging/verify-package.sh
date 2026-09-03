#!/usr/bin/env bash
set -euo pipefail

fail() {
  printf '%s\n' "$1" >&2
  exit 1
}

[[ $# == 1 ]] || fail 'usage: verify-package.sh PACKAGE_DIRECTORY'
package=$1
[[ $package = /* && -d $package && ! -L $package ]] || fail 'package directory must be an absolute safe directory'

required=(
  SHA256SUMS
  SOURCE_SHA
  VERSION
  INSTALL_MANIFEST
  solodock
  install.sh
  solodock-backup
  solodock-restore
  solodock-update
  verify-package.sh
  solodock.service
  solodock.toml.example
)
for name in "${required[@]}"; do
  [[ -f $package/$name && ! -L $package/$name ]] || fail "package is missing a safe $name"
done
if find "$package" -xdev \( ! -type d ! -type f \) -print -quit | grep -q .; then
  fail 'package contains a symlink or unsupported file type'
fi
(cd -- "$package" && sha256sum --strict -c SHA256SUMS >/dev/null) || fail 'package checksum verification failed'

mapfile -t manifest <"$package/INSTALL_MANIFEST"
[[ ${#manifest[@]} == 5 ]] || fail 'install manifest has an invalid shape'
[[ ${manifest[0]} == 'FORMAT=solodock-install-v1' ]] || fail 'install manifest format is unsupported'
[[ ${manifest[1]} =~ ^CHANNEL=(stable|main)$ ]] || fail 'install manifest channel is invalid'
channel=${BASH_REMATCH[1]}
[[ ${manifest[2]} =~ ^VERSION=([0-9A-Za-z][0-9A-Za-z._-]{0,63})$ ]] || fail 'install manifest version is invalid'
version=${BASH_REMATCH[1]}
[[ ${manifest[3]} =~ ^SOURCE_SHA=([0-9a-f]{40})$ ]] || fail 'install manifest source SHA is invalid'
source_sha=${BASH_REMATCH[1]}
[[ ${manifest[4]} =~ ^PACKAGE_IDENTITY=([0-9a-f]{64})$ ]] || fail 'install manifest package identity is invalid'
package_identity=${BASH_REMATCH[1]}

[[ $(<"$package/VERSION") == "$version" ]] || fail 'install manifest version does not match VERSION'
[[ $(<"$package/SOURCE_SHA") == "$source_sha" ]] || fail 'install manifest source SHA does not match SOURCE_SHA'
if [[ $channel == stable ]]; then
  [[ $version =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || fail 'stable package version is not canonical SemVer'
else
  [[ $version == "main-${source_sha:0:12}" ]] || fail 'main package version does not match its source SHA'
fi

identity_list=$(mktemp)
trap 'rm -f -- "$identity_list"' EXIT
(cd -- "$package" && find . -type f ! -name SHA256SUMS ! -name INSTALL_MANIFEST -print0 | LC_ALL=C sort -z | xargs -0 sha256sum) >"$identity_list"
actual_identity=$(sha256sum "$identity_list" | awk '{print $1}')
[[ $actual_identity == "$package_identity" ]] || fail 'install manifest package identity does not match package contents'

printf '%s\t%s\t%s\t%s\n' "$channel" "$version" "$source_sha" "$package_identity"
