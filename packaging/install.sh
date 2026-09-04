#!/usr/bin/env bash
set -Eeuo pipefail

usage() {
  printf '%s\n' 'usage: install.sh --version VERSION [--binary PATH] [--destdir PATH] [--enable-now]'
}

inspect_packaged_config() {
  local inspector=$1 config=$2 output health authority management
  local -a fields
  output=$("$inspector" inspect-packaged-config "$config") || return 1
  mapfile -t fields <<<"$output"
  [[ ${#fields[@]} == 4 ]] || return 1
  [[ ${fields[0]} == 'FORMAT=solodock-packaged-config-v1' ]] || return 1
  [[ ${fields[1]} == HEALTH_URL=* ]] || return 1
  health=${fields[1]#HEALTH_URL=}
  [[ -n $health && ${#health} -le 320 && $health != *[$' \t\r\n']* ]] || return 1
  authority=${health#http://}
  authority=${authority%/healthz}
  [[ -n $authority && $health == "http://$authority/healthz" && $authority != *['/?#@\']* ]] || return 1
  [[ ${fields[2]} == "LOCAL_AUTHORITY=$authority" ]] || return 1
  [[ ${fields[3]} == MANAGEMENT_AUTHORITY=* ]] || return 1
  management=${fields[3]#MANAGEMENT_AUTHORITY=}
  [[ -n $management && ${#management} -le 320 && $management != *[$' \t\r\n']* && $management != *['/?#@\']* ]] || return 1
}

validate_docker_socket() {
  local socket=$1
  if [[ -e $socket || -L $socket ]]; then
    [[ -S $socket && ! -L $socket && $(stat -c '%G' "$socket") == docker ]] || return 1
  fi
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
  binary="$script_dir/solodock"
fi
unit_source="$script_dir/solodock.service"
if [[ ! -f $unit_source ]]; then
  unit_source="$script_dir/systemd/solodock.service"
fi
config_source="$script_dir/solodock.toml.example"
update_source="$script_dir/solodock-update"
backup_source="$script_dir/solodock-backup"
restore_source="$script_dir/solodock-restore"
verify_source="$script_dir/verify-package.sh"
manifest_source="$script_dir/INSTALL_MANIFEST"

[[ $version =~ ^[0-9A-Za-z][0-9A-Za-z._-]{0,63}$ ]] || { printf '%s\n' 'invalid version' >&2; exit 2; }
[[ -f $binary && ! -L $binary ]] || { printf '%s\n' 'binary must be a regular file' >&2; exit 2; }
for asset in "$unit_source" "$config_source" "$update_source" "$backup_source" "$restore_source" "$verify_source" "$manifest_source"; do
  [[ -f $asset && ! -L $asset ]] || { printf '%s\n' 'package assets are missing or unsafe' >&2; exit 2; }
done
[[ $(realpath -e -- "$binary") == $(realpath -e -- "$script_dir/solodock") ]] || { printf '%s\n' 'binary must be the verified package binary' >&2; exit 2; }
package_data=$("$verify_source" "$script_dir") || exit 1
IFS=$'\t' read -r package_channel package_version package_source_sha package_identity <<<"$package_data"
[[ $package_version == "$version" ]] || { printf '%s\n' 'requested version does not match the verified install manifest' >&2; exit 2; }
if [[ -n ${SOLODOCK_INSTALL_FAIL_AT:-}${SOLODOCK_INSTALL_ROLLBACK_FAIL_AT:-} && -z $destdir ]]; then
  printf '%s\n' 'install failure injection is available only with --destdir' >&2
  exit 2
fi
if [[ -n $destdir ]]; then
  [[ $destdir = /* && $destdir != / && ! -L $destdir ]] || { printf '%s\n' 'unsafe DESTDIR' >&2; exit 2; }
  ((enable_now == 0)) || { printf '%s\n' '--enable-now is unavailable with --destdir' >&2; exit 2; }
else
  [[ ${EUID:-$(id -u)} -eq 0 ]] || { printf '%s\n' 'production installation requires root' >&2; exit 1; }
  grep -qx 'ID=ubuntu' /etc/os-release
  grep -Eq '^VERSION_ID="?24\.04"?$' /etc/os-release
  command -v systemctl >/dev/null
  command -v docker >/dev/null
  /usr/bin/docker compose version --short >/dev/null
  getent group docker >/dev/null
fi

root=${destdir%/}
config_target="$root/etc/solodock/config.toml"
config_to_inspect=$config_source
if [[ -e $config_target || -L $config_target ]]; then
  [[ -f $config_target && ! -L $config_target ]] || { printf '%s\n' 'refusing unsafe existing config target' >&2; exit 1; }
  config_to_inspect=$config_target
fi
inspect_packaged_config "$binary" "$config_to_inspect" || { printf '%s\n' 'packaged configuration preflight failed' >&2; exit 1; }
docker_socket=/var/run/docker.sock
[[ -z $destdir ]] || docker_socket="$root/var/run/docker.sock"
validate_docker_socket "$docker_socket" || { printf '%s\n' 'Docker socket has an unsafe type or group' >&2; exit 1; }
[[ -z $destdir || -d $destdir ]] || mkdir -p -- "$destdir"
managed_root="$root/usr/local/lib/solodock"
generations="$managed_root/generations"
bin_dir="$root/usr/local/bin"
etc_dir="$root/etc/solodock"
state_dir="$root/var/lib/solodock"
unit_dir="$root/etc/systemd/system"
target="$bin_dir/solodock"
update_target="$bin_dir/solodock-update"
backup_target="$bin_dir/solodock-backup"
restore_target="$bin_dir/solodock-restore"
unit_target="$unit_dir/solodock.service"

validate_public_target() {
  local candidate=$1
  local name=$2
  local current
  if [[ -e $candidate || -L $candidate ]]; then
    if [[ ! -L $candidate ]]; then
      printf 'refusing to replace unknown non-symlink %s target\n' "$name" >&2
      exit 1
    fi
    current=$(readlink -- "$candidate")
    if [[ ! $current =~ ^/usr/local/lib/solodock/[^/]+/$name$ && ! $current =~ ^/usr/local/lib/solodock/generations/[0-9A-Za-z._-]+\.[0-9a-f]{64}\.[0-9A-Za-z]{12}/$name$ ]]; then
      printf 'refusing unknown %s symlink target\n' "$name" >&2
      exit 1
    fi
  fi
}
validate_public_target "$target" solodock
validate_public_target "$update_target" solodock-update
validate_public_target "$backup_target" solodock-backup
validate_public_target "$restore_target" solodock-restore
if [[ -e $unit_target || -L $unit_target ]]; then
  if [[ -L $unit_target ]]; then
    unit_link=$(readlink -- "$unit_target")
    [[ $unit_link =~ ^/usr/local/lib/solodock/generations/[0-9A-Za-z._-]+\.[0-9a-f]{64}\.[0-9A-Za-z]{12}/solodock\.service$ ]] || { printf '%s\n' 'refusing unknown solodock.service symlink target' >&2; exit 1; }
  else
    [[ -f $unit_target ]] || { printf '%s\n' 'refusing unsafe existing systemd unit target' >&2; exit 1; }
  fi
fi
for directory in "$managed_root" "$bin_dir" "$etc_dir" "$state_dir" "$unit_dir"; do
  if [[ -e $directory || -L $directory ]]; then
    [[ -d $directory && ! -L $directory ]] || { printf 'refusing unsafe managed directory %s\n' "$directory" >&2; exit 1; }
  fi
done
if [[ -e $generations || -L $generations ]]; then
  [[ -d $generations && ! -L $generations ]] || { printf '%s\n' 'refusing unsafe generations directory' >&2; exit 1; }
fi

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
install -d -m 0755 -- "$managed_root" "$generations" "$bin_dir" "$unit_dir"
install -d -m 0700 -- "$etc_dir" "$state_dir"

transaction=''
generation=''
generation_link=''
committed=0
mutation_started=0
snapshot_manifest=''
snapshot_manifest_sha=''

declare -A snapshot_type snapshot_value
snapshot() {
  local name=$1
  local path=$2
  local allow_file=${3:-0}
  if [[ ${SOLODOCK_INSTALL_FAIL_AT:-} == "snapshot-$name" ]]; then
    printf 'injected installer failure at snapshot-%s\n' "$name" >&2
    return 1
  fi
  if [[ -L $path ]]; then
    snapshot_type[$name]=link
    snapshot_value[$name]=$(readlink -- "$path")
  elif [[ -e $path ]]; then
    [[ $allow_file == 1 && -f $path ]] || return 1
    snapshot_type[$name]=file
    snapshot_value[$name]=$(stat -c '%a:%u:%g' -- "$path")
    cp -a -- "$path" "$transaction/$name"
  else
    snapshot_type[$name]=absent
    snapshot_value[$name]=''
  fi
}
replace_link() {
  local destination=$1
  local link_target=$2
  local temporary="${destination}.tmp.$$"
  ln -s -- "$link_target" "$temporary"
  mv -Tf -- "$temporary" "$destination"
}
restore_snapshot() {
  local name=$1
  local path=$2
  local temporary="${path}.rollback.$$"
  if [[ ${SOLODOCK_INSTALL_ROLLBACK_FAIL_AT:-} == "restore-$name" ]]; then
    printf 'injected installer rollback failure at restore-%s\n' "$name" >&2
    return 1
  fi
  case ${snapshot_type[$name]} in
    link)
      rm -f -- "$temporary" || return 1
      ln -s -- "${snapshot_value[$name]}" "$temporary" || return 1
      mv -Tf -- "$temporary" "$path" || return 1
      ;;
    file)
      cp -a -- "$transaction/$name" "$temporary" || return 1
      mv -Tf -- "$temporary" "$path" || return 1
      ;;
    absent) rm -f -- "$path" || return 1 ;;
    *) return 1 ;;
  esac
  rm -f -- "$temporary" || return 1
}
verify_snapshot() {
  local name=$1
  local path=$2
  local restored_target
  case ${snapshot_type[$name]} in
    link)
      [[ -L $path && $(readlink -- "$path") == "${snapshot_value[$name]}" ]] || return 1
      restored_target="$root${snapshot_value[$name]}"
      [[ -f $restored_target && ! -L $restored_target ]]
      ;;
    file)
      [[ -f $path && ! -L $path ]] &&
        cmp -s -- "$transaction/$name" "$path" &&
        [[ $(stat -c '%a:%u:%g' -- "$path") == "${snapshot_value[$name]}" ]]
      ;;
    absent) [[ ! -e $path && ! -L $path ]] ;;
    *) return 1 ;;
  esac
}
rollback() {
  local status=$?
  local rollback_incomplete=0
  local entry name path
  trap - ERR EXIT
  set +e
  if ((!committed && mutation_started)); then
    for entry in \
      "solodock.service:$unit_target" \
      "solodock-restore:$restore_target" \
      "solodock-backup:$backup_target" \
      "solodock-update:$update_target" \
      "solodock:$target"; do
      name=${entry%%:*}
      path=${entry#*:}
      restore_snapshot "$name" "$path" || rollback_incomplete=1
    done
    for entry in \
      "solodock.service:$unit_target" \
      "solodock-restore:$restore_target" \
      "solodock-backup:$backup_target" \
      "solodock-update:$update_target" \
      "solodock:$target"; do
      name=${entry%%:*}
      path=${entry#*:}
      verify_snapshot "$name" "$path" || rollback_incomplete=1
    done
    if [[ -n $snapshot_manifest ]]; then
      [[ -f $snapshot_manifest && ! -L $snapshot_manifest && $(sha256sum "$snapshot_manifest" | awk '{print $1}') == "$snapshot_manifest_sha" ]] || rollback_incomplete=1
    fi
    if [[ -z $destdir ]] && command -v systemctl >/dev/null; then
      systemctl daemon-reload >/dev/null 2>&1 || rollback_incomplete=1
    fi
  fi
  if ((rollback_incomplete)); then
    printf '%s\n' 'ROLLBACK_INCOMPLETE: SoloDock installation entries could not be restored as one package; keep the service stopped and recover manually.' >&2
    printf 'Preserved installer scene: generation=%s transaction=%s\n' "${generation:-none}" "${transaction:-none}" >&2
    exit 70
  fi
  if [[ -n $generation ]]; then
    rm -rf -- "$generation" || rollback_incomplete=1
  fi
  if [[ -n $transaction ]]; then
    rm -rf -- "$transaction" || rollback_incomplete=1
  fi
  if ((rollback_incomplete)); then
    printf '%s\n' 'ROLLBACK_INCOMPLETE: restored entries passed verification, but installer scene cleanup failed; keep the service stopped and inspect the preserved paths.' >&2
    printf 'Preserved installer scene: generation=%s transaction=%s\n' "${generation:-none}" "${transaction:-none}" >&2
    exit 70
  fi
  ((status == 70)) && status=1
  exit "$status"
}

transaction=$(mktemp -d "$managed_root/.install-transaction.XXXXXXXXXXXX")
trap rollback ERR EXIT
chmod 0700 "$transaction"
generation=$(mktemp -d "$generations/$version.$package_identity.XXXXXXXXXXXX")
chmod 0755 "$generation"
generation_name=$(basename -- "$generation")
generation_link="/usr/local/lib/solodock/generations/$generation_name"

snapshot solodock "$target"
snapshot solodock-update "$update_target"
snapshot solodock-backup "$backup_target"
snapshot solodock-restore "$restore_target"
snapshot solodock.service "$unit_target" 1
if [[ ${snapshot_type[solodock]} == link ]]; then
  snapshot_binary="$root${snapshot_value[solodock]}"
  [[ -f $snapshot_binary && ! -L $snapshot_binary ]] || { printf '%s\n' 'existing managed binary target is unsafe' >&2; exit 1; }
  if [[ ${snapshot_value[solodock]} == /usr/local/lib/solodock/generations/*/solodock ]]; then
    snapshot_manifest="${snapshot_binary%/solodock}/INSTALL_MANIFEST"
    [[ -f $snapshot_manifest && ! -L $snapshot_manifest ]] || { printf '%s\n' 'existing managed generation manifest is unsafe' >&2; exit 1; }
    snapshot_manifest_sha=$(sha256sum "$snapshot_manifest" | awk '{print $1}')
  fi
fi
maybe_fail() {
  [[ ${SOLODOCK_INSTALL_FAIL_AT:-} != "$1" ]] || { printf 'injected installer failure at %s\n' "$1" >&2; return 1; }
}

install -m 0755 -- "$binary" "$generation/solodock"
maybe_fail stage-solodock
install -m 0755 -- "$update_source" "$generation/solodock-update"
maybe_fail stage-update
install -m 0755 -- "$backup_source" "$generation/solodock-backup"
maybe_fail stage-backup
install -m 0755 -- "$restore_source" "$generation/solodock-restore"
maybe_fail stage-restore
install -m 0755 -- "$verify_source" "$generation/verify-package.sh"
maybe_fail stage-verifier
install -m 0644 -- "$unit_source" "$generation/solodock.service"
maybe_fail stage-unit
install -m 0644 -- "$manifest_source" "$generation/INSTALL_MANIFEST"
maybe_fail stage-manifest

if [[ ! -e $etc_dir/config.toml ]]; then
  install -m 0600 -- "$config_source" "$etc_dir/config.toml"
elif [[ -L $etc_dir/config.toml || ! -f $etc_dir/config.toml ]]; then
  printf '%s\n' 'refusing unsafe existing config target' >&2
  exit 1
fi

# Helpers and the unit move first. The binary link is the installation identity
# commit marker; every earlier failure restores the complete old snapshot.
mutation_started=1
replace_link "$update_target" "$generation_link/solodock-update"
maybe_fail after-link-update
replace_link "$backup_target" "$generation_link/solodock-backup"
maybe_fail after-link-backup
replace_link "$restore_target" "$generation_link/solodock-restore"
maybe_fail after-link-restore
replace_link "$unit_target" "$generation_link/solodock.service"
maybe_fail after-link-unit
replace_link "$target" "$generation_link/solodock"
maybe_fail after-link-solodock

if [[ -z $destdir ]]; then
  chown solodock:solodock /etc/solodock /etc/solodock/config.toml /var/lib/solodock
  maybe_fail daemon-reload
  systemctl daemon-reload
fi
committed=1
trap - ERR EXIT
rm -rf -- "$transaction" || true

if [[ -z $destdir ]] && ((enable_now)); then systemctl enable --now solodock.service; fi

printf '%s\n' 'Keep a verified offline backup before upgrading; database migrations are forward-only.'
printf '%s\n' "SoloDock $version installed; existing config and state were preserved."
