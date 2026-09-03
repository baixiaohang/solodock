#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' 'usage: stage-package.sh --binary PATH --output PATH --source-sha SHA --version VERSION --channel stable|main [--resource-report PATH]'
}

fail() {
  printf '%s\n' "$1" >&2
  exit 1
}

binary=''
output=''
source_sha=''
version=''
channel=''
resource_report=''
while (($#)); do
  case "$1" in
    --binary) binary=${2-}; shift 2 ;;
    --output) output=${2-}; shift 2 ;;
    --source-sha) source_sha=${2-}; shift 2 ;;
    --version) version=${2-}; shift 2 ;;
    --channel) channel=${2-}; shift 2 ;;
    --resource-report) resource_report=${2-}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

[[ -f $binary && ! -L $binary ]] || fail 'binary must be a safe regular file'
[[ $output = /* && $output != / && ! -e $output ]] || fail 'output must be a new absolute path'
[[ $source_sha =~ ^[0-9a-f]{40}$ ]] || fail 'source SHA must be a lowercase 40-character hexadecimal commit'
[[ $version =~ ^[0-9A-Za-z][0-9A-Za-z._-]{0,63}$ ]] || fail 'version is invalid'
[[ $channel == stable || $channel == main ]] || fail 'channel must be stable or main'
if [[ $channel == stable ]]; then
  [[ $version =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || fail 'stable version must be canonical SemVer'
else
  [[ $version == "main-${source_sha:0:12}" ]] || fail 'main version must match its source SHA'
fi
if [[ -n $resource_report ]]; then
  [[ -f $resource_report && ! -L $resource_report ]] || fail 'resource report must be a safe regular file'
fi

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
install -d -m 0755 -- "$output/docs"
install -m 0755 -- \
  "$repo_root/packaging/install.sh" \
  "$repo_root/packaging/solodock-backup" \
  "$repo_root/packaging/solodock-restore" \
  "$repo_root/packaging/solodock-update" \
  "$repo_root/packaging/verify-package.sh" \
  "$output/"
install -m 0755 -- "$binary" "$output/solodock"
install -m 0644 -- \
  "$repo_root/packaging/systemd/solodock.service" \
  "$repo_root/packaging/solodock.toml.example" \
  "$repo_root/README.md" \
  "$output/"
install -m 0644 -- \
  "$repo_root/docs/architecture.md" \
  "$repo_root/docs/operations.md" \
  "$repo_root/docs/recovery.md" \
  "$repo_root/docs/threat-model.md" \
  "$repo_root/docs/resource-budget.md" \
  "$output/docs/"
if [[ -n $resource_report ]]; then
  install -m 0644 -- "$resource_report" "$output/resource-report.json"
fi
printf '%s\n' "$source_sha" >"$output/SOURCE_SHA"
printf '%s\n' "$version" >"$output/VERSION"
identity_list=$(mktemp)
trap 'rm -f -- "$identity_list"' EXIT
(cd -- "$output" && find . -type f ! -name SHA256SUMS ! -name INSTALL_MANIFEST -print0 | LC_ALL=C sort -z | xargs -0 sha256sum) >"$identity_list"
package_identity=$(sha256sum "$identity_list" | awk '{print $1}')
printf '%s\n' \
  'FORMAT=solodock-install-v1' \
  "CHANNEL=$channel" \
  "VERSION=$version" \
  "SOURCE_SHA=$source_sha" \
  "PACKAGE_IDENTITY=$package_identity" >"$output/INSTALL_MANIFEST"
(cd -- "$output" && find . -type f ! -name SHA256SUMS -print0 | LC_ALL=C sort -z | xargs -0 sha256sum >SHA256SUMS)
"$output/verify-package.sh" "$output" >/dev/null
