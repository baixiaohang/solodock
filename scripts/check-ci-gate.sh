#!/usr/bin/env bash
set -euo pipefail

if (( $# != 4 )); then
  echo "usage: $0 <run-core> <run-docker-e2e> <event-name> <needs-json>" >&2
  exit 2
fi

run_core=$1
run_docker_e2e=$2
event_name=$3
needs_json=$4

require_boolean() {
  local name=$1
  local value=$2
  if [[ $value != true && $value != false ]]; then
    echo "invalid classify output: $name=$value" >&2
    exit 1
  fi
}

result_for() {
  local job=$1
  jq -er --arg job "$job" '.[$job].result' <<<"$needs_json" 2>/dev/null || printf '%s\n' missing
}

require_result() {
  local job=$1
  local expected=$2
  local actual
  actual=$(result_for "$job")
  if [[ $actual != "$expected" ]]; then
    echo "unexpected CI result: $job=$actual (expected $expected)" >&2
    failed=true
  fi
}

require_boolean run_core "$run_core"
require_boolean run_docker_e2e "$run_docker_e2e"
if [[ $event_name != pull_request && $event_name != push ]]; then
  echo "invalid workflow event: $event_name" >&2
  exit 1
fi

failed=false
require_result classify success
require_result security-policy success

core_expected=skipped
if [[ $run_core == true ]]; then
  core_expected=success
fi
for job in web rust-lint rust-test package-smoke; do
  require_result "$job" "$core_expected"
done

attestation_expected=skipped
if [[ $event_name == push ]]; then
  attestation_expected=success
fi
require_result attest-package "$attestation_expected"

docker_expected=skipped
if [[ $run_docker_e2e == true ]]; then
  docker_expected=success
fi
require_result docker-e2e "$docker_expected"
require_result docker-containerd-e2e "$docker_expected"

if [[ $failed == true ]]; then
  exit 1
fi

echo "all required CI jobs matched the classified execution plan"
