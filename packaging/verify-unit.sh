#!/usr/bin/env bash
set -euo pipefail
fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT
fake="$fixture/solodock"
printf '#!/bin/sh\nexit 0\n' >"$fake"
chmod 0755 "$fake"
./packaging/install.sh --version verify --binary "$fake" --destdir "$fixture/root" >/dev/null
unit_dir="$fixture/root/etc/systemd/system"
for target in sysinit.target basic.target shutdown.target network-online.target multi-user.target; do
  printf '[Unit]\nDescription=fixture %s\n' "$target" >"$unit_dir/$target"
done
printf '[Unit]\nDescription=fixture docker\n[Service]\nType=oneshot\nExecStart=/bin/true\nRemainAfterExit=yes\n' >"$unit_dir/docker.service"
mkdir -p "$fixture/root/bin"
cp /bin/true "$fixture/root/bin/true"
systemd-analyze verify --root="$fixture/root" /etc/systemd/system/solodock.service
