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
