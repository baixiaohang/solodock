#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' 'usage: install.sh --version VERSION [--binary PATH] [--destdir PATH] [--enable-now]'
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
version=''
binary=''
destdir=''
enable_now=0
while (($#)); do
  case "$1" in
    --version) version=${2-}; shift 2 ;;
    --binary) binary=${2-}; shift 2 ;;
    --destdir) destdir=${2-}; shift 2 ;;
    --enable-now) enable_now=1; shift ;;
    *) usage >&2; exit 2 ;;
  esac
done

if [[ -z $binary ]]; then
  if [[ -f $script_dir/solodock ]]; then
    binary="$script_dir/solodock"
  else
    binary='target/release/solodock'
  fi
fi
unit_source="$script_dir/solodock.service"
if [[ ! -f $unit_source ]]; then
  unit_source="$script_dir/systemd/solodock.service"
fi
config_source="$script_dir/solodock.toml.example"
update_source="$script_dir/solodock-update"

[[ $version =~ ^[0-9A-Za-z][0-9A-Za-z._-]{0,63}$ ]] || { printf '%s\n' 'invalid version' >&2; exit 2; }
[[ -f $binary && ! -L $binary ]] || { printf '%s\n' 'binary must be a regular file' >&2; exit 2; }
[[ -f $unit_source && ! -L $unit_source && -f $config_source && ! -L $config_source && -f $update_source && ! -L $update_source ]] || { printf '%s\n' 'package assets are missing or unsafe' >&2; exit 2; }
if [[ -n $destdir ]]; then
  [[ $destdir = /* && $destdir != / && ! -L $destdir ]] || { printf '%s\n' 'unsafe DESTDIR' >&2; exit 2; }
  mkdir -p -- "$destdir"
else
  [[ ${EUID:-$(id -u)} -eq 0 ]] || { printf '%s\n' 'production installation requires root' >&2; exit 1; }
  grep -qx 'ID=ubuntu' /etc/os-release
  grep -Eq '^VERSION_ID="?24\.04"?$' /etc/os-release
  command -v systemctl >/dev/null
  command -v docker >/dev/null
  /usr/bin/docker compose version --short >/dev/null
  getent group docker >/dev/null
  [[ -S /var/run/docker.sock && $(stat -c '%G' /var/run/docker.sock) == docker ]] || { printf '%s\n' 'Docker socket/group is unavailable' >&2; exit 1; }
fi

root=${destdir%/}
lib="$root/usr/local/lib/solodock/$version"
bin_dir="$root/usr/local/bin"
etc_dir="$root/etc/solodock"
state_dir="$root/var/lib/solodock"
unit_dir="$root/etc/systemd/system"
target="$bin_dir/solodock"
update_target="$bin_dir/solodock-update"
validate_managed_target() {
  local candidate=$1
  local name=$2
  local current
  if [[ -e $candidate || -L $candidate ]]; then
    if [[ ! -L $candidate ]]; then
      printf 'refusing to replace unknown non-symlink %s target\n' "$name" >&2
      exit 1
    fi
    current=$(readlink -- "$candidate")
    [[ $current == /usr/local/lib/solodock/*/$name ]] || { printf 'refusing unknown %s symlink target\n' "$name" >&2; exit 1; }
  fi
}
validate_managed_target "$target" solodock
validate_managed_target "$update_target" solodock-update
if [[ -z $destdir ]]; then
  if account=$(getent passwd solodock); then
    IFS=: read -r account_name _ account_uid account_gid _ account_home account_shell <<<"$account"
    primary_group=$(getent group "$account_gid" | cut -d: -f1)
    if [[ $account_name != solodock || ! $account_uid =~ ^[0-9]+$ || $account_uid -eq 0 || $account_uid -ge 1000 || $primary_group != solodock || $account_home != /nonexistent || $account_shell != /usr/sbin/nologin ]]; then
      printf '%s\n' 'existing solodock account is not the dedicated system account' >&2
      exit 1
    fi
  else
    getent group solodock >/dev/null && { printf '%s\n' 'refusing an unrelated pre-existing solodock group' >&2; exit 1; }
    useradd --system --home-dir /nonexistent --no-create-home --shell /usr/sbin/nologin --user-group solodock
  fi
  usermod -a -G docker solodock
fi
install -d -m 0755 -- "$lib" "$bin_dir" "$unit_dir"
install -d -m 0700 -- "$etc_dir" "$state_dir"
install -m 0755 -- "$binary" "$lib/solodock"
install -m 0755 -- "$update_source" "$lib/solodock-update"
install -m 0644 -- "$unit_source" "$unit_dir/solodock.service"
if [[ ! -e $etc_dir/config.toml ]]; then
  install -m 0600 -- "$config_source" "$etc_dir/config.toml"
elif [[ -L $etc_dir/config.toml || ! -f $etc_dir/config.toml ]]; then
  printf '%s\n' 'refusing unsafe existing config target' >&2
  exit 1
fi

tmp="$bin_dir/.solodock-link-$version-$$"
update_tmp="$bin_dir/.solodock-update-link-$version-$$"
trap 'rm -f -- "$tmp" "$update_tmp"' EXIT
ln -s -- "/usr/local/lib/solodock/$version/solodock" "$tmp"
ln -s -- "/usr/local/lib/solodock/$version/solodock-update" "$update_tmp"
mv -Tf -- "$tmp" "$target"
mv -Tf -- "$update_tmp" "$update_target"
trap - EXIT

if [[ -z $destdir ]]; then
  chown solodock:solodock /etc/solodock /etc/solodock/config.toml /var/lib/solodock
  systemctl daemon-reload
  if ((enable_now)); then systemctl enable --now solodock.service; fi
elif ((enable_now)); then
  printf '%s\n' '--enable-now is unavailable with --destdir' >&2
  exit 2
fi

printf '%s\n' 'Keep a verified offline backup before upgrading; database migrations are forward-only.'
printf '%s\n' "SoloDock $version installed; existing config and state were preserved."
