#!/usr/bin/env bash
set -euo pipefail
fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT
fake="$fixture/solodock"
printf '#!/bin/sh\nexit 0\n' >"$fake"
chmod 0755 "$fake"
package="$fixture/package"
./packaging/stage-package.sh \
  --binary "$fake" \
  --output "$package" \
  --source-sha "$(printf '%040d' 1)" \
  --version 0.0.0 \
  --channel stable
"$package/install.sh" --version 0.0.0 --destdir "$fixture/root" >/dev/null
unit_dir="$fixture/root/etc/systemd/system"
for target in sysinit.target basic.target shutdown.target network-online.target multi-user.target; do
  printf '[Unit]\nDescription=fixture %s\n' "$target" >"$unit_dir/$target"
done
printf '[Unit]\nDescription=fixture docker\n[Service]\nType=oneshot\nExecStart=/bin/true\nRemainAfterExit=yes\n' >"$unit_dir/docker.service"
mkdir -p "$fixture/root/bin"
cp /bin/true "$fixture/root/bin/true"
unit_target=$(readlink -- "$unit_dir/solodock.service")
[[ $unit_target =~ ^/usr/local/lib/solodock/generations/.+/solodock\.service$ ]]
cmp "$package/solodock.service" "$fixture/root$unit_target"
# systemd-analyze --root does not follow an absolute unit symlink through its
# fixture root, so verify the already-compared unit contents at the canonical path.
rm -- "$unit_dir/solodock.service"
cp -- "$package/solodock.service" "$unit_dir/solodock.service"
systemd-analyze verify --root="$fixture/root" /etc/systemd/system/solodock.service
