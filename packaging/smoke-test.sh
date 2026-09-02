#!/usr/bin/env bash
set -euo pipefail
fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT
fake="$fixture/solodock"
printf '#!/bin/sh\nexit 0\n' >"$fake"
chmod 0755 "$fake"
./packaging/install.sh --version 0.1.0-test --binary "$fake" --destdir "$fixture/root" >/dev/null
[[ $(readlink "$fixture/root/usr/local/bin/solodock-update") == /usr/local/lib/solodock/0.1.0-test/solodock-update ]]
printf '# retained\n' >>"$fixture/root/etc/solodock/config.toml"
./packaging/install.sh --version 0.1.1-test --binary "$fake" --destdir "$fixture/root" >/dev/null
grep -q retained "$fixture/root/etc/solodock/config.toml"
unknown="$fixture/unknown-root"
mkdir -p "$unknown/usr/local/bin"
printf '%s\n' 'do not replace' >"$unknown/usr/local/bin/solodock"
if ./packaging/install.sh --version 0.1.2-test --binary "$fake" --destdir "$unknown" >"$fixture/install.stdout" 2>"$fixture/install.stderr"; then
  printf '%s\n' 'installer accepted an unknown binary target' >&2
  exit 1
fi
[[ ! -e $unknown/usr/local/lib/solodock/0.1.2-test && ! -e $unknown/etc/solodock && ! -s $fixture/install.stdout ]]

package="$fixture/package"
mkdir -- "$package"
install -m 0755 "$fake" "$package/solodock"
install -m 0755 packaging/install.sh "$package/install.sh"
install -m 0755 packaging/solodock-update "$package/solodock-update"
install -m 0644 packaging/systemd/solodock.service "$package/solodock.service"
install -m 0644 packaging/solodock.toml.example "$package/solodock.toml.example"
(cd "$fixture" && "$package/install.sh" --version 0.1.3-test --destdir "$fixture/package-root" >/dev/null)
[[ -x $fixture/package-root/usr/local/lib/solodock/0.1.3-test/solodock ]]
[[ $(readlink "$fixture/package-root/usr/local/bin/solodock") == /usr/local/lib/solodock/0.1.3-test/solodock ]]
[[ $(readlink "$fixture/package-root/usr/local/bin/solodock-update") == /usr/local/lib/solodock/0.1.3-test/solodock-update ]]

./packaging/solodock-update --help >/dev/null
if ./packaging/solodock-update --health-url 'http://0.0.0.0:8080/healthz' >"$fixture/update.stdout" 2>"$fixture/update.stderr"; then
  printf '%s\n' 'updater accepted a non-loopback health URL' >&2
  exit 1
fi
[[ ! -s $fixture/update.stdout ]]

update_package="$fixture/update-package"
fake_bin="$fixture/fake-bin"
mkdir -p "$update_package" "$fake_bin"
for file in solodock install.sh solodock-backup solodock-update; do
  install -m 0755 "$fake" "$update_package/$file"
done
install -m 0644 packaging/systemd/solodock.service "$update_package/solodock.service"
install -m 0644 packaging/solodock.toml.example "$update_package/solodock.toml.example"
printf '%040d\n' 1 >"$update_package/SOURCE_SHA"
(cd "$update_package" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum >SHA256SUMS)
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
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
  'exit 1' >"$fake_bin/gh"
printf '%s\n' '#!/bin/sh' 'exit 0' >"$fake_bin/sudo"
chmod 0755 "$fake_bin/gh" "$fake_bin/sudo"
attestation_args="$fixture/attestation.args"
if PATH="$fake_bin:$PATH" \
  SOLODOCK_SMOKE_ATTESTATION_SUPPORTED=no \
  ./packaging/solodock-update >"$fixture/attestation-support.stdout" 2>"$fixture/attestation-support.stderr"; then
  printf '%s\n' 'updater accepted a GitHub CLI without attestation support' >&2
  exit 1
fi
grep -qx 'GitHub CLI does not support artifact attestation verification' "$fixture/attestation-support.stderr"

if PATH="$fake_bin:$PATH" \
  SOLODOCK_SMOKE_PACKAGE="$update_package" \
  SOLODOCK_SMOKE_ATTESTATION_ARGS="$attestation_args" \
  SOLODOCK_SMOKE_ATTESTATION_RESULT=failure \
  ./packaging/solodock-update >"$fixture/attestation.stdout" 2>"$fixture/attestation.stderr"; then
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
  ./packaging/solodock-update >"$fixture/source-sha.stdout" 2>"$fixture/source-sha.stderr"; then
  printf '%s\n' 'updater accepted an artifact from a different source commit' >&2
  exit 1
fi
grep -qx 'artifact source SHA does not match its workflow run' "$fixture/source-sha.stderr"

mkdir -m 0700 -p "$fixture/root/var/lib/solodock/apps"
mkdir -m 0700 -p "$fixture/root/var/lib/solodock/secrets"
head -c 32 /dev/zero >"$fixture/root/var/lib/solodock/secrets/idempotency.key"
chmod 0600 "$fixture/root/var/lib/solodock/secrets/idempotency.key"
chmod 0600 "$fixture/root/etc/solodock/config.toml"
./packaging/solodock-backup --root "$fixture/root" --output "$fixture/backup.tar" >/dev/null
(cd "$fixture" && sha256sum -c backup.tar.sha256 >/dev/null)
tar -tf "$fixture/backup.tar" | grep -q '^var/lib/solodock/'
[[ $(stat -c '%a' "$fixture/backup.tar") == 600 ]]
./packaging/solodock-restore --archive "$fixture/backup.tar" --checksum "$fixture/backup.tar.sha256" --output "$fixture/restored" --validator target/release/solodock >/dev/null
cmp "$fixture/root/etc/solodock/config.toml" "$fixture/restored/etc/solodock/config.toml"

ln -s /tmp "$fixture/root/var/lib/solodock/unsafe-link"
if ./packaging/solodock-backup --root "$fixture/root" --output "$fixture/unsafe.tar" >"$fixture/unsafe.stdout" 2>"$fixture/unsafe.stderr"; then
  printf '%s\n' 'backup accepted a symlink inside state' >&2
  exit 1
fi
[[ ! -e $fixture/unsafe.tar && ! -s $fixture/unsafe.stdout ]]
rm -- "$fixture/root/var/lib/solodock/unsafe-link"

printf '%s\n' 'not a tar archive' >"$fixture/incomplete.tar"
sha256sum "$fixture/incomplete.tar" >"$fixture/incomplete.tar.sha256"
if ./packaging/solodock-restore --archive "$fixture/incomplete.tar" --checksum "$fixture/incomplete.tar.sha256" --output "$fixture/partial" --validator target/release/solodock >"$fixture/partial.stdout" 2>"$fixture/partial.stderr"; then
  printf '%s\n' 'restore accepted an invalid archive' >&2
  exit 1
fi
[[ ! -e $fixture/partial && ! -s $fixture/partial.stdout ]]
