#!/usr/bin/env bash
set -euo pipefail
binary=${1:?usage: measure-resources.sh BINARY OUTPUT_JSON}
output=${2:?usage: measure-resources.sh BINARY OUTPUT_JSON}
[[ -x $binary && $output = /* ]] || { printf '%s\n' 'invalid binary or output path' >&2; exit 2; }
fixture=$(mktemp -d)
pid=''
cleanup() { if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then kill -TERM "$pid"; wait "$pid" || true; fi; rm -rf -- "$fixture"; }
trap cleanup EXIT
chmod 0700 "$fixture"
cat >"$fixture/config.toml" <<EOF
schema_version = 1
listen_address = "127.0.0.1:0"
public_origin = "https://solodock.example.invalid"
state_directory = "$fixture/state"
runtime_directory = "$fixture/run"
allowed_bind_roots = []
EOF
chmod 0600 "$fixture/config.toml"
SOLODOCK_CONFIG_PATH="$fixture/config.toml" "$binary" >"$fixture/stdout" 2>"$fixture/stderr" & pid=$!
for _ in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || { printf '%s\n' 'measurement process exited early' >&2; exit 1; }; [[ -r /proc/$pid/status ]] && break; sleep 0.1; done
warmup_seconds=${SOLODOCK_MEASURE_WARMUP_SECONDS:-60}
sample_seconds=${SOLODOCK_MEASURE_SAMPLE_SECONDS:-60}
[[ $warmup_seconds =~ ^[1-9][0-9]*$ && $sample_seconds =~ ^[1-9][0-9]*$ ]] || { printf '%s\n' 'measurement durations must be positive integers' >&2; exit 2; }
sleep "$warmup_seconds"
ticks_before=$(awk '{print $14+$15}' "/proc/$pid/stat")
sleep "$sample_seconds"
rss_kib=$(awk '/^VmRSS:/ {print $2}' "/proc/$pid/status")
fd_count=$(find "/proc/$pid/fd" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l)
task_count=$(find "/proc/$pid/task" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l)
ticks_after=$(awk '{print $14+$15}' "/proc/$pid/stat")
ticks=$((ticks_after - ticks_before))
clock_ticks=$(getconf CLK_TCK)
cpu_percent=$(awk -v ticks="$ticks" -v hz="$clock_ticks" -v seconds="$sample_seconds" 'BEGIN { printf "%.4f", (ticks / hz / seconds) * 100 }')
size_bytes=$(stat -c '%s' "$binary")
repository=$(cd -- "$(dirname -- "$0")/.." && pwd)
commit=$(git -C "$repository" rev-parse HEAD 2>/dev/null || printf unknown)
cgroup_path=$(awk -F: '$1 == "0" {print $3}' "/proc/$pid/cgroup")
cgroup_root="/sys/fs/cgroup$cgroup_path"
controller_value() {
  local current=$cgroup_root
  local name=$1
  while [[ $current == /sys/fs/cgroup/* || $current == /sys/fs/cgroup ]]; do
    if [[ -r $current/$name ]]; then
      tr '\n' ' ' <"$current/$name" | sed 's/[[:space:]]*$//'
      return
    fi
    [[ $current != /sys/fs/cgroup ]] || break
    current=$(dirname -- "$current")
  done
  printf unavailable
}
cpu_max=$(controller_value cpu.max)
memory_max=$(controller_value memory.max)
tool_version() {
  local tool=$1
  if command -v "$tool" >/dev/null 2>&1; then
    local version
    version=$("$tool" --version 2>/dev/null | tr -d '"' || true)
    if [[ -n $version ]]; then
      printf '%s' "$version"
    else
      printf unavailable
    fi
  else
    printf unavailable
  fi
}
environment='local/CI measurement; not Tencent Cloud 2C4G validation'
if [[ $cpu_max == '200000 100000' && $memory_max == '4294967296' ]]; then
  environment='isolated cgroup equivalent: 2 vCPU / 4 GiB; not a Tencent Cloud host'
fi
mkdir -p -- "$(dirname -- "$output")"
printf '{"commit":"%s","kernel":"%s","rust":"%s","node":"%s","cgroup_cpu_max":"%s","cgroup_memory_max":"%s","warmup_seconds":%s,"sample_seconds":%s,"idle_rss_kib":%s,"idle_cpu_ticks":%s,"idle_cpu_percent":%s,"fd_count":%s,"task_count":%s,"binary_bytes":%s,"environment":"%s"}\n' \
  "$commit" "$(uname -sr | tr -d '"')" "$(tool_version rustc)" "$(tool_version node)" "$cpu_max" "$memory_max" "$warmup_seconds" "$sample_seconds" "$rss_kib" "$ticks" "$cpu_percent" "$fd_count" "$task_count" "$size_bytes" "$environment" >"$output"
((rss_kib < 524288)) || { printf '%s\n' 'idle RSS exceeded CI regression ceiling' >&2; exit 1; }
