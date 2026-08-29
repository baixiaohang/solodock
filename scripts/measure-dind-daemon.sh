#!/usr/bin/env bash
set -euo pipefail

container=${1:?usage: measure-dind-daemon.sh CONTAINER OUTPUT_JSON -- COMMAND...}
output=${2:?usage: measure-dind-daemon.sh CONTAINER OUTPUT_JSON -- COMMAND...}
shift 2
[[ ${1:-} == -- && $output = /* ]] || {
  printf '%s\n' 'expected an absolute output path and -- before the command' >&2
  exit 2
}
shift
(( $# > 0 )) || { printf '%s\n' 'missing measured command' >&2; exit 2; }

outer_host=${SOLODOCK_OUTER_DOCKER_HOST:-unix:///var/run/docker.sock}
DOCKER_HOST=$outer_host docker inspect "$container" >/dev/null
peak_rss_kib=0
sample_count=0
stop_file=$(mktemp)
rm -f -- "$stop_file"
cleanup() {
  : >"$stop_file"
  wait "${sampler_pid:-}" 2>/dev/null || true
  rm -f -- "$stop_file"
}
trap cleanup EXIT INT TERM
(
  while [[ ! -e $stop_file ]]; do
    rss=$(DOCKER_HOST=$outer_host docker exec "$container" sh -c '
      for status in /proc/[0-9]*/status; do
        [ "$(sed -n "s/^Name:[[:space:]]*//p" "$status")" = dockerd ] || continue
        sed -n "s/^VmRSS:[[:space:]]*\([0-9][0-9]*\).*/\1/p" "$status"
        break
      done
    ' 2>/dev/null || true)
    if [[ $rss =~ ^[0-9]+$ ]]; then
      printf '%s\n' "$rss"
    fi
    sleep 0.2
  done
) >"$output.samples" &
sampler_pid=$!

"$@"
: >"$stop_file"
wait "$sampler_pid" || true
sampler_pid=''
if [[ -s $output.samples ]]; then
  peak_rss_kib=$(sort -n "$output.samples" | tail -1)
  sample_count=$(wc -l <"$output.samples")
fi
rm -f -- "$output.samples"
mkdir -p -- "$(dirname -- "$output")"
printf '{"container":"%s","process":"dockerd","peak_rss_kib":%s,"sample_count":%s,"sample_interval_ms":200}\n' \
  "$container" "$peak_rss_kib" "$sample_count" >"$output"
(( sample_count > 0 && peak_rss_kib > 0 )) || {
  printf '%s\n' 'Docker daemon sampler produced no valid observations' >&2
  exit 1
}
