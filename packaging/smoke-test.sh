#!/usr/bin/env bash
set -euo pipefail
validator="${CARGO_TARGET_DIR:-target}/release/solodock"
export SOLODOCK_SMOKE_REAL_BINARY
SOLODOCK_SMOKE_REAL_BINARY=$(realpath -e -- "$validator")
fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT
managed_install_dir() {
  local root=$1
  local target
  target=$(readlink -- "$root/usr/local/bin/solodock")
  printf '%s%s\n' "$root" "${target%/solodock}"
}
assert_install_coherent() {
  local root=$1
  local directory prefix name
  directory=$(managed_install_dir "$root")
  prefix=${directory#"$root"}
  [[ $prefix =~ ^/usr/local/lib/solodock/generations/[0-9A-Za-z._-]+\.[0-9a-f]{64}\.[0-9A-Za-z]{12}$ ]]
  for name in solodock solodock-update solodock-backup solodock-restore; do
    [[ $(readlink -- "$root/usr/local/bin/$name") == "$prefix/$name" ]]
    [[ -f $directory/$name && ! -L $directory/$name ]]
  done
  [[ $(readlink -- "$root/etc/systemd/system/solodock.service") == "$prefix/solodock.service" ]]
  [[ -f $directory/solodock.service && ! -L $directory/solodock.service ]]
  [[ -f $directory/INSTALL_MANIFEST && ! -L $directory/INSTALL_MANIFEST ]]
}
capture_install_snapshot() {
  local root=$1
  local directory name channel version source_sha package_identity generation nonce
  directory=$(managed_install_dir "$root")
  for name in solodock solodock-update solodock-backup solodock-restore; do
    printf '%s=%s\n' "$name" "$(readlink -- "$root/usr/local/bin/$name")"
  done
  printf 'unit=%s\n' "$(readlink -- "$root/etc/systemd/system/solodock.service")"
  channel=$(sed -n 's/^CHANNEL=//p' "$directory/INSTALL_MANIFEST")
  version=$(sed -n 's/^VERSION=//p' "$directory/INSTALL_MANIFEST")
  source_sha=$(sed -n 's/^SOURCE_SHA=//p' "$directory/INSTALL_MANIFEST")
  package_identity=$(sed -n 's/^PACKAGE_IDENTITY=//p' "$directory/INSTALL_MANIFEST")
  generation=$(basename -- "$directory")
  [[ $generation == "$version.$package_identity."* ]]
  nonce=${generation#"$version.$package_identity."}
  [[ $nonce =~ ^[0-9A-Za-z]{12}$ ]]
  printf 'api-visible=%s:%s:%s:%s\n' "$channel" "$version" "$source_sha" "$package_identity"
  (cd -- "$directory" && sha256sum INSTALL_MANIFEST solodock solodock-update solodock-backup solodock-restore verify-package.sh solodock.service)
  find "$root/usr/local/lib/solodock/generations" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | LC_ALL=C sort
}
fake="$fixture/solodock"
printf '%s\n' \
  '#!/bin/sh' \
  'if [ "${1-}" = inspect-packaged-config ]; then exec "$SOLODOCK_SMOKE_REAL_BINARY" "$@"; fi' \
  'exit 0' >"$fake"
chmod 0755 "$fake"
stamped_binary="$fixture/stamped-binary"
cp /bin/true "$stamped_binary"
./packaging/stamp-binary.sh --binary "$stamped_binary" --generation verified-release-v1
readelf -p .solodock.package "$stamped_binary" | grep -Fq verified-release-v1
if ./packaging/stamp-binary.sh --binary "$stamped_binary" --generation duplicate >"$fixture/stamp.stdout" 2>"$fixture/stamp.stderr"; then
  printf '%s\n' 'binary stamp accepted a second package generation' >&2
  exit 1
fi
fixture_sha=$(printf '%040d' 2)
install_package_010="$fixture/install-package-010"
install_package_011="$fixture/install-package-011"
install_package_012="$fixture/install-package-012"
install_package_013="$fixture/install-package-013"
for package_spec in \
  "$install_package_010:0.1.0" \
  "$install_package_011:0.1.1" \
  "$install_package_012:0.1.2" \
  "$install_package_013:0.1.3"; do
  package_path=${package_spec%%:*}
  package_version=${package_spec##*:}
  ./packaging/stage-package.sh \
    --binary "$fake" \
    --output "$package_path" \
    --source-sha "$fixture_sha" \
    --version "$package_version" \
    --channel stable
done
"$install_package_010/install.sh" --version 0.1.0 --destdir "$fixture/root" >/dev/null
assert_install_coherent "$fixture/root"
root_install=$(managed_install_dir "$fixture/root")
grep -qx 'CHANNEL=stable' "$root_install/INSTALL_MANIFEST"
[[ -x $root_install/verify-package.sh ]]
snapshot_failure_root="$fixture/snapshot-failure-root"
"$install_package_010/install.sh" --version 0.1.0 --destdir "$snapshot_failure_root" >/dev/null
snapshot_before="$fixture/snapshot-failure-before"
snapshot_after="$fixture/snapshot-failure-after"
capture_install_snapshot "$snapshot_failure_root" >"$snapshot_before"
if SOLODOCK_INSTALL_FAIL_AT=snapshot-solodock.service \
  "$install_package_011/install.sh" --version 0.1.1 --destdir "$snapshot_failure_root" >"$fixture/snapshot-failure.stdout" 2>"$fixture/snapshot-failure.stderr"; then
  printf '%s\n' 'installer accepted an injected snapshot failure' >&2
  exit 1
fi
grep -Fq 'injected installer failure at snapshot-solodock.service' "$fixture/snapshot-failure.stderr"
capture_install_snapshot "$snapshot_failure_root" >"$snapshot_after"
cmp "$snapshot_before" "$snapshot_after"
if find "$snapshot_failure_root/usr/local/lib/solodock" -maxdepth 1 -type d -name '.install-transaction.*' -print -quit | grep -q .; then
  printf '%s\n' 'snapshot failure left an installer transaction behind' >&2
  exit 1
fi
custom_layout_install_root="$fixture/custom-layout-install-root"
"$install_package_010/install.sh" --version 0.1.0 --destdir "$custom_layout_install_root" >/dev/null
sed -i 's#^state_directory = .*#state_directory = "/srv/solodock-state"#' "$custom_layout_install_root/etc/solodock/config.toml"
custom_layout_before="$fixture/custom-layout-install-before"
custom_layout_after="$fixture/custom-layout-install-after"
capture_install_snapshot "$custom_layout_install_root" >"$custom_layout_before"
custom_layout_config_sha=$(sha256sum "$custom_layout_install_root/etc/solodock/config.toml" | awk '{print $1}')
if "$install_package_011/install.sh" --version 0.1.1 --destdir "$custom_layout_install_root" >"$fixture/custom-layout-install.stdout" 2>"$fixture/custom-layout-install.stderr"; then
  printf '%s\n' 'installer accepted a custom packaged state layout' >&2
  exit 1
fi
grep -Fq 'packaged configuration preflight failed' "$fixture/custom-layout-install.stderr"
capture_install_snapshot "$custom_layout_install_root" >"$custom_layout_after"
cmp "$custom_layout_before" "$custom_layout_after"
[[ $(sha256sum "$custom_layout_install_root/etc/solodock/config.toml" | awk '{print $1}') == "$custom_layout_config_sha" ]]

invalid_socket_root="$fixture/invalid-socket-root"
mkdir -p "$invalid_socket_root/var/run"
: >"$invalid_socket_root/var/run/docker.sock"
if "$install_package_010/install.sh" --version 0.1.0 --destdir "$invalid_socket_root" >"$fixture/invalid-socket.stdout" 2>"$fixture/invalid-socket.stderr"; then
  printf '%s\n' 'installer accepted a non-socket Docker path' >&2
  exit 1
fi
grep -Fq 'Docker socket has an unsafe type or group' "$fixture/invalid-socket.stderr"
[[ ! -e $invalid_socket_root/usr/local/lib/solodock ]]

wrong_group=$(id -Gn | tr ' ' '\n' | awk '$0 != "docker" { print; exit }')
[[ -n $wrong_group ]]
wrong_group_socket_root="$fixture/wrong-group-socket-root"
mkdir -p "$wrong_group_socket_root/var/run"
python3 - "$wrong_group_socket_root/var/run/docker.sock" <<'PY'
import socket
import sys

sock = socket.socket(socket.AF_UNIX)
sock.bind(sys.argv[1])
sock.close()
PY
[[ -S $wrong_group_socket_root/var/run/docker.sock ]]
chgrp "$wrong_group" "$wrong_group_socket_root/var/run/docker.sock"
if "$install_package_010/install.sh" --version 0.1.0 --destdir "$wrong_group_socket_root" >"$fixture/wrong-group-socket.stdout" 2>"$fixture/wrong-group-socket.stderr"; then
  printf '%s\n' 'installer accepted a Docker socket with the wrong group' >&2
  exit 1
fi
grep -Fq 'Docker socket has an unsafe type or group' "$fixture/wrong-group-socket.stderr"
[[ ! -e $wrong_group_socket_root/usr/local/lib/solodock ]]

sed -i 's#^public_origin = .*#public_origin = "https://[::1]:8443"#' "$fixture/root/etc/solodock/config.toml"
printf '# retained\n' >>"$fixture/root/etc/solodock/config.toml"
"$install_package_011/install.sh" --version 0.1.1 --destdir "$fixture/root" >/dev/null
grep -q retained "$fixture/root/etc/solodock/config.toml"
unknown="$fixture/unknown-root"
mkdir -p "$unknown/usr/local/bin"
printf '%s\n' 'do not replace' >"$unknown/usr/local/bin/solodock"
if "$install_package_012/install.sh" --version 0.1.2 --destdir "$unknown" >"$fixture/install.stdout" 2>"$fixture/install.stderr"; then
  printf '%s\n' 'installer accepted an unknown binary target' >&2
  exit 1
fi
[[ ! -e $unknown/usr/local/lib/solodock/0.1.2 && ! -e $unknown/etc/solodock && ! -s $fixture/install.stdout ]]

(cd "$fixture" && "$install_package_013/install.sh" --version 0.1.3 --destdir "$fixture/package-root" >/dev/null)
assert_install_coherent "$fixture/package-root"
package_root_install=$(managed_install_dir "$fixture/package-root")
[[ -x $package_root_install/solodock ]]

staged_package="$fixture/staged-package"
./packaging/stage-package.sh \
  --binary "$fake" \
  --output "$staged_package" \
  --source-sha "$fixture_sha" \
  --version 0.1.3 \
  --channel stable
(cd "$staged_package" && sha256sum -c SHA256SUMS >/dev/null)
grep -qx "$(printf '%040d' 2)" "$staged_package/SOURCE_SHA"
grep -qx '0.1.3' "$staged_package/VERSION"
grep -qx 'CHANNEL=stable' "$staged_package/INSTALL_MANIFEST"
"$staged_package/verify-package.sh" "$staged_package" | grep -q $'^stable\t0.1.3\t'
for staged_asset in INSTALL_MANIFEST solodock install.sh solodock-backup solodock-restore solodock-update verify-package.sh solodock.service solodock.toml.example README.md docs/operations.md; do
  [[ -f $staged_package/$staged_asset && ! -L $staged_package/$staged_asset ]]
done

./packaging/solodock-update --help >/dev/null
if ./packaging/solodock-update --health-url 'http://0.0.0.0:8080/healthz' >"$fixture/update.stdout" 2>"$fixture/update.stderr"; then
  printf '%s\n' 'updater retained the removed health URL override' >&2
  exit 1
fi
[[ ! -s $fixture/update.stdout ]]
grep -Fq 'usage: solodock-update' "$fixture/update.stderr"

update_package="$fixture/update-package"
fake_bin="$fixture/fake-bin"
mkdir -p "$fake_bin"
mismatched_sha=$(printf '%040d' 1)
./packaging/stage-package.sh \
  --binary "$fake" \
  --output "$update_package" \
  --source-sha "$mismatched_sha" \
  --version "main-${mismatched_sha:0:12}" \
  --channel main
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  '[[ -z ${SOLODOCK_SMOKE_GH_LOG:-} ]] || printf "%s\n" "$*" >>"$SOLODOCK_SMOKE_GH_LOG"' \
  'if [[ $1 == --version ]]; then printf "%s\n" "${SOLODOCK_SMOKE_GH_VERSION:-gh version 2.40.1 (fixture)}"; exit 0; fi' \
  'if [[ $1 == auth && $2 == status ]]; then exit 0; fi' \
  'if [[ $1 == attestation && $2 == verify ]]; then' \
  '  if [[ ${3-} == --help ]]; then [[ ${SOLODOCK_SMOKE_ATTESTATION_SUPPORTED:-yes} == yes ]]; exit; fi' \
  '  printf "%s\n" "${@:3}" >"$SOLODOCK_SMOKE_ATTESTATION_ARGS"' \
  '  [[ ${SOLODOCK_SMOKE_ATTESTATION_RESULT:-success} == success ]]' \
  '  exit 0' \
  'fi' \
  "if [[ \$1 == run && \$2 == list ]]; then printf '%s\\t%s\\t%s\\t%s\\n' 123 $(printf '%040d' 0) main success; exit 0; fi" \
  'if [[ $1 == run && $2 == download ]]; then' \
  '  while (($#)); do if [[ $1 == --dir ]]; then destination=$2; break; fi; shift; done' \
  '  mkdir -p "$destination/solodock-package"' \
  '  cp -a "$SOLODOCK_SMOKE_PACKAGE/." "$destination/solodock-package/"' \
  '  exit 0' \
  'fi' \
  'if [[ $1 == release && $2 == list ]]; then printf "%s\t%s\t%s\n" v0.1.1 false false; exit 0; fi' \
  'if [[ $1 == release && $2 == view ]]; then' \
  '  tag=${SOLODOCK_SMOKE_LATEST_TAG:-v0.1.0}' \
  '  [[ ${3-} == --repo ]] || tag=$3' \
  '  printf "%s\t%s\t%s\n" "$tag" false false' \
  '  exit 0' \
  'fi' \
  "if [[ \$1 == api ]]; then printf '%s\\n' $(printf '%040d' 0); exit 0; fi" \
  'if [[ $1 == release && $2 == download ]]; then' \
  '  while (($#)); do if [[ $1 == --dir ]]; then destination=$2; break; fi; shift; done' \
  '  mkdir -p "$destination"' \
  '  cp -a "$SOLODOCK_SMOKE_RELEASE/." "$destination/"' \
  '  exit 0' \
  'fi' \
  'exit 1' >"$fake_bin/gh"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  '[[ -z ${SOLODOCK_SMOKE_SUDO_LOG:-} ]] || printf "%s\n" "$*" >>"$SOLODOCK_SMOKE_SUDO_LOG"' \
  'exit 0' >"$fake_bin/sudo"
chmod 0755 "$fake_bin/gh" "$fake_bin/sudo"
attestation_args="$fixture/attestation.args"
gh_log="$fixture/gh.log"
preflight_root="$fixture/preflight-root"
"$install_package_010/install.sh" --version 0.1.0 --destdir "$preflight_root" >/dev/null
unknown_channel_root="$fixture/unknown-channel-root"
mkdir -p "$unknown_channel_root/usr/local/bin" "$unknown_channel_root/usr/local/lib/solodock/custom-build"
install -m 0755 "$fake" "$unknown_channel_root/usr/local/lib/solodock/custom-build/solodock"
ln -s /usr/local/lib/solodock/custom-build/solodock "$unknown_channel_root/usr/local/bin/solodock"
rm -f "$gh_log"
if PATH="$fake_bin:$PATH" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$unknown_channel_root" \
  SOLODOCK_SMOKE_GH_LOG="$gh_log" \
  ./packaging/solodock-update >"$fixture/unknown-channel.stdout" 2>"$fixture/unknown-channel.stderr"; then
  printf '%s\n' 'updater inferred a channel from an unknown legacy installation' >&2
  exit 1
fi
grep -qx 'cannot infer the update channel from this legacy installation; pass --channel stable or --channel main explicitly' "$fixture/unknown-channel.stderr"
[[ ! -e $gh_log ]]
damaged_manifest_root="$fixture/damaged-manifest-root"
cp -a "$preflight_root" "$damaged_manifest_root"
damaged_install=$(managed_install_dir "$damaged_manifest_root")
sed -i 's/^CHANNEL=stable$/CHANNEL=invalid/' "$damaged_install/INSTALL_MANIFEST"
rm -f "$gh_log"
if PATH="$fake_bin:$PATH" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$damaged_manifest_root" \
  SOLODOCK_SMOKE_GH_LOG="$gh_log" \
  ./packaging/solodock-update --channel main >"$fixture/damaged-manifest.stdout" 2>"$fixture/damaged-manifest.stderr"; then
  printf '%s\n' 'updater accepted a damaged installed manifest' >&2
  exit 1
fi
grep -qx 'installed SoloDock manifest channel is invalid' "$fixture/damaged-manifest.stderr"
[[ ! -e $gh_log ]]
rm -f "$gh_log" "$fixture/implicit-stable-selector-sudo.log"
if PATH="$fake_bin:$PATH" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$preflight_root" \
  SOLODOCK_SMOKE_GH_LOG="$gh_log" \
  SOLODOCK_SMOKE_SUDO_LOG="$fixture/implicit-stable-selector-sudo.log" \
  ./packaging/solodock-update --branch main >"$fixture/implicit-stable-selector.stdout" 2>"$fixture/implicit-stable-selector.stderr"; then
  printf '%s\n' 'updater accepted a main-only selector for an inferred stable channel' >&2
  exit 1
fi
grep -qx -- '--branch and --workflow are valid only with --channel main' "$fixture/implicit-stable-selector.stderr"
[[ ! -e $gh_log && ! -e $fixture/implicit-stable-selector-sudo.log ]]
for selector in branch workflow; do
  rm -f "$gh_log"
  if PATH="$fake_bin:$PATH" SOLODOCK_SMOKE_GH_LOG="$gh_log" \
    ./packaging/solodock-update --channel stable "--$selector" main >"$fixture/selector.stdout" 2>"$fixture/selector.stderr"; then
    printf 'updater accepted a stable channel with the main-only %s selector\n' "$selector" >&2
    exit 1
  fi
  grep -qx -- '--branch and --workflow are valid only with --channel main' "$fixture/selector.stderr"
  [[ ! -e $gh_log ]]
done

if PATH="$fake_bin:$PATH" SOLODOCK_SMOKE_GH_LOG="$gh_log" \
  ./packaging/solodock-update --channel candidate >"$fixture/channel.stdout" 2>"$fixture/channel.stderr"; then
  printf '%s\n' 'updater accepted an unknown channel' >&2
  exit 1
fi
grep -qx 'channel must be stable or main' "$fixture/channel.stderr"
[[ ! -e $gh_log ]]

attestation_support_gh_log="$fixture/attestation-support-gh.log"
attestation_support_sudo_log="$fixture/attestation-support-sudo.log"
attestation_support_before="$fixture/attestation-support-before"
attestation_support_after="$fixture/attestation-support-after"
capture_install_snapshot "$preflight_root" >"$attestation_support_before"
rm -f "$attestation_support_gh_log" "$attestation_support_sudo_log"
if PATH="$fake_bin:$PATH" \
  SOLODOCK_SMOKE_ATTESTATION_SUPPORTED=no \
  SOLODOCK_SMOKE_GH_VERSION='gh version 2.40.1 (fixture)' \
  SOLODOCK_SMOKE_GH_LOG="$attestation_support_gh_log" \
  SOLODOCK_SMOKE_SUDO_LOG="$attestation_support_sudo_log" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$preflight_root" \
  ./packaging/solodock-update --channel main >"$fixture/attestation-support.stdout" 2>"$fixture/attestation-support.stderr"; then
  printf '%s\n' 'updater accepted a GitHub CLI without attestation support' >&2
  exit 1
fi
grep -Fxq 'GitHub CLI is missing the required gh attestation verify capability.' "$fixture/attestation-support.stderr"
grep -Fxq 'Current GitHub CLI: gh version 2.40.1 (fixture)' "$fixture/attestation-support.stderr"
grep -Fxq 'https://github.com/cli/cli/blob/trunk/docs/install_linux.md' "$fixture/attestation-support.stderr"
grep -Fxq '  gh attestation verify --help' "$fixture/attestation-support.stderr"
grep -Fxq '  gh auth status' "$fixture/attestation-support.stderr"
printf '%s\n' 'attestation verify --help' '--version' >"$fixture/attestation-support-gh.expected"
cmp "$fixture/attestation-support-gh.expected" "$attestation_support_gh_log"
[[ ! -e $attestation_support_sudo_log ]]
capture_install_snapshot "$preflight_root" >"$attestation_support_after"
cmp "$attestation_support_before" "$attestation_support_after"

if PATH="$fake_bin:$PATH" \
  SOLODOCK_SMOKE_PACKAGE="$update_package" \
  SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
  SOLODOCK_SMOKE_ATTESTATION_RESULT=failure \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$preflight_root" \
  ./packaging/solodock-update --channel main >"$fixture/attestation.stdout" 2>"$fixture/attestation.stderr"; then
  printf '%s\n' 'updater accepted an artifact without valid provenance' >&2
  exit 1
fi
grep -qx 'artifact provenance attestation verification failed' "$fixture/attestation.stderr"
attested_subject=$(head -n 1 "$attestation_args")
[[ $attested_subject == /tmp/solodock-update.*/solodock-package/SHA256SUMS ]]
grep -Fxq -- '--repo' "$attestation_args"
grep -Fxq -- 'baixiaohang/solodock' "$attestation_args"
grep -Fxq -- '--signer-workflow' "$attestation_args"
grep -Fxq -- 'baixiaohang/solodock/.github/workflows/ci.yml' "$attestation_args"
grep -Fxq -- '--source-ref' "$attestation_args"
grep -Fxq -- 'refs/heads/main' "$attestation_args"
grep -Fxq -- '--source-digest' "$attestation_args"
grep -Fxq -- "$(printf '%040d' 0)" "$attestation_args"
grep -Fxq -- '--deny-self-hosted-runners' "$attestation_args"

if PATH="$fake_bin:$PATH" \
  SOLODOCK_SMOKE_PACKAGE="$update_package" \
  SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
  SOLODOCK_SMOKE_SUDO_LOG="$fixture/preflight-sudo.log" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$preflight_root" \
  ./packaging/solodock-update --channel main >"$fixture/source-sha.stdout" 2>"$fixture/source-sha.stderr"; then
  printf '%s\n' 'updater accepted an artifact from a different source commit' >&2
  exit 1
fi
grep -qx 'package source SHA does not match its trusted source' "$fixture/source-sha.stderr"
if grep -Eq 'systemctl stop|solodock-backup|install\.sh' "$fixture/preflight-sudo.log"; then
  printf '%s\n' 'failed main preflight reached a mutation command' >&2
  exit 1
fi

release_assets="$fixture/release-assets"
release_stage="$fixture/release-stage/solodock-package"
mkdir -p "$release_assets"
./packaging/stage-package.sh \
  --binary "$fake" \
  --output "$release_stage" \
  --source-sha "$(printf '%040d' 0)" \
  --version 0.1.1 \
  --channel stable
(cd "$fixture/release-stage" && tar -czf "$release_assets/solodock-v0.1.0-ubuntu-24.04-x86_64.tar.gz" solodock-package)
printf '%040d\n' 0 >"$release_assets/SOURCE_SHA"
(cd "$release_assets" && sha256sum solodock-v0.1.0-ubuntu-24.04-x86_64.tar.gz SOURCE_SHA >SHA256SUMS)
rm -f "$gh_log"
if PATH="$fake_bin:$PATH" \
  SOLODOCK_SMOKE_RELEASE="$release_assets" \
  SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
  SOLODOCK_SMOKE_GH_LOG="$gh_log" \
  SOLODOCK_SMOKE_SUDO_LOG="$fixture/stable-preflight-sudo.log" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$preflight_root" \
  ./packaging/solodock-update >"$fixture/stable-source.stdout" 2>"$fixture/stable-source.stderr"; then
  printf '%s\n' 'stable updater accepted a package version that differs from its tag' >&2
  exit 1
fi
grep -qx 'package version does not match its trusted source' "$fixture/stable-source.stderr"
if grep -Eq 'systemctl stop|solodock-backup|install\.sh' "$fixture/stable-preflight-sudo.log"; then
  printf '%s\n' 'failed stable preflight reached a mutation command' >&2
  exit 1
fi
grep -Fqx 'release view --repo baixiaohang/solodock --json tagName,isDraft,isPrerelease --jq [.tagName, .isDraft, .isPrerelease] | @tsv' "$gh_log"
grep -Fqx 'release view v0.1.0 --repo baixiaohang/solodock --json tagName,isDraft,isPrerelease --jq [.tagName, .isDraft, .isPrerelease] | @tsv' "$gh_log"
if grep -Fq 'release list' "$gh_log"; then
  printf '%s\n' 'stable updater treated release creation order as the Latest Release fact' >&2
  exit 1
fi
attested_subject=$(head -n 1 "$attestation_args")
[[ $attested_subject == /tmp/solodock-update.*/SHA256SUMS ]]
grep -Fxq -- 'baixiaohang/solodock/.github/workflows/release.yml' "$attestation_args"
grep -Fxq -- 'refs/tags/v0.1.0' "$attestation_args"
grep -Fxq -- "$(printf '%040d' 0)" "$attestation_args"

printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  '[[ -z ${SOLODOCK_SMOKE_SUDO_LOG:-} ]] || printf "%s\n" "$*" >>"$SOLODOCK_SMOKE_SUDO_LOG"' \
  'if [[ ${1-} == -n && ${2-} == true ]]; then exit 0; fi' \
  'exec "$@"' >"$fake_bin/sudo"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'printf "%s\n" "$*" >>"$SOLODOCK_SMOKE_SYSTEMCTL_LOG"' \
  'exit 0' >"$fake_bin/systemctl"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  '[[ -z ${SOLODOCK_SMOKE_CURL_LOG:-} ]] || printf "%s\n" "$*" >>"$SOLODOCK_SMOKE_CURL_LOG"' \
  'for argument in "$@"; do' \
  '  [[ $argument == --write-out ]] && { printf "%s" image/svg+xml; exit 0; }' \
  'done' \
  'exit 0' >"$fake_bin/curl"
chmod 0755 "$fake_bin/sudo" "$fake_bin/systemctl" "$fake_bin/curl"

new_binary="$fixture/new-solodock"
printf '%s\n' \
  '#!/bin/sh' \
  'if [ "${1-}" = inspect-packaged-config ]; then exec "$SOLODOCK_SMOKE_REAL_BINARY" "$@"; fi' \
  'exit 0' \
  '# new package binary' >"$new_binary"
chmod 0755 "$new_binary"
trusted_sha=$(printf '%040d' 0)
main_package="$fixture/main-package"
stable_package="$fixture/stable-package"
./packaging/stage-package.sh --binary "$new_binary" --output "$main_package" --source-sha "$trusted_sha" --version "main-${trusted_sha:0:12}" --channel main
./packaging/stage-package.sh --binary "$new_binary" --output "$stable_package" --source-sha "$trusted_sha" --version 0.2.0 --channel stable

old_main_sha=$(printf '1%.0s' {1..40})
old_stable_sha=$(printf '2%.0s' {1..40})
old_main_package="$fixture/old-main-package"
old_stable_package="$fixture/old-stable-package"
same_binary_main_package="$fixture/same-binary-main-package"
./packaging/stage-package.sh --binary "$fake" --output "$old_main_package" --source-sha "$old_main_sha" --version "main-${old_main_sha:0:12}" --channel main
./packaging/stage-package.sh --binary "$fake" --output "$old_stable_package" --source-sha "$old_stable_sha" --version 0.1.0 --channel stable
./packaging/stage-package.sh --binary "$new_binary" --output "$same_binary_main_package" --source-sha "$old_main_sha" --version "main-${old_main_sha:0:12}" --channel main

stable_assets="$fixture/stable-assets"
mkdir -p "$stable_assets" "$fixture/stable-archive"
cp -a "$stable_package" "$fixture/stable-archive/solodock-package"
(cd "$fixture/stable-archive" && tar -czf "$stable_assets/solodock-v0.2.0-ubuntu-24.04-x86_64.tar.gz" solodock-package)
printf '%s\n' "$trusted_sha" >"$stable_assets/SOURCE_SHA"
(cd "$stable_assets" && sha256sum solodock-v0.2.0-ubuntu-24.04-x86_64.tar.gz SOURCE_SHA >SHA256SUMS)

run_successful_update() {
  local name=$1
  local initial_package=$2
  local selected_channel=${3-}
  local legacy_install=${4-0}
  local listen_override=${5-}
  local root="$fixture/$name-root"
  local backups="$fixture/$name-backups"
  local systemctl_log="$fixture/$name-systemctl.log"
  local gh_trace="$fixture/$name-gh.log"
  local output="$fixture/$name.stdout"
  local curl_log="$fixture/$name-curl.log"
  local initial_version
  initial_version=$(<"$initial_package/VERSION")
  "$initial_package/install.sh" --version "$initial_version" --destdir "$root" >/dev/null
  sed -i 's#^public_origin = .*#public_origin = "https://[::1]:8443"#' "$root/etc/solodock/config.toml"
  if [[ -n $listen_override ]]; then
    sed -i "s#^listen_address = .*#listen_address = \"$listen_override\"#" "$root/etc/solodock/config.toml"
  fi
  if [[ $legacy_install == 1 ]]; then
    local generation legacy name
    generation=$(managed_install_dir "$root")
    legacy="$root/usr/local/lib/solodock/$initial_version"
    mkdir -p -- "$legacy"
    cp -a "$generation/." "$legacy/"
    rm -- "$legacy/INSTALL_MANIFEST"
    for name in solodock solodock-update solodock-backup solodock-restore; do
      rm -- "$root/usr/local/bin/$name"
      ln -s -- "/usr/local/lib/solodock/$initial_version/$name" "$root/usr/local/bin/$name"
    done
    rm -- "$root/etc/systemd/system/solodock.service"
    cp -- "$legacy/solodock.service" "$root/etc/systemd/system/solodock.service"
  fi
  : >"$systemctl_log"
  : >"$curl_log"
  local channel_args=()
  [[ -z $selected_channel ]] || channel_args=(--channel "$selected_channel")
  PATH="$fake_bin:$PATH" \
    SOLODOCK_UPDATE_TEST_MODE=1 \
    SOLODOCK_UPDATE_TEST_ROOT="$root" \
    SOLODOCK_SMOKE_PACKAGE="$main_package" \
    SOLODOCK_SMOKE_RELEASE="$stable_assets" \
    SOLODOCK_SMOKE_LATEST_TAG=v0.2.0 \
    SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
    SOLODOCK_SMOKE_GH_LOG="$gh_trace" \
    SOLODOCK_SMOKE_SYSTEMCTL_LOG="$systemctl_log" \
    SOLODOCK_SMOKE_CURL_LOG="$curl_log" \
    ./packaging/solodock-update "${channel_args[@]}" --backup-dir "$backups" >"$output"
  printf '%s\n' "$root" "$backups" "$systemctl_log" "$output" "$gh_trace" "$curl_log"
}

mapfile -t main_result < <(run_successful_update main-success "$old_main_package" '' 1)
main_root=${main_result[0]}
main_backups=${main_result[1]}
main_systemctl_log=${main_result[2]}
main_output=${main_result[3]}
assert_install_coherent "$main_root"
main_install=$(managed_install_dir "$main_root")
grep -qx 'stop solodock.service' "$main_systemctl_log"
grep -qx 'start solodock.service' "$main_systemctl_log"
find "$main_backups" -maxdepth 1 -type f -name '*.tar' -print -quit | grep -q .
find "$main_backups" -maxdepth 1 -type f -name '*.tar.sha256' -print -quit | grep -q .
grep -Fq 'from CI run 123' "$main_output"
grep -qx 'CHANNEL=main' "$main_install/INSTALL_MANIFEST"

mapfile -t ipv6_result < <(run_successful_update ipv6-success "$old_main_package" main 0 '[::1]:9124')
ipv6_curl_log=${ipv6_result[5]}
grep -Fq 'http://[::1]:9124/healthz' "$ipv6_curl_log"
grep -Fq 'http://[::1]:9124/favicon.svg' "$ipv6_curl_log"

custom_update_root="$fixture/custom-update-root"
"$old_main_package/install.sh" --version "main-${old_main_sha:0:12}" --destdir "$custom_update_root" >/dev/null
sed -i 's#^state_directory = .*#state_directory = "/srv/solodock-state"#' "$custom_update_root/etc/solodock/config.toml"
capture_install_snapshot "$custom_update_root" >"$fixture/custom-update-before"
: >"$fixture/custom-update-systemctl.log"
: >"$fixture/custom-update-sudo.log"
if PATH="$fake_bin:$PATH" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$custom_update_root" \
  SOLODOCK_SMOKE_PACKAGE="$main_package" \
  SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
  SOLODOCK_SMOKE_GH_LOG="$fixture/custom-update-gh.log" \
  SOLODOCK_SMOKE_SYSTEMCTL_LOG="$fixture/custom-update-systemctl.log" \
  SOLODOCK_SMOKE_SUDO_LOG="$fixture/custom-update-sudo.log" \
  ./packaging/solodock-update --channel main --backup-dir "$fixture/custom-update-backups" >"$fixture/custom-update.stdout" 2>"$fixture/custom-update.stderr"; then
  printf '%s\n' 'updater accepted a custom packaged state layout' >&2
  exit 1
fi
grep -Fq 'downloaded package rejected the installed packaged configuration' "$fixture/custom-update.stderr"
capture_install_snapshot "$custom_update_root" >"$fixture/custom-update-after"
cmp "$fixture/custom-update-before" "$fixture/custom-update-after"
if grep -Eq '(^| )(stop solodock\.service|.*solodock-backup|.*install\.sh)($| )' "$fixture/custom-update-sudo.log"; then
  printf '%s\n' 'custom-layout updater preflight reached a mutation command' >&2
  exit 1
fi
[[ ! -e $fixture/custom-update-backups ]]

malformed_inspector="$fixture/malformed-inspector"
printf '%s\n' \
  '#!/bin/sh' \
  'if [ "${1-}" = inspect-packaged-config ]; then printf "%s\n" FORMAT=wrong HEALTH_URL=http://127.0.0.1:8080/healthz; fi' \
  'exit 0' >"$malformed_inspector"
chmod 0755 "$malformed_inspector"
malformed_package="$fixture/malformed-package"
./packaging/stage-package.sh --binary "$malformed_inspector" --output "$malformed_package" --source-sha "$trusted_sha" --version "main-${trusted_sha:0:12}" --channel main
malformed_root="$fixture/malformed-root"
"$old_main_package/install.sh" --version "main-${old_main_sha:0:12}" --destdir "$malformed_root" >/dev/null
: >"$fixture/malformed-systemctl.log"
: >"$fixture/malformed-sudo.log"
if PATH="$fake_bin:$PATH" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$malformed_root" \
  SOLODOCK_SMOKE_PACKAGE="$malformed_package" \
  SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
  SOLODOCK_SMOKE_GH_LOG="$fixture/malformed-gh.log" \
  SOLODOCK_SMOKE_SYSTEMCTL_LOG="$fixture/malformed-systemctl.log" \
  SOLODOCK_SMOKE_SUDO_LOG="$fixture/malformed-sudo.log" \
  ./packaging/solodock-update --channel main --backup-dir "$fixture/malformed-backups" >"$fixture/malformed.stdout" 2>"$fixture/malformed.stderr"; then
  printf '%s\n' 'updater accepted malformed packaged-config inspector output' >&2
  exit 1
fi
grep -Fq 'downloaded package rejected the installed packaged configuration' "$fixture/malformed.stderr"
if grep -Eq '(^| )(stop solodock\.service|.*solodock-backup|.*install\.sh)($| )' "$fixture/malformed-sudo.log"; then
  printf '%s\n' 'malformed inspector output reached a mutation command' >&2
  exit 1
fi
[[ ! -e $fixture/malformed-backups ]]

: >"$main_systemctl_log"
PATH="$fake_bin:$PATH" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$main_root" \
  SOLODOCK_SMOKE_PACKAGE="$main_package" \
  SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
  SOLODOCK_SMOKE_GH_LOG="$fixture/main-follow-gh.log" \
  SOLODOCK_SMOKE_SYSTEMCTL_LOG="$main_systemctl_log" \
  ./packaging/solodock-update --backup-dir "$fixture/main-follow-backups" >"$fixture/main-follow.stdout"
grep -Fq 'run list --repo baixiaohang/solodock' "$fixture/main-follow-gh.log"
grep -Fq 'already current at CI run 123' "$fixture/main-follow.stdout"

mapfile -t stable_result < <(run_successful_update stable-success "$old_stable_package" '' 1)
stable_root=${stable_result[0]}
stable_backups=${stable_result[1]}
stable_systemctl_log=${stable_result[2]}
stable_output=${stable_result[3]}
stable_gh_log=${stable_result[4]}
assert_install_coherent "$stable_root"
stable_install=$(managed_install_dir "$stable_root")
grep -qx 'stop solodock.service' "$stable_systemctl_log"
grep -qx 'start solodock.service' "$stable_systemctl_log"
find "$stable_backups" -maxdepth 1 -type f -name '*.tar' -print -quit | grep -q .
grep -Fq 'from Release v0.2.0' "$stable_output"
grep -qx 'CHANNEL=stable' "$stable_install/INSTALL_MANIFEST"
grep -Fqx 'release view --repo baixiaohang/solodock --json tagName,isDraft,isPrerelease --jq [.tagName, .isDraft, .isPrerelease] | @tsv' "$stable_gh_log"
grep -Fq 'release download v0.2.0 --repo baixiaohang/solodock --pattern solodock-v0.2.0-ubuntu-24.04-x86_64.tar.gz --pattern SHA256SUMS --pattern SOURCE_SHA --dir ' "$stable_gh_log"
if grep -Fq 'v0.1.1' "$stable_gh_log" || grep -Fq 'release list' "$stable_gh_log"; then
  printf '%s\n' 'stable updater selected a newly created older release instead of GitHub Latest v0.2.0' >&2
  exit 1
fi

: >"$stable_systemctl_log"
rm -f "$fixture/stable-downgrade-gh.log" "$fixture/stable-downgrade-sudo.log"
if PATH="$fake_bin:$PATH" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$stable_root" \
  SOLODOCK_SMOKE_LATEST_TAG=v0.1.1 \
  SOLODOCK_SMOKE_GH_LOG="$fixture/stable-downgrade-gh.log" \
  SOLODOCK_SMOKE_SUDO_LOG="$fixture/stable-downgrade-sudo.log" \
  SOLODOCK_SMOKE_SYSTEMCTL_LOG="$stable_systemctl_log" \
  ./packaging/solodock-update --backup-dir "$fixture/stable-downgrade-backups" >"$fixture/stable-downgrade.stdout" 2>"$fixture/stable-downgrade.stderr"; then
  printf '%s\n' 'stable updater accepted a lower Latest Release than the installed stable version' >&2
  exit 1
fi
grep -qx 'latest stable Release 0.1.1 is older than installed stable version 0.2.0' "$fixture/stable-downgrade.stderr"
if grep -Fq 'release download' "$fixture/stable-downgrade-gh.log" || grep -Eq 'systemctl stop|solodock-backup|install\.sh' "$fixture/stable-downgrade-sudo.log"; then
  printf '%s\n' 'stable downgrade guard reached download or mutation' >&2
  exit 1
fi

mapfile -t channel_result < <(run_successful_update same-binary-channel "$same_binary_main_package" stable)
channel_root=${channel_result[0]}
channel_systemctl_log=${channel_result[2]}
channel_output=${channel_result[3]}
assert_install_coherent "$channel_root"
channel_install=$(managed_install_dir "$channel_root")
if grep -Eq '^(stop|start) solodock\.service$' "$channel_systemctl_log"; then
  printf '%s\n' 'same-binary main-to-stable transition restarted the service' >&2
  exit 1
fi
[[ ! -e $fixture/same-binary-channel-backups ]]
grep -Fq 'without restarting the unchanged binary' "$channel_output"
grep -qx 'CHANNEL=stable' "$channel_install/INSTALL_MANIFEST"

: >"$channel_systemctl_log"
PATH="$fake_bin:$PATH" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$channel_root" \
  SOLODOCK_SMOKE_RELEASE="$stable_assets" \
  SOLODOCK_SMOKE_LATEST_TAG=v0.2.0 \
  SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
  SOLODOCK_SMOKE_GH_LOG="$fixture/same-binary-follow-gh.log" \
  SOLODOCK_SMOKE_SYSTEMCTL_LOG="$channel_systemctl_log" \
  ./packaging/solodock-update --backup-dir "$fixture/same-binary-follow-backups" >"$fixture/same-binary-follow.stdout"
grep -Fq 'release view --repo baixiaohang/solodock' "$fixture/same-binary-follow-gh.log"
grep -Fq 'already current at Release v0.2.0' "$fixture/same-binary-follow.stdout"

mapfile -t helper_result < <(run_successful_update helper-refresh "$stable_package")
helper_root=${helper_result[0]}
helper_systemctl_log=${helper_result[2]}

package_only="$fixture/package-only"
cp -a "$stable_package" "$package_only"
printf '%s\n' 'Package-only fixture.' >>"$package_only/README.md"
package_only_identity_list="$fixture/package-only-identity-list"
(cd "$package_only" && find . -type f ! -name SHA256SUMS ! -name INSTALL_MANIFEST -print0 | LC_ALL=C sort -z | xargs -0 sha256sum) >"$package_only_identity_list"
package_only_identity=$(sha256sum "$package_only_identity_list" | awk '{print $1}')
sed -i "s/^PACKAGE_IDENTITY=.*/PACKAGE_IDENTITY=$package_only_identity/" "$package_only/INSTALL_MANIFEST"
(cd "$package_only" && find . -type f ! -name SHA256SUMS -print0 | LC_ALL=C sort -z | xargs -0 sha256sum >SHA256SUMS)
"$package_only/verify-package.sh" "$package_only" >/dev/null
package_only_assets="$fixture/package-only-assets"
mkdir -p "$package_only_assets" "$fixture/package-only-archive"
cp -a "$package_only" "$fixture/package-only-archive/solodock-package"
(cd "$fixture/package-only-archive" && tar -czf "$package_only_assets/solodock-v0.2.0-ubuntu-24.04-x86_64.tar.gz" solodock-package)
printf '%s\n' "$trusted_sha" >"$package_only_assets/SOURCE_SHA"
(cd "$package_only_assets" && sha256sum solodock-v0.2.0-ubuntu-24.04-x86_64.tar.gz SOURCE_SHA >SHA256SUMS)

failure_points=(
  stage-solodock
  stage-update
  stage-backup
  stage-restore
  stage-verifier
  stage-unit
  stage-manifest
  after-link-update
  after-link-backup
  after-link-restore
  after-link-unit
  after-link-solodock
)
for update_kind in package-only stopped; do
  for failure_point in "${failure_points[@]}"; do
    failure_name="failure-$update_kind-$failure_point"
    failure_root="$fixture/$failure_name-root"
    failure_log="$fixture/$failure_name-systemctl.log"
    failure_before="$fixture/$failure_name-before"
    failure_after="$fixture/$failure_name-after"
    failure_backups="$fixture/$failure_name-backups"
    if [[ $update_kind == package-only ]]; then
      "$stable_package/install.sh" --version 0.2.0 --destdir "$failure_root" >/dev/null
      failure_assets=$package_only_assets
    else
      "$old_stable_package/install.sh" --version 0.1.0 --destdir "$failure_root" >/dev/null
      failure_assets=$stable_assets
    fi
    capture_install_snapshot "$failure_root" >"$failure_before"
    : >"$failure_log"
    if PATH="$fake_bin:$PATH" \
      SOLODOCK_UPDATE_TEST_MODE=1 \
      SOLODOCK_UPDATE_TEST_ROOT="$failure_root" \
      SOLODOCK_INSTALL_FAIL_AT="$failure_point" \
      SOLODOCK_SMOKE_RELEASE="$failure_assets" \
      SOLODOCK_SMOKE_LATEST_TAG=v0.2.0 \
      SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
      SOLODOCK_SMOKE_SYSTEMCTL_LOG="$failure_log" \
      ./packaging/solodock-update --backup-dir "$failure_backups" >"$fixture/$failure_name.stdout" 2>"$fixture/$failure_name.stderr"; then
      printf 'updater accepted injected %s failure in %s path\n' "$failure_point" "$update_kind" >&2
      exit 1
    fi
    grep -Fq "injected installer failure at $failure_point" "$fixture/$failure_name.stderr"
    assert_install_coherent "$failure_root"
    capture_install_snapshot "$failure_root" >"$failure_after"
    cmp "$failure_before" "$failure_after"
    if [[ $update_kind == package-only ]]; then
      if grep -Eq '^(stop|start) solodock\.service$' "$failure_log"; then
        printf 'package-only %s failure stopped or restarted the service\n' "$failure_point" >&2
        exit 1
      fi
      [[ ! -e $failure_backups ]]
    else
      grep -qx 'stop solodock.service' "$failure_log"
      grep -qx 'start solodock.service' "$failure_log"
      find "$failure_backups" -maxdepth 1 -type f -name '*.tar' -print -quit | grep -q .
    fi
  done
done

rollback_failure_points=(restore-solodock restore-solodock-update restore-solodock.service)
for rollback_failure_point in "${rollback_failure_points[@]}"; do
  rollback_failure_name=${rollback_failure_point//./-}
  rollback_root="$fixture/rollback-$rollback_failure_name-root"
  rollback_log="$fixture/rollback-$rollback_failure_name-systemctl.log"
  rollback_backups="$fixture/rollback-$rollback_failure_name-backups"
  "$old_stable_package/install.sh" --version 0.1.0 --destdir "$rollback_root" >/dev/null
  : >"$rollback_log"
  if PATH="$fake_bin:$PATH" \
    SOLODOCK_UPDATE_TEST_MODE=1 \
    SOLODOCK_UPDATE_TEST_ROOT="$rollback_root" \
    SOLODOCK_INSTALL_FAIL_AT=after-link-solodock \
    SOLODOCK_INSTALL_ROLLBACK_FAIL_AT="$rollback_failure_point" \
    SOLODOCK_SMOKE_RELEASE="$stable_assets" \
    SOLODOCK_SMOKE_LATEST_TAG=v0.2.0 \
    SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
    SOLODOCK_SMOKE_SYSTEMCTL_LOG="$rollback_log" \
    ./packaging/solodock-update --backup-dir "$rollback_backups" >"$fixture/rollback-$rollback_failure_name.stdout" 2>"$fixture/rollback-$rollback_failure_name.stderr"; then
    printf 'updater accepted incomplete installer rollback at %s\n' "$rollback_failure_point" >&2
    exit 1
  else
    rollback_status=$?
  fi
  ((rollback_status == 70))
  grep -Fq "injected installer rollback failure at $rollback_failure_point" "$fixture/rollback-$rollback_failure_name.stderr"
  grep -Fq 'ROLLBACK_INCOMPLETE:' "$fixture/rollback-$rollback_failure_name.stderr"
  grep -Fq 'solodock.service remains stopped' "$fixture/rollback-$rollback_failure_name.stderr"
  grep -qx 'stop solodock.service' "$rollback_log"
  if grep -qx 'start solodock.service' "$rollback_log"; then
    printf 'updater started the service after incomplete rollback at %s\n' "$rollback_failure_point" >&2
    exit 1
  fi
  find "$rollback_backups" -maxdepth 1 -type f -name '*.tar' -print -quit | grep -q .
  find "$rollback_root/usr/local/lib/solodock" -maxdepth 1 -type d -name '.install-transaction.*' -print -quit | grep -q .
  generation_count=$(find "$rollback_root/usr/local/lib/solodock/generations" -mindepth 1 -maxdepth 1 -type d | wc -l)
  ((generation_count == 2))
done

package_rollback_root="$fixture/package-rollback-incomplete-root"
package_rollback_log="$fixture/package-rollback-incomplete-systemctl.log"
"$stable_package/install.sh" --version 0.2.0 --destdir "$package_rollback_root" >/dev/null
: >"$package_rollback_log"
if PATH="$fake_bin:$PATH" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$package_rollback_root" \
  SOLODOCK_INSTALL_FAIL_AT=after-link-solodock \
  SOLODOCK_INSTALL_ROLLBACK_FAIL_AT=restore-solodock-update \
  SOLODOCK_SMOKE_RELEASE="$package_only_assets" \
  SOLODOCK_SMOKE_LATEST_TAG=v0.2.0 \
  SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
  SOLODOCK_SMOKE_SYSTEMCTL_LOG="$package_rollback_log" \
  ./packaging/solodock-update --backup-dir "$fixture/package-rollback-incomplete-backups" >"$fixture/package-rollback-incomplete.stdout" 2>"$fixture/package-rollback-incomplete.stderr"; then
  printf '%s\n' 'package-only updater accepted an incomplete installer rollback' >&2
  exit 1
else
  package_rollback_status=$?
fi
((package_rollback_status == 70))
grep -Fq 'ROLLBACK_INCOMPLETE:' "$fixture/package-rollback-incomplete.stderr"
grep -Fq 'solodock.service was stopped' "$fixture/package-rollback-incomplete.stderr"
grep -qx 'stop solodock.service' "$package_rollback_log"
if grep -qx 'start solodock.service' "$package_rollback_log"; then
  printf '%s\n' 'package-only updater restarted the service after incomplete rollback' >&2
  exit 1
fi

: >"$helper_systemctl_log"
PATH="$fake_bin:$PATH" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$helper_root" \
  SOLODOCK_SMOKE_RELEASE="$package_only_assets" \
  SOLODOCK_SMOKE_LATEST_TAG=v0.2.0 \
  SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
  SOLODOCK_SMOKE_SYSTEMCTL_LOG="$helper_systemctl_log" \
  ./packaging/solodock-update --backup-dir "$fixture/package-only-backups" >"$fixture/package-only.stdout"
assert_install_coherent "$helper_root"
helper_install=$(managed_install_dir "$helper_root")
cmp "$package_only/INSTALL_MANIFEST" "$helper_install/INSTALL_MANIFEST"
if grep -Eq '^(stop|start) solodock\.service$' "$helper_systemctl_log"; then
  printf '%s\n' 'package-identity-only refresh restarted the service' >&2
  exit 1
fi
[[ ! -e $fixture/package-only-backups ]]
grep -Fq 'without restarting the unchanged binary' "$fixture/package-only.stdout"

printf '%s\n' '# stale helper' >"$helper_install/solodock-update"
: >"$helper_systemctl_log"
PATH="$fake_bin:$PATH" \
  SOLODOCK_UPDATE_TEST_MODE=1 \
  SOLODOCK_UPDATE_TEST_ROOT="$helper_root" \
  SOLODOCK_SMOKE_RELEASE="$package_only_assets" \
  SOLODOCK_SMOKE_LATEST_TAG=v0.2.0 \
  SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
  SOLODOCK_SMOKE_SYSTEMCTL_LOG="$helper_systemctl_log" \
  ./packaging/solodock-update --backup-dir "$fixture/helper-refresh-backups" >"$fixture/helper-refresh.stdout"
assert_install_coherent "$helper_root"
helper_install=$(managed_install_dir "$helper_root")
cmp "$package_only/solodock-update" "$helper_install/solodock-update"
if grep -Eq '^(stop|start) solodock\.service$' "$helper_systemctl_log"; then
  printf '%s\n' 'helper-only package refresh restarted the service' >&2
  exit 1
fi
grep -Fq 'without restarting the unchanged binary' "$fixture/helper-refresh.stdout"

assert_install_coherent "$stable_root"

mkdir -m 0700 -p "$fixture/root/var/lib/solodock/apps"
mkdir -m 0700 -p "$fixture/root/var/lib/solodock/secrets"
head -c 32 /dev/zero >"$fixture/root/var/lib/solodock/secrets/idempotency.key"
chmod 0600 "$fixture/root/var/lib/solodock/secrets/idempotency.key"
chmod 0600 "$fixture/root/etc/solodock/config.toml"
restore_identity_bin="$fixture/restore-identity-bin"
mkdir -m 0700 "$restore_identity_bin"
restore_uid=$(id -u)
restore_gid=$(id -g)
if [[ $restore_uid == 0 ]]; then
  # Root-capable CI exercises the production root-unpack -> non-root
  # validator transition instead of validating as root.
  restore_uid=999
  restore_gid=999
fi
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'if [[ $1 == passwd && $2 == solodock ]]; then printf "solodock:x:%s:%s::/nonexistent:/usr/sbin/nologin\n" "$SOLODOCK_TEST_UID" "$SOLODOCK_TEST_GID"; exit 0; fi' \
  'if [[ $1 == group && $2 == "$SOLODOCK_TEST_GID" ]]; then printf "solodock:x:%s:\n" "$SOLODOCK_TEST_GID"; exit 0; fi' \
  'exit 2' >"$restore_identity_bin/getent"
chmod 0755 "$restore_identity_bin/getent"
ln -s /tmp "$fixture/.solodock-backup-predictable.tmp"
./packaging/solodock-backup --root "$fixture/root" --validator "$validator" --output "$fixture/backup.tar" >/dev/null
(cd "$fixture" && sha256sum -c backup.tar.sha256 >/dev/null)
tar -tf "$fixture/backup.tar" | grep -q '^var/lib/solodock/'
[[ $(stat -c '%a' "$fixture/backup.tar") == 600 ]]

custom_backup_root="$fixture/custom-backup-root"
cp -a "$fixture/root" "$custom_backup_root"
sed -i 's#^state_directory = .*#state_directory = "/srv/solodock-state"#' "$custom_backup_root/etc/solodock/config.toml"
if ./packaging/solodock-backup --root "$custom_backup_root" --validator "$validator" --output "$fixture/custom-backup.tar" >"$fixture/custom-backup.stdout" 2>"$fixture/custom-backup.stderr"; then
  printf '%s\n' 'backup accepted a custom packaged state layout' >&2
  exit 1
fi
grep -Fq 'backup configuration is not a valid packaged layout' "$fixture/custom-backup.stderr"
[[ ! -e $fixture/custom-backup.tar && ! -e $fixture/custom-backup.tar.sha256 ]]
if find "$fixture" -maxdepth 1 -name '.solodock-backup.*' -print -quit | grep -q .; then
  printf '%s\n' 'custom-layout backup created a temporary output' >&2
  exit 1
fi

custom_restore_tree="$fixture/custom-restore-tree"
mkdir -m 0700 "$custom_restore_tree"
tar -C "$custom_restore_tree" -xf "$fixture/backup.tar"
sed -i 's#^state_directory = .*#state_directory = "/srv/solodock-state"#' "$custom_restore_tree/etc/solodock/config.toml"
tar --format=pax -C "$custom_restore_tree" -cf "$fixture/custom-restore.tar" var/lib/solodock etc/solodock/config.toml
sha256sum "$fixture/custom-restore.tar" >"$fixture/custom-restore.tar.sha256"
if PATH="$restore_identity_bin:$PATH" SOLODOCK_TEST_UID="$restore_uid" SOLODOCK_TEST_GID="$restore_gid" \
  ./packaging/solodock-restore --archive "$fixture/custom-restore.tar" --checksum "$fixture/custom-restore.tar.sha256" --output "$fixture/custom-restored" --validator "$validator" >"$fixture/custom-restore.stdout" 2>"$fixture/custom-restore.stderr"; then
  printf '%s\n' 'restore accepted a custom packaged state layout' >&2
  exit 1
fi
grep -Fq 'restored configuration is not a valid packaged layout' "$fixture/custom-restore.stderr"
[[ ! -e $fixture/custom-restored ]]

race_bin="$fixture/backup-race-bin"
mkdir -m 0700 "$race_bin"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  ': >"$SOLODOCK_TEST_TAR_REACHED"' \
  'while [[ ! -e $SOLODOCK_TEST_TAR_CONTINUE ]]; do sleep 0.01; done' \
  'exec /usr/bin/tar "$@"' >"$race_bin/tar"
chmod 0755 "$race_bin/tar"
SOLODOCK_TEST_TAR_REACHED="$fixture/tar-reached" \
SOLODOCK_TEST_TAR_CONTINUE="$fixture/tar-continue" \
PATH="$race_bin:$PATH" \
  ./packaging/solodock-backup --root "$fixture/root" --validator "$validator" --output "$fixture/raced-backup.tar" >"$fixture/raced-backup.stdout" 2>"$fixture/raced-backup.stderr" &
race_pid=$!
for _ in {1..500}; do
  [[ -e $fixture/tar-reached ]] && break
  kill -0 "$race_pid" 2>/dev/null || { printf '%s\n' 'backup race fixture exited before tar' >&2; exit 1; }
  sleep 0.01
done
[[ -e $fixture/tar-reached ]]
printf '%s\n' 'attacker-checksum-canary' >"$fixture/raced-backup.tar.sha256"
: >"$fixture/tar-continue"
if wait "$race_pid"; then
  printf '%s\n' 'backup replaced a checksum that appeared concurrently' >&2
  exit 1
fi
[[ ! -e $fixture/raced-backup.tar ]]
grep -Fxq 'attacker-checksum-canary' "$fixture/raced-backup.tar.sha256"

mkdir -m 0700 "$fixture/replaced-backup-parent"
rm -f -- "$fixture/tar-reached" "$fixture/tar-continue"
SOLODOCK_TEST_TAR_REACHED="$fixture/tar-reached" \
SOLODOCK_TEST_TAR_CONTINUE="$fixture/tar-continue" \
PATH="$race_bin:$PATH" \
  ./packaging/solodock-backup --root "$fixture/root" --validator "$validator" --output "$fixture/replaced-backup-parent/backup.tar" >"$fixture/replaced-backup.stdout" 2>"$fixture/replaced-backup.stderr" &
race_pid=$!
for _ in {1..500}; do
  [[ -e $fixture/tar-reached ]] && break
  kill -0 "$race_pid" 2>/dev/null || { printf '%s\n' 'backup parent race fixture exited before tar' >&2; exit 1; }
  sleep 0.01
done
[[ -e $fixture/tar-reached ]]
mv -- "$fixture/replaced-backup-parent" "$fixture/displaced-backup-parent"
mkdir -m 0700 "$fixture/replaced-backup-parent"
: >"$fixture/tar-continue"
if wait "$race_pid"; then
  printf '%s\n' 'backup accepted a replaced output parent' >&2
  exit 1
fi
[[ ! -e $fixture/replaced-backup-parent/backup.tar ]]
if find "$fixture/displaced-backup-parent" -maxdepth 1 -name '.solodock-backup.*' -print -quit | grep -q .; then
  printf '%s\n' 'backup left a secret temporary file after parent replacement' >&2
  exit 1
fi

mkdir -m 0777 "$fixture/unsafe-restore-parent"
if PATH="$restore_identity_bin:$PATH" SOLODOCK_TEST_UID="$restore_uid" SOLODOCK_TEST_GID="$restore_gid" \
  ./packaging/solodock-restore --archive "$fixture/backup.tar" --checksum "$fixture/backup.tar.sha256" --output "$fixture/unsafe-restore-parent/restored" --validator "$validator" >"$fixture/unsafe-restore-parent.stdout" 2>"$fixture/unsafe-restore-parent.stderr"; then
  printf '%s\n' 'restore accepted a group/other-writable output parent' >&2
  exit 1
fi
[[ ! -e $fixture/unsafe-restore-parent/restored ]]

missing_identity_bin="$fixture/missing-identity-bin"
mkdir -m 0700 "$missing_identity_bin"
printf '%s\n' '#!/usr/bin/env bash' 'exit 2' >"$missing_identity_bin/getent"
chmod 0755 "$missing_identity_bin/getent"
if PATH="$missing_identity_bin:$PATH" \
  ./packaging/solodock-restore --archive "$fixture/backup.tar" --checksum "$fixture/backup.tar.sha256" --output "$fixture/missing-account-restore" --validator "$validator" >"$fixture/missing-account.stdout" 2>"$fixture/missing-account.stderr"; then
  printf '%s\n' 'restore accepted a missing solodock service account' >&2
  exit 1
fi
[[ ! -e $fixture/missing-account-restore ]]

if PATH="$restore_identity_bin:$PATH" SOLODOCK_TEST_UID=0 SOLODOCK_TEST_GID="$restore_gid" \
  ./packaging/solodock-restore --archive "$fixture/backup.tar" --checksum "$fixture/backup.tar.sha256" --output "$fixture/root-account-restore" --validator "$validator" >"$fixture/root-account.stdout" 2>"$fixture/root-account.stderr"; then
  printf '%s\n' 'restore accepted root as the solodock service identity' >&2
  exit 1
fi
[[ ! -e $fixture/root-account-restore ]]

unexpected_uid=1001
unexpected_gid=1001
if [[ $unexpected_uid == "$(id -u)" && $unexpected_gid == "$(id -g)" ]]; then
  unexpected_uid=1002
  unexpected_gid=1002
fi
if PATH="$restore_identity_bin:$PATH" SOLODOCK_TEST_UID="$unexpected_uid" SOLODOCK_TEST_GID="$unexpected_gid" \
  ./packaging/solodock-restore --archive "$fixture/backup.tar" --checksum "$fixture/backup.tar.sha256" --output "$fixture/non-system-account-restore" --validator "$validator" >"$fixture/non-system-account.stdout" 2>"$fixture/non-system-account.stderr"; then
  printf '%s\n' 'restore accepted an unexpected non-system service identity' >&2
  exit 1
fi
[[ ! -e $fixture/non-system-account-restore ]]

malicious_tree="$fixture/config-link-tree"
mkdir -m 0700 "$malicious_tree"
tar -C "$malicious_tree" -xf "$fixture/backup.tar"
printf '%s\n' 'external-config-canary' >"$fixture/config-canary"
chmod 0644 "$fixture/config-canary"
rm -- "$malicious_tree/etc/solodock/config.toml"
ln -s "$fixture/config-canary" "$malicious_tree/etc/solodock/config.toml"
tar --format=pax -C "$malicious_tree" -cf "$fixture/config-link.tar" var/lib/solodock etc/solodock/config.toml
sha256sum "$fixture/config-link.tar" >"$fixture/config-link.tar.sha256"
if PATH="$restore_identity_bin:$PATH" SOLODOCK_TEST_UID="$restore_uid" SOLODOCK_TEST_GID="$restore_gid" \
  ./packaging/solodock-restore --archive "$fixture/config-link.tar" --checksum "$fixture/config-link.tar.sha256" --output "$fixture/config-link-restored" --validator "$validator" >"$fixture/config-link.stdout" 2>"$fixture/config-link.stderr"; then
  printf '%s\n' 'restore accepted a symlink config' >&2
  exit 1
fi
grep -Fxq 'external-config-canary' "$fixture/config-canary"
[[ $(stat -c '%a' "$fixture/config-canary") == 644 ]]
[[ ! -e $fixture/config-link-restored ]]

restore_race_bin="$fixture/restore-race-bin"
mkdir -m 0700 "$restore_race_bin"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'for argument in "$@"; do' \
  '  if [[ $argument == -xf ]]; then' \
  '    : >"$SOLODOCK_TEST_TAR_REACHED"' \
  '    while [[ ! -e $SOLODOCK_TEST_TAR_CONTINUE ]]; do sleep 0.01; done' \
  '    break' \
  '  fi' \
  'done' \
  'exec /usr/bin/tar "$@"' >"$restore_race_bin/tar"
chmod 0755 "$restore_race_bin/tar"
ln -s "$restore_identity_bin/getent" "$restore_race_bin/getent"
mkdir -m 0700 "$fixture/replaced-restore-parent"
rm -f -- "$fixture/tar-reached" "$fixture/tar-continue"
PATH="$restore_race_bin:$PATH" SOLODOCK_TEST_UID="$restore_uid" SOLODOCK_TEST_GID="$restore_gid" \
SOLODOCK_TEST_TAR_REACHED="$fixture/tar-reached" SOLODOCK_TEST_TAR_CONTINUE="$fixture/tar-continue" \
  ./packaging/solodock-restore --archive "$fixture/backup.tar" --checksum "$fixture/backup.tar.sha256" --output "$fixture/replaced-restore-parent/restored" --validator "$validator" >"$fixture/replaced-restore.stdout" 2>"$fixture/replaced-restore.stderr" &
race_pid=$!
for _ in {1..500}; do
  [[ -e $fixture/tar-reached ]] && break
  kill -0 "$race_pid" 2>/dev/null || { printf '%s\n' 'restore parent race fixture exited before extraction' >&2; exit 1; }
  sleep 0.01
done
[[ -e $fixture/tar-reached ]]
mv -- "$fixture/replaced-restore-parent" "$fixture/displaced-restore-parent"
mkdir -m 0700 "$fixture/replaced-restore-parent"
: >"$fixture/tar-continue"
if wait "$race_pid"; then
  printf '%s\n' 'restore accepted a replaced output parent' >&2
  exit 1
fi
[[ ! -e $fixture/replaced-restore-parent/restored ]]
if find "$fixture/displaced-restore-parent" -maxdepth 1 -name '.solodock-restore.*' -print -quit | grep -q .; then
  printf '%s\n' 'restore left a staging directory after parent replacement' >&2
  exit 1
fi

publication_validator="$fixture/publication-validator"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'if [[ ${1-} == inspect-packaged-config ]]; then exec "$SOLODOCK_TEST_REAL_VALIDATOR" "$@"; fi' \
  '"$SOLODOCK_TEST_REAL_VALIDATOR" "$@"' \
  ': >"$SOLODOCK_TEST_VALIDATOR_REACHED"' \
  'while [[ ! -e $SOLODOCK_TEST_VALIDATOR_CONTINUE ]]; do sleep 0.01; done' >"$publication_validator"
chmod 0755 "$publication_validator"
SOLODOCK_TEST_REAL_VALIDATOR="$(realpath -e -- "$validator")" SOLODOCK_TEST_VALIDATOR_REACHED="$fixture/validator-reached" SOLODOCK_TEST_VALIDATOR_CONTINUE="$fixture/validator-continue" \
PATH="$restore_identity_bin:$PATH" SOLODOCK_TEST_UID="$restore_uid" SOLODOCK_TEST_GID="$restore_gid" \
  ./packaging/solodock-restore --archive "$fixture/backup.tar" --checksum "$fixture/backup.tar.sha256" --output "$fixture/concurrent-restore" --validator "$publication_validator" >"$fixture/concurrent-restore.stdout" 2>"$fixture/concurrent-restore.stderr" &
race_pid=$!
for _ in {1..500}; do
  [[ -e $fixture/validator-reached ]] && break
  kill -0 "$race_pid" 2>/dev/null || { printf '%s\n' 'restore publication fixture exited before validation' >&2; exit 1; }
  sleep 0.01
done
[[ -e $fixture/validator-reached ]]
mkdir -m 0700 "$fixture/concurrent-restore"
printf '%s\n' 'concurrent-target-canary' >"$fixture/concurrent-restore/canary"
: >"$fixture/validator-continue"
if wait "$race_pid"; then
  printf '%s\n' 'restore replaced a concurrently created target' >&2
  exit 1
fi
grep -Fxq 'concurrent-target-canary' "$fixture/concurrent-restore/canary"

PATH="$restore_identity_bin:$PATH" SOLODOCK_TEST_UID="$restore_uid" SOLODOCK_TEST_GID="$restore_gid" \
  ./packaging/solodock-restore --archive "$fixture/backup.tar" --checksum "$fixture/backup.tar.sha256" --output "$fixture/restored" --validator "$validator" >/dev/null
cmp "$fixture/root/etc/solodock/config.toml" "$fixture/restored/etc/solodock/config.toml"
PATH="$restore_identity_bin:$PATH" SOLODOCK_TEST_UID="$restore_uid" SOLODOCK_TEST_GID="$restore_gid" \
  "$package_root_install/solodock-restore" \
  --archive "$fixture/backup.tar" \
  --checksum "$fixture/backup.tar.sha256" \
  --output "$fixture/restored-with-version-validator" >/dev/null
cmp "$fixture/root/etc/solodock/config.toml" "$fixture/restored-with-version-validator/etc/solodock/config.toml"

mkdir -m 0777 "$fixture/unsafe-backup-parent"
if ./packaging/solodock-backup --root "$fixture/root" --validator "$validator" --output "$fixture/unsafe-backup-parent/backup.tar" >"$fixture/unsafe-parent.stdout" 2>"$fixture/unsafe-parent.stderr"; then
  printf '%s\n' 'backup accepted a group/other-writable output parent' >&2
  exit 1
fi
[[ ! -e $fixture/unsafe-backup-parent/backup.tar ]]

ln -s /tmp "$fixture/root/var/lib/solodock/unsafe-link"
if ./packaging/solodock-backup --root "$fixture/root" --validator "$validator" --output "$fixture/unsafe.tar" >"$fixture/unsafe.stdout" 2>"$fixture/unsafe.stderr"; then
  printf '%s\n' 'backup accepted a symlink inside state' >&2
  exit 1
fi
[[ ! -e $fixture/unsafe.tar && ! -s $fixture/unsafe.stdout ]]
rm -- "$fixture/root/var/lib/solodock/unsafe-link"

printf '%s\n' 'not a tar archive' >"$fixture/incomplete.tar"
sha256sum "$fixture/incomplete.tar" >"$fixture/incomplete.tar.sha256"
if ./packaging/solodock-restore --archive "$fixture/incomplete.tar" --checksum "$fixture/incomplete.tar.sha256" --output "$fixture/partial" --validator "$validator" >"$fixture/partial.stdout" 2>"$fixture/partial.stderr"; then
  printf '%s\n' 'restore accepted an invalid archive' >&2
  exit 1
fi
[[ ! -e $fixture/partial && ! -s $fixture/partial.stdout ]]
